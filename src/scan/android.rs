//! Android virtual devices and system images.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::fsutil::days_since;
use crate::model::{Item, Removal, SourceId, Status};
use crate::platform::Os;
use crate::scan::{ScanCtx, ScanEvent, Scanner, Sender, size_later};

pub struct Android {
    pub sdk: Option<PathBuf>,
    pub avd_home: PathBuf,
}

/// Where AVDs live: `$ANDROID_AVD_HOME`, else `avd/` under
/// `$ANDROID_USER_HOME` or `$ANDROID_EMULATOR_HOME`, else `.android/avd`
/// under `$ANDROID_SDK_HOME` or the home folder.
pub fn avd_home_from(env: &dyn Fn(&str) -> Option<std::ffi::OsString>, home: &Path) -> PathBuf {
    let from = |key: &str, rel: &str| {
        env(key).filter(|v| !v.is_empty()).map(|v| {
            let base = PathBuf::from(v);
            if rel.is_empty() { base } else { base.join(rel) }
        })
    };
    from("ANDROID_AVD_HOME", "")
        .or_else(|| from("ANDROID_USER_HOME", "avd"))
        .or_else(|| from("ANDROID_EMULATOR_HOME", "avd"))
        .or_else(|| from("ANDROID_SDK_HOME", ".android/avd"))
        .unwrap_or_else(|| home.join(".android/avd"))
}

impl Android {
    /// Locate the SDK (`$ANDROID_HOME`, `$ANDROID_SDK_ROOT`, the Android
    /// Studio default) and the AVD home (`$ANDROID_AVD_HOME`, `~/.android/avd`).
    pub fn detect(home: &Path, os: Os) -> Android {
        let env = &crate::platform::process_env;
        let sdk = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
            .iter()
            .filter_map(|key| env(key))
            .map(PathBuf::from)
            .chain(os.android_sdk_default(home, env))
            .find(|p| p.is_dir());
        let avd_home = avd_home_from(env, home);
        Android { sdk, avd_home }
    }

    fn scan_avds(&self, ctx: &ScanCtx, tx: &Sender<ScanEvent>) -> anyhow::Result<()> {
        let avdmanager = self
            .sdk
            .as_ref()
            .map(|sdk| {
                sdk.join("cmdline-tools/latest/bin")
                    .join(ctx.os.script_name("avdmanager"))
            })
            .filter(|p| p.is_file());
        let mut referenced = HashSet::new();
        let avds = self.avds();

        for (name, dir) in avds {
            if let Some(sysdir) = read_ini(&dir.join("config.ini"), "image.sysdir.1") {
                // Windows writes this path with backslashes.
                let sysdir = sysdir.replace('\\', "/");
                referenced.insert(sysdir.trim_end_matches('/').to_string());
            }
            // The emulator names its AVD with `-avd NAME` or `@NAME`, on
            // every system. A snapshot that failed says so instead.
            let named = vec![format!("-avd {name}"), format!("@{name}")];
            let age = std::fs::metadata(&dir)
                .and_then(|m| m.modified())
                .ok()
                .map(days_since);
            let mut lock = ctx.inuse.failure().or_else(|| {
                ctx.inuse
                    .args_containing(&named)
                    .map(|_| "emulator running".to_string())
            });
            let removal = match &avdmanager {
                Some(bin) => Removal::Command {
                    argv: vec![
                        bin.display().to_string(),
                        "delete".into(),
                        "avd".into(),
                        "-n".into(),
                        name.clone(),
                    ],
                    cwd: None,
                },
                None => {
                    // Without avdmanager we delete the files ourselves, so the
                    // folder named by the .ini must be inside the AVD home.
                    if !within(&dir, &self.avd_home) {
                        lock.get_or_insert_with(|| "AVD folder outside the AVD home".into());
                    }
                    if !removable(ctx, &dir) {
                        lock.get_or_insert_with(|| OUTSIDE.into());
                    }
                    Removal::RemovePaths(vec![
                        dir.clone(),
                        self.avd_home.join(format!("{name}.ini")),
                    ])
                }
            };
            let item = Item {
                id: ctx.next_id(),
                source: SourceId::Android,
                label: name.clone(),
                path: Some(dir.clone()),
                size: None,
                status: age.map(Status::LastUsed).into_iter().collect(),
                lock,
                safe: false,
                removal,
                age_days: age,
                recheck: crate::model::Recheck {
                    args: named,
                    ..Default::default()
                },
            };
            emit(tx, item);
        }

        if let Some(sdk) = &self.sdk {
            for rel in system_images(sdk) {
                if referenced.contains(&rel) {
                    continue;
                }
                let path = sdk.join(&rel);
                let label = rel
                    .trim_start_matches("system-images/")
                    .split('/')
                    .collect::<Vec<_>>()
                    .join(" · ");
                let item = Item {
                    id: ctx.next_id(),
                    source: SourceId::Android,
                    label,
                    path: Some(path.clone()),
                    size: None,
                    status: vec![Status::Orphan],
                    lock: (!removable(ctx, &path)).then(|| OUTSIDE.into()),
                    // "Unused" is a guess from config.ini; downloading again costs GBs.
                    safe: false,
                    removal: Removal::RemoveDir(path),
                    age_days: None,
                    recheck: crate::model::Recheck::default(),
                };
                emit(tx, item);
            }
        }
        Ok(())
    }

    /// `(name, <name>.avd dir)` for every AVD registered in the AVD home.
    fn avds(&self) -> Vec<(String, PathBuf)> {
        let Ok(entries) = std::fs::read_dir(&self.avd_home) else {
            return vec![];
        };
        let mut avds: Vec<_> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                let name = path
                    .file_name()?
                    .to_str()?
                    .strip_suffix(".ini")?
                    .to_string();
                let dir = read_ini(&path, "path")
                    .map_or_else(|| self.avd_home.join(format!("{name}.avd")), PathBuf::from);
                dir.is_dir().then_some((name, dir))
            })
            .collect();
        avds.sort();
        avds
    }
}

fn emit(tx: &Sender<ScanEvent>, item: Item) {
    let (path, id) = (item.path.clone(), item.id);
    let _ = tx.send(ScanEvent::Found(item));
    if let Some(path) = path {
        size_later(path, id, tx.clone());
    }
}

/// Why a folder devsweep would have to delete itself is not offered.
const OUTSIDE: &str = "outside the home folder";

/// Whether the guard would let `path` be deleted: it sits under the home
/// folder, or under the scanned one unless that holds the home folder. An
/// SDK or an AVD home kept elsewhere is shown, never offered and refused.
fn removable(ctx: &ScanCtx, path: &Path) -> bool {
    within(path, &ctx.home) || (within(path, &ctx.target) && !within(&ctx.home, &ctx.target))
}

/// `path` is `root` or below it, comparing canonical paths.
fn within(path: &Path, root: &Path) -> bool {
    match (std::fs::canonicalize(path), std::fs::canonicalize(root)) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => false,
    }
}

fn read_ini(path: &Path, key: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()?.lines().find_map(|l| {
        l.strip_prefix(key)?
            .trim_start()
            .strip_prefix('=')
            .map(|v| v.trim().to_string())
    })
}

/// `system-images/<api>/<tag>/<abi>` paths relative to the SDK.
fn system_images(sdk: &Path) -> Vec<String> {
    let base = sdk.join("system-images");
    let mut out = Vec::new();
    for api in read_dirs(&base) {
        for tag in read_dirs(&api) {
            for abi in read_dirs(&tag) {
                if let Ok(rel) = abi.strip_prefix(sdk) {
                    // Always with `/`, the way an AVD names its image.
                    let parts: Vec<_> = rel
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect();
                    out.push(parts.join("/"));
                }
            }
        }
    }
    out.sort();
    out
}

fn read_dirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

impl Scanner for Android {
    fn source(&self) -> SourceId {
        SourceId::Android
    }

    fn available(&self) -> bool {
        self.sdk.is_some() || self.avd_home.is_dir()
    }

    fn scan(&self, ctx: &ScanCtx, tx: &Sender<ScanEvent>) -> anyhow::Result<()> {
        self.scan_avds(ctx, tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inuse::InUse;
    use crate::model::{Removal, Status};
    use std::fs;
    use std::path::{Path, PathBuf};

    fn make_avd(avd_home: &Path, name: &str, sysdir: &str) {
        let dir = avd_home.join(format!("{name}.avd"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("config.ini"),
            format!("AvdId={name}\nimage.sysdir.1={sysdir}\n"),
        )
        .unwrap();
        fs::write(dir.join("userdata.img"), vec![0u8; 4096]).unwrap();
        fs::write(
            avd_home.join(format!("{name}.ini")),
            format!("path={}\n", dir.display()),
        )
        .unwrap();
    }

    fn make_image(sdk: &Path, rel: &str) {
        fs::create_dir_all(sdk.join(rel)).unwrap();
        fs::write(sdk.join(rel).join("system.img"), vec![0u8; 4096]).unwrap();
    }

    /// A process snapshot holding the given `ps` lines (`PID command line`).
    fn processes(ps: &str) -> InUse {
        let args = InUse::parse_ps(ps);
        let lsof: String = args
            .keys()
            .map(|pid| format!("p{pid}\ncqemu-system-aarch64\nn/\n"))
            .collect();
        InUse::parse(&lsof).with_args(&args)
    }

    fn scan(android: &Android, ps: &str) -> Vec<Item> {
        scan_in(android, processes(ps))
    }

    fn scan_in(android: &Android, inuse: InUse) -> Vec<Item> {
        // The folder `setup` puts the SDK and the AVD home in.
        let home = android.avd_home.parent().unwrap().to_path_buf();
        scan_at(android, inuse, home)
    }

    /// Scan for a user whose home folder, also the scanned one, is `home`.
    fn scan_at(android: &Android, inuse: InUse, home: PathBuf) -> Vec<Item> {
        // As on macOS: `avdmanager` has no extension there.
        let ctx = ScanCtx::new(home.clone(), home, inuse).with_os(Os::MacOs);
        let (tx, rx) = crossbeam_channel::unbounded();
        android.scan(&ctx, &tx).unwrap();
        drop(tx);
        rx.iter()
            .filter_map(|e| {
                if let ScanEvent::Found(i) = e {
                    Some(i)
                } else {
                    None
                }
            })
            .collect()
    }

    fn setup() -> (tempfile::TempDir, Android) {
        let d = tempfile::tempdir().unwrap();
        let sdk = d.path().join("sdk");
        let avd_home = d.path().join("avd");
        make_avd(
            &avd_home,
            "Pixel_8_API_34",
            "system-images/android-34/google_apis/arm64-v8a/",
        );
        make_image(&sdk, "system-images/android-34/google_apis/arm64-v8a");
        make_image(&sdk, "system-images/android-30/default/arm64-v8a");
        (
            d,
            Android {
                sdk: Some(sdk),
                avd_home,
            },
        )
    }

    #[test]
    fn lists_avds_from_ini_files() {
        let (_d, a) = setup();
        let items = scan(&a, "");
        let avd = items.iter().find(|i| i.label == "Pixel_8_API_34").unwrap();
        assert!(!avd.safe);
        assert!(avd.lock.is_none());
    }

    #[test]
    fn running_emulator_locks_avd() {
        let (_d, a) = setup();
        let items = scan(
            &a,
            "812 /sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64 -avd Pixel_8_API_34 -netdelay none\n",
        );
        let avd = items.iter().find(|i| i.label == "Pixel_8_API_34").unwrap();
        assert_eq!(avd.lock.as_deref(), Some("emulator running"));
    }

    /// The emulator binary differs per system; what names the AVD does not.
    #[test]
    fn running_emulator_locks_avd_on_any_platform() {
        let (_d, a) = setup();
        for ps in [
            "812 /home/u/Android/Sdk/emulator/qemu/linux-x86_64/qemu-system-x86_64 -avd Pixel_8_API_34\n",
            "812 C:\\Users\\u\\AppData\\Local\\Android\\Sdk\\emulator\\emulator.exe @Pixel_8_API_34\n",
        ] {
            let items = scan(&a, ps);
            let avd = items.iter().find(|i| i.label == "Pixel_8_API_34").unwrap();
            assert_eq!(avd.lock.as_deref(), Some("emulator running"), "{ps}");
        }
    }

    #[test]
    fn another_avd_running_does_not_lock_this_one() {
        let (_d, a) = setup();
        let items = scan(&a, "812 /sdk/emulator/qemu-system-aarch64 -avd Pixel_9\n");
        let avd = items.iter().find(|i| i.label == "Pixel_8_API_34").unwrap();
        assert!(avd.lock.is_none());
    }

    /// Without a process snapshot nobody knows whether the emulator runs.
    #[test]
    fn failed_process_snapshot_locks_every_avd() {
        let (_d, a) = setup();
        let items = scan_in(&a, InUse::failed("lsof missing"));
        let avd = items.iter().find(|i| i.label == "Pixel_8_API_34").unwrap();
        assert_eq!(
            avd.lock.as_deref(),
            Some("process check failed: lsof missing")
        );
    }

    #[test]
    fn avd_is_rechecked_for_an_emulator_started_after_the_scan() {
        let (_d, a) = setup();
        let items = scan(&a, "");
        let avd = items.iter().find(|i| i.label == "Pixel_8_API_34").unwrap();
        assert_eq!(
            avd.recheck.args,
            vec![
                "-avd Pixel_8_API_34".to_string(),
                "@Pixel_8_API_34".to_string()
            ]
        );
    }

    #[test]
    fn avd_removal_uses_avdmanager_when_present() {
        let (d, a) = setup();
        let bin = d.path().join("sdk/cmdline-tools/latest/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("avdmanager"), "").unwrap();
        let items = scan(&a, "");
        let avd = items.iter().find(|i| i.label == "Pixel_8_API_34").unwrap();
        let Removal::Command { argv, cwd: None } = &avd.removal else {
            panic!("{:?}", avd.removal);
        };
        // Compared as a path: the separators may differ in spelling.
        assert_eq!(Path::new(&argv[0]), bin.join("avdmanager"));
        assert_eq!(argv[1..], ["delete", "avd", "-n", "Pixel_8_API_34"]);
    }

    #[test]
    fn avd_removal_falls_back_to_guarded_removal_of_dir_and_ini() {
        let (d, a) = setup();
        let items = scan(&a, "");
        let avd = items.iter().find(|i| i.label == "Pixel_8_API_34").unwrap();
        let avd_home = d.path().join("avd");
        assert_eq!(
            avd.removal,
            Removal::RemovePaths(vec![
                avd_home.join("Pixel_8_API_34.avd"),
                avd_home.join("Pixel_8_API_34.ini"),
            ])
        );
    }

    #[test]
    fn avd_folder_outside_avd_home_is_locked_without_avdmanager() {
        let (d, a) = setup();
        let elsewhere = d.path().join("Documents/work");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(
            a.avd_home.join("Pixel_8_API_34.ini"),
            format!("path={}\n", elsewhere.display()),
        )
        .unwrap();
        let items = scan(&a, "");
        let avd = items.iter().find(|i| i.label == "Pixel_8_API_34").unwrap();
        assert!(avd.lock.is_some());
        assert!(!avd.selectable());
    }

    /// On Windows the AVD names its system image with backslashes. An image
    /// in use must not show up as an orphan because of how its path is spelled.
    #[test]
    fn windows_style_sysdir_marks_the_image_as_referenced() {
        let d = tempfile::tempdir().unwrap();
        let (sdk, avd_home) = (d.path().join("sdk"), d.path().join("avd"));
        make_avd(
            &avd_home,
            "Pixel_8_API_34",
            "system-images\\android-34\\google_apis\\arm64-v8a\\",
        );
        make_image(&sdk, "system-images/android-34/google_apis/arm64-v8a");
        let a = Android {
            sdk: Some(sdk),
            avd_home,
        };
        let labels: Vec<String> = scan(&a, "").into_iter().map(|i| i.label).collect();
        assert_eq!(labels, ["Pixel_8_API_34"]);
    }

    /// The SDK ships `avdmanager` as a batch file on Windows.
    #[test]
    fn avd_removal_uses_the_batch_file_on_windows() {
        let (d, a) = setup();
        let bin = d.path().join("sdk/cmdline-tools/latest/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("avdmanager.bat"), "").unwrap();
        let ctx = ScanCtx::new("/tmp".into(), "/tmp".into(), InUse::default()).with_os(Os::Windows);
        let (tx, rx) = crossbeam_channel::unbounded();
        a.scan(&ctx, &tx).unwrap();
        drop(tx);
        let avd = rx
            .iter()
            .find_map(|e| match e {
                ScanEvent::Found(i) if i.label == "Pixel_8_API_34" => Some(i),
                _ => None,
            })
            .unwrap();
        let Removal::Command { argv, .. } = &avd.removal else {
            panic!("{:?}", avd.removal);
        };
        assert!(argv[0].ends_with("avdmanager.bat"), "{}", argv[0]);
    }

    #[test]
    fn orphan_system_image_is_listed_but_never_safe() {
        let (d, a) = setup();
        let items = scan(&a, "");
        let img = items
            .iter()
            .find(|i| i.label == "android-30 · default · arm64-v8a")
            .unwrap();
        assert!(img.status.contains(&Status::Orphan));
        assert!(!img.safe);
        assert_eq!(
            img.removal,
            Removal::RemoveDir(
                d.path()
                    .join("sdk/system-images/android-30/default/arm64-v8a")
            )
        );
    }

    /// The guard only removes folders under the home folder or the scanned
    /// one: an SDK kept elsewhere must not be offered and then refused.
    #[test]
    fn system_image_of_an_sdk_outside_the_home_folder_is_locked() {
        let (d, a) = setup();
        let home = d.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let items = scan_at(&a, processes(""), home);
        let img = items
            .iter()
            .find(|i| i.label == "android-30 · default · arm64-v8a")
            .unwrap();
        assert_eq!(img.lock.as_deref(), Some("outside the home folder"));
    }

    /// The same goes for an AVD removed without avdmanager.
    #[test]
    fn avd_outside_the_home_folder_is_locked_without_avdmanager() {
        let (d, a) = setup();
        let home = d.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let items = scan_at(&a, processes(""), home);
        let avd = items.iter().find(|i| i.label == "Pixel_8_API_34").unwrap();
        assert_eq!(avd.lock.as_deref(), Some("outside the home folder"));
    }

    #[test]
    fn images_are_not_safe_when_no_avd_was_found() {
        let d = tempfile::tempdir().unwrap();
        let sdk = d.path().join("sdk");
        make_image(&sdk, "system-images/android-34/google_apis/arm64-v8a");
        let a = Android {
            sdk: Some(sdk),
            avd_home: d.path().join("nowhere"),
        };
        let items = scan(&a, "");
        assert_eq!(items.len(), 1);
        assert!(!items[0].safe);
    }

    #[test]
    fn avd_home_honours_android_env_vars() {
        let home = std::path::Path::new("/Users/u");
        let env =
            |k: &str| (k == "ANDROID_USER_HOME").then(|| std::ffi::OsString::from("/data/android"));
        assert_eq!(
            avd_home_from(&env, home),
            PathBuf::from("/data/android/avd")
        );
        let env = |k: &str| (k == "ANDROID_SDK_HOME").then(|| std::ffi::OsString::from("/sdkhome"));
        assert_eq!(
            avd_home_from(&env, home),
            PathBuf::from("/sdkhome/.android/avd")
        );
        assert_eq!(
            avd_home_from(&|_: &str| None, home),
            PathBuf::from("/Users/u/.android/avd")
        );
    }

    #[test]
    fn referenced_system_image_is_not_listed() {
        let (_d, a) = setup();
        assert!(
            !scan(&a, "")
                .iter()
                .any(|i| i.label.starts_with("android-34"))
        );
    }
}
