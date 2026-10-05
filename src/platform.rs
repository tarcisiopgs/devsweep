//! What differs between macOS, Linux and Windows, as plain functions of
//! [`Os`]. Taking the system as a value keeps every platform's rules
//! testable on any of them.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::model::SourceId;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    MacOs,
    Linux,
    Windows,
}

/// Environment lookup, injectable in tests.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// The real environment of this process.
pub fn process_env(key: &str) -> Option<OsString> {
    std::env::var_os(key)
}

impl Os {
    pub const ALL: [Os; 3] = [Os::MacOs, Os::Linux, Os::Windows];

    /// The system this binary was built for.
    pub fn current() -> Os {
        if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        }
    }

    /// Whether the source exists on this system at all.
    pub fn has_source(self, source: SourceId) -> bool {
        match source {
            SourceId::Ios | SourceId::Xcode | SourceId::Homebrew => self == Os::MacOs,
            _ => true,
        }
    }

    /// The user's home folder, from the variable this system sets.
    pub fn home_dir(self, env: Env) -> Option<PathBuf> {
        let key = match self {
            Os::MacOs | Os::Linux => "HOME",
            Os::Windows => "USERPROFILE",
        };
        env(key).filter(|v| !v.is_empty()).map(PathBuf::from)
    }

    /// Folders the guard never removes: the home folder and the ones that
    /// hold every tool's data on this system.
    pub fn protected_dirs(self, home: &Path, env: Env) -> Vec<PathBuf> {
        let (inside, overrides): (&[&str], &[&str]) = match self {
            Os::MacOs => (
                &[
                    "Library",
                    "Library/Caches",
                    "Library/Developer",
                    "Library/Logs",
                ],
                &[],
            ),
            Os::Linux => (
                &[
                    ".cache",
                    ".config",
                    ".local",
                    ".local/share",
                    ".local/state",
                ],
                &[
                    "XDG_CACHE_HOME",
                    "XDG_CONFIG_HOME",
                    "XDG_DATA_HOME",
                    "XDG_STATE_HOME",
                ],
            ),
            Os::Windows => (
                &[
                    "AppData",
                    "AppData/Local",
                    "AppData/LocalLow",
                    "AppData/Roaming",
                    "AppData/Local/Temp",
                ],
                &["LOCALAPPDATA", "APPDATA"],
            ),
        };
        std::iter::once(home.to_path_buf())
            .chain(inside.iter().map(|rel| home.join(rel)))
            .chain(
                overrides
                    .iter()
                    .filter_map(|key| env(key))
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from),
            )
            .collect()
    }

    /// Roots outside the home folder where removals are still allowed: the
    /// per-user cache folder, when the system keeps it elsewhere. A value
    /// that is relative, or that is the home folder or above it, would
    /// widen the guard instead and is dropped.
    pub fn guard_extra_roots(
        self,
        home: &Path,
        env: Env,
        darwin_cache: Option<PathBuf>,
    ) -> Vec<PathBuf> {
        let candidate = match self {
            Os::MacOs => darwin_cache,
            Os::Linux => env("XDG_CACHE_HOME").map(PathBuf::from),
            Os::Windows => None,
        };
        candidate
            .filter(|p| p.is_absolute() && !home.starts_with(p))
            .into_iter()
            .collect()
    }

    /// False when `path` sits on a volume that is not there right now: its
    /// absence then says nothing about what it holds. `mounts` are the
    /// current mount points ([`mount_points`]) and `exists` tells whether a
    /// volume root is present.
    pub fn volume_mounted(
        self,
        path: &Path,
        mounts: &[PathBuf],
        exists: &dyn Fn(&Path) -> bool,
    ) -> bool {
        match self {
            Os::MacOs => {
                let mut parts = path.components();
                match (parts.next(), parts.next(), parts.next()) {
                    (Some(std::path::Component::RootDir), Some(v), Some(name))
                        if v.as_os_str() == "Volumes" =>
                    {
                        exists(&Path::new("/Volumes").join(name))
                    }
                    _ => true,
                }
            }
            Os::Linux => {
                // Where removable and extra disks get mounted. An empty
                // folder left there by an unmounted disk still exists, so
                // only the mount table can tell.
                const REMOVABLE: [&str; 3] = ["/mnt", "/media", "/run/media"];
                let removable = |p: &Path| REMOVABLE.iter().any(|root| p.starts_with(root));
                !removable(path) || mounts.iter().any(|m| removable(m) && path.starts_with(m))
            }
            Os::Windows => windows_volume_root(path).is_some_and(|root| exists(&root)),
        }
    }

    /// Where Android Studio installs the SDK when no variable says otherwise.
    pub fn android_sdk_default(self, home: &Path, env: Env) -> Option<PathBuf> {
        match self {
            Os::MacOs => Some(home.join("Library/Android/sdk")),
            Os::Linux => Some(home.join("Android/Sdk")),
            Os::Windows => env("LOCALAPPDATA")
                .filter(|v| !v.is_empty())
                .map(|local| PathBuf::from(local).join("Android/Sdk")),
        }
    }

    /// File names a command may have on disk. Windows finds `npm` as
    /// `npm.cmd` and `git` as `git.exe`: one name per extension in
    /// `PATHEXT`, unless the command already carries its own.
    pub fn exe_names(self, bin: &str, pathext: Option<&str>) -> Vec<String> {
        const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";
        match self {
            Os::MacOs | Os::Linux => vec![bin.to_string()],
            Os::Windows if Path::new(bin).extension().is_some() => vec![bin.to_string()],
            Os::Windows => {
                let extensions = pathext.filter(|list| !list.trim().is_empty());
                extensions
                    .unwrap_or(DEFAULT_PATHEXT)
                    .split(';')
                    .filter(|ext| !ext.is_empty())
                    .map(|ext| format!("{bin}{ext}"))
                    .collect()
            }
        }
    }

    /// File name of a tool an SDK ships as a script: a batch file on Windows.
    pub fn script_name(self, base: &str) -> String {
        match self {
            Os::Windows => format!("{base}.bat"),
            Os::MacOs | Os::Linux => base.to_string(),
        }
    }

    /// Folders directly inside the home folder that hold tools and their
    /// data, not projects: neither the repository walk nor the artifact walk
    /// enters them.
    pub fn skip_at_home(self) -> &'static [&'static str] {
        match self {
            Os::MacOs => &["Library", ".Trash"],
            Os::Linux => &[".cache"],
            Os::Windows => &["AppData", "scoop"],
        }
    }
}

/// The first file in `dirs` carrying one of `names` ([`Os::exe_names`]).
pub fn find_executable(
    dirs: impl IntoIterator<Item = PathBuf>,
    names: &[String],
) -> Option<PathBuf> {
    dirs.into_iter()
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

/// A path as git prints it. On Unix a path is its bytes; elsewhere git
/// prints UTF-8, and bytes that are not UTF-8 name no path at all.
#[cfg(unix)]
pub fn path_from_git_bytes(bytes: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
pub fn path_from_git_bytes(bytes: &[u8]) -> Option<PathBuf> {
    utf8_path(bytes)
}

#[cfg(any(not(unix), test))]
fn utf8_path(bytes: &[u8]) -> Option<PathBuf> {
    std::str::from_utf8(bytes).ok().map(PathBuf::from)
}

/// `path` with symlinks resolved. On Windows the plain form (`C:\…`), not
/// the verbatim one (`\\?\C:\…`) that git and people do not expect.
pub fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    #[cfg(windows)]
    {
        dunce::canonicalize(path)
    }
    #[cfg(not(windows))]
    {
        std::fs::canonicalize(path)
    }
}

/// The drive (`D:\`) or network share (`\\server\share\`) a Windows path
/// lives on, read from its text so the rule holds on any system. git writes
/// these paths with forward slashes.
fn windows_volume_root(path: &Path) -> Option<PathBuf> {
    let text = path.to_str()?.replace('/', "\\");
    if let Some(rest) = text.strip_prefix("\\\\") {
        let mut parts = rest.split('\\').filter(|part| !part.is_empty());
        let (server, share) = (parts.next()?, parts.next()?);
        return Some(PathBuf::from(format!("\\\\{server}\\{share}\\")));
    }
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(drive), Some(':')) if drive.is_ascii_alphabetic() => {
            Some(PathBuf::from(format!("{drive}:\\")))
        }
        _ => None,
    }
}

/// Current mount points, where the system lists them in a file. Empty
/// when it does not, or when the list cannot be read.
pub fn mount_points(os: Os) -> Vec<PathBuf> {
    match os {
        Os::Linux => std::fs::read_to_string("/proc/self/mounts")
            .map(|table| parse_mount_points(&table))
            .unwrap_or_default(),
        Os::MacOs | Os::Windows => Vec::new(),
    }
}

/// The mount point column of `/proc/self/mounts`, where a space, tab,
/// newline or backslash in a name is written as a three-digit octal escape.
pub fn parse_mount_points(table: &str) -> Vec<PathBuf> {
    table
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(|field| PathBuf::from(unescape_octal(field)))
        .collect()
}

fn unescape_octal(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let code = (bytes[i] == b'\\')
            .then(|| field.get(i + 1..i + 4))
            .flatten()
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        match code {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceId;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn apple_only_sources_exist_only_on_macos() {
        for os in Os::ALL {
            for source in SourceId::ALL {
                let apple = matches!(source, SourceId::Ios | SourceId::Xcode | SourceId::Homebrew);
                assert_eq!(
                    os.has_source(source),
                    !apple || os == Os::MacOs,
                    "{os:?} {source:?}"
                );
            }
        }
    }

    #[test]
    fn home_comes_from_userprofile_on_windows() {
        let env = env_of(&[("USERPROFILE", "C:\\Users\\u")]);
        assert_eq!(
            Os::Windows.home_dir(&env),
            Some(PathBuf::from("C:\\Users\\u"))
        );
        assert_eq!(Os::Linux.home_dir(&env), None);
        assert_eq!(Os::MacOs.home_dir(&env), None);
    }

    #[test]
    fn home_comes_from_home_on_unix() {
        let env = env_of(&[("HOME", "/home/u")]);
        assert_eq!(Os::Linux.home_dir(&env), Some(PathBuf::from("/home/u")));
        assert_eq!(Os::MacOs.home_dir(&env), Some(PathBuf::from("/home/u")));
        assert_eq!(Os::Windows.home_dir(&env), None);
    }

    #[test]
    fn an_empty_home_variable_is_no_home() {
        let env = env_of(&[("HOME", "")]);
        assert_eq!(Os::Linux.home_dir(&env), None);
    }

    #[test]
    fn macos_protected_dirs_are_unchanged() {
        let home = Path::new("/Users/u");
        let env = env_of(&[]);
        assert_eq!(
            Os::MacOs.protected_dirs(home, &env),
            [
                "/Users/u",
                "/Users/u/Library",
                "/Users/u/Library/Caches",
                "/Users/u/Library/Developer",
                "/Users/u/Library/Logs",
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn linux_protects_the_xdg_folders() {
        let home = Path::new("/home/u");
        let env = env_of(&[]);
        assert_eq!(
            Os::Linux.protected_dirs(home, &env),
            [
                "/home/u",
                "/home/u/.cache",
                "/home/u/.config",
                "/home/u/.local",
                "/home/u/.local/share",
                "/home/u/.local/state",
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn linux_protects_xdg_overrides() {
        let home = Path::new("/home/u");
        let env = env_of(&[("XDG_DATA_HOME", "/data/xdg"), ("XDG_CACHE_HOME", "/c")]);
        let dirs = Os::Linux.protected_dirs(home, &env);
        assert!(dirs.contains(&PathBuf::from("/data/xdg")));
        assert!(dirs.contains(&PathBuf::from("/c")));
        // The defaults stay protected next to the overrides.
        assert!(dirs.contains(&PathBuf::from("/home/u/.local/share")));
    }

    #[test]
    fn windows_protects_appdata() {
        let home = Path::new("/u");
        let env = env_of(&[("LOCALAPPDATA", "/elsewhere/Local")]);
        let dirs = Os::Windows.protected_dirs(home, &env);
        for rel in [
            "AppData",
            "AppData/Local",
            "AppData/LocalLow",
            "AppData/Roaming",
            "AppData/Local/Temp",
        ] {
            assert!(dirs.contains(&home.join(rel)), "{rel}");
        }
        assert!(dirs.contains(&PathBuf::from("/elsewhere/Local")));
        assert!(dirs.contains(&home.to_path_buf()));
    }

    // Linux and macOS rules over their own absolute paths, which are not
    // absolute on Windows.
    #[cfg(unix)]
    #[test]
    fn extra_root_above_home_is_dropped() {
        let home = Path::new("/home/u");
        for above in ["/", "/home", "/home/u"] {
            let env = move |k: &str| (k == "XDG_CACHE_HOME").then(|| OsString::from(above));
            assert!(
                Os::Linux.guard_extra_roots(home, &env, None).is_empty(),
                "{above}"
            );
        }
        let env = env_of(&[("XDG_CACHE_HOME", "/var/cache/u")]);
        assert_eq!(
            Os::Linux.guard_extra_roots(home, &env, None),
            [PathBuf::from("/var/cache/u")]
        );
    }

    #[test]
    fn relative_extra_root_is_dropped() {
        let home = Path::new("/home/u");
        let env = env_of(&[("XDG_CACHE_HOME", "cache")]);
        assert!(Os::Linux.guard_extra_roots(home, &env, None).is_empty());
        assert!(
            Os::MacOs
                .guard_extra_roots(Path::new("/Users/u"), &env, Some(PathBuf::from("T")))
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn macos_extra_root_is_the_darwin_cache_dir() {
        let home = Path::new("/Users/u");
        let env = env_of(&[("XDG_CACHE_HOME", "/var/cache/u")]);
        let cache = PathBuf::from("/private/var/folders/ab/C");
        assert_eq!(
            Os::MacOs.guard_extra_roots(home, &env, Some(cache.clone())),
            std::slice::from_ref(&cache)
        );
        // Neither of the other systems has one.
        assert!(
            Os::Windows
                .guard_extra_roots(home, &env, Some(cache))
                .is_empty()
        );
    }

    fn mounts(points: &[&str]) -> Vec<PathBuf> {
        points.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn linux_path_under_an_unmounted_mount_dir_is_not_mounted() {
        let exists = |_: &Path| true;
        let plain = mounts(&["/", "/home"]);
        for path in ["/mnt/disk/r", "/media/u/disk/r", "/run/media/u/disk/r"] {
            assert!(
                !Os::Linux.volume_mounted(Path::new(path), &plain, &exists),
                "{path}"
            );
        }
        let with_disk = mounts(&["/", "/mnt/disk", "/media/u/disk", "/run/media/u/disk"]);
        for path in ["/mnt/disk/r", "/media/u/disk/r", "/run/media/u/disk/r"] {
            assert!(
                Os::Linux.volume_mounted(Path::new(path), &with_disk, &exists),
                "{path}"
            );
        }
        // Anywhere else is the system's own disk.
        assert!(Os::Linux.volume_mounted(Path::new("/home/u/r"), &plain, &exists));
    }

    #[test]
    fn linux_mount_list_that_could_not_be_read_mounts_nothing_removable() {
        let exists = |_: &Path| true;
        assert!(!Os::Linux.volume_mounted(Path::new("/mnt/disk/r"), &[], &exists));
        assert!(Os::Linux.volume_mounted(Path::new("/home/u/r"), &[], &exists));
    }

    #[test]
    fn linux_sibling_mount_does_not_count() {
        let exists = |_: &Path| true;
        let other = mounts(&["/", "/mnt/disk2"]);
        assert!(!Os::Linux.volume_mounted(Path::new("/mnt/disk/r"), &other, &exists));
    }

    #[test]
    fn windows_missing_drive_is_not_mounted() {
        let gone = |_: &Path| false;
        let there = |p: &Path| p == Path::new("D:\\");
        for path in ["D:\\r\\.git", "D:/r/.git", "d:\\r"] {
            assert!(
                !Os::Windows.volume_mounted(Path::new(path), &[], &gone),
                "{path}"
            );
        }
        assert!(Os::Windows.volume_mounted(Path::new("D:\\r\\.git"), &[], &there));
        assert!(Os::Windows.volume_mounted(Path::new("D:/r/.git"), &[], &there));
    }

    #[test]
    fn windows_network_share_is_checked_by_its_root() {
        let share = |p: &Path| p == Path::new("\\\\nas\\code\\");
        let gone = |_: &Path| false;
        let path = Path::new("\\\\nas\\code\\r\\.git");
        assert!(Os::Windows.volume_mounted(path, &[], &share));
        assert!(!Os::Windows.volume_mounted(path, &[], &gone));
    }

    /// A path whose volume cannot be told is not known to be mounted.
    #[test]
    fn windows_path_without_a_volume_is_not_mounted() {
        let exists = |_: &Path| true;
        assert!(!Os::Windows.volume_mounted(Path::new("r\\.git"), &[], &exists));
    }

    #[test]
    fn macos_rule_is_unchanged() {
        let none = |_: &Path| false;
        let disk = |p: &Path| p == Path::new("/Volumes/Disk");
        assert!(!Os::MacOs.volume_mounted(Path::new("/Volumes/Disk/r"), &[], &none));
        assert!(Os::MacOs.volume_mounted(Path::new("/Volumes/Disk/r"), &[], &disk));
        assert!(Os::MacOs.volume_mounted(Path::new("/Users/u/r"), &[], &none));
    }

    #[test]
    fn parses_mount_points() {
        let table = "sysfs /sys sysfs rw,nosuid 0 0\n\
                     /dev/sda1 / ext4 rw 0 0\n\
                     /dev/sdb1 /media/u/My\\040Disk ext4 rw 0 0\n\
                     garbage\n";
        assert_eq!(
            parse_mount_points(table),
            mounts(&["/sys", "/", "/media/u/My Disk"])
        );
    }

    #[test]
    fn android_sdk_default_follows_the_platform() {
        let none = env_of(&[]);
        assert_eq!(
            Os::MacOs.android_sdk_default(Path::new("/Users/u"), &none),
            Some(PathBuf::from("/Users/u/Library/Android/sdk"))
        );
        assert_eq!(
            Os::Linux.android_sdk_default(Path::new("/home/u"), &none),
            Some(PathBuf::from("/home/u/Android/Sdk"))
        );
        // Windows keeps it under the local app data folder, wherever that is.
        assert_eq!(
            Os::Windows.android_sdk_default(Path::new("/u"), &none),
            None
        );
        let env = env_of(&[("LOCALAPPDATA", "/u/AppData/Local")]);
        assert_eq!(
            Os::Windows.android_sdk_default(Path::new("/u"), &env),
            Some(PathBuf::from("/u/AppData/Local/Android/Sdk"))
        );
    }

    #[test]
    fn sdk_scripts_are_batch_files_on_windows() {
        assert_eq!(Os::Windows.script_name("avdmanager"), "avdmanager.bat");
        assert_eq!(Os::Linux.script_name("avdmanager"), "avdmanager");
        assert_eq!(Os::MacOs.script_name("avdmanager"), "avdmanager");
    }

    #[test]
    fn git_paths_are_strict_utf8_where_paths_are_not_bytes() {
        assert_eq!(
            utf8_path(b"src/caf\xc3\xa9.rs"),
            Some(PathBuf::from("src/café.rs"))
        );
        assert_eq!(utf8_path(b"src/caf\xe9.rs"), None);
    }

    #[cfg(unix)]
    #[test]
    fn git_paths_are_raw_bytes_on_unix() {
        use std::os::unix::ffi::OsStrExt;
        let path = path_from_git_bytes(b"src/caf\xe9.rs").unwrap();
        assert_eq!(path.as_os_str().as_bytes(), b"src/caf\xe9.rs");
    }

    #[test]
    fn windows_tries_pathext_extensions() {
        let names = Os::Windows.exe_names("npm", Some(".COM;.EXE;.BAT;.CMD"));
        assert_eq!(names, ["npm.COM", "npm.EXE", "npm.BAT", "npm.CMD"]);
        // Without PATHEXT, the system's own default list.
        assert_eq!(Os::Windows.exe_names("npm", None), names);
        assert_eq!(Os::Windows.exe_names("npm", Some("")), names);
        assert_eq!(
            Os::Windows.exe_names("git", Some(".EXE;;.CMD")),
            ["git.EXE", "git.CMD"]
        );
    }

    #[test]
    fn windows_keeps_an_explicit_extension() {
        assert_eq!(
            Os::Windows.exe_names("avdmanager.bat", Some(".EXE")),
            ["avdmanager.bat"]
        );
    }

    #[test]
    fn unix_uses_the_name_as_is() {
        for os in [Os::MacOs, Os::Linux] {
            assert_eq!(os.exe_names("npm", Some(".EXE;.CMD")), ["npm"]);
        }
    }

    #[test]
    fn executable_is_found_by_any_of_its_names() {
        let d = tempfile::tempdir().unwrap();
        let (empty, bin) = (d.path().join("empty"), d.path().join("bin"));
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(bin.join("npm.EXE")).unwrap();
        std::fs::write(bin.join("npm.CMD"), "").unwrap();
        let names = Os::Windows.exe_names("npm", Some(".EXE;.CMD"));
        // A folder with the right name is not an executable.
        assert_eq!(
            find_executable([empty.clone(), bin.clone()], &names),
            Some(bin.join("npm.CMD"))
        );
        assert_eq!(find_executable([empty], &names), None);
    }

    #[test]
    fn home_folders_skipped_by_the_walk() {
        assert_eq!(Os::MacOs.skip_at_home(), ["Library", ".Trash"]);
        assert_eq!(Os::Linux.skip_at_home(), [".cache"]);
        assert_eq!(Os::Windows.skip_at_home(), ["AppData", "scoop"]);
    }
}
