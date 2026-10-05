//! The Trash: Finder's on macOS (the home folder's and those of mounted
//! volumes) and the freedesktop one on Linux.

use std::path::{Path, PathBuf};

use crate::fsutil::dir_size;
use crate::model::{Item, Removal, SourceId, Status};
use crate::platform::{Env, Os};
use crate::scan::{ScanCtx, ScanEvent, Scanner, Sender};

pub struct Trash {
    /// Folders whose entries are the trashed items, the user's own first.
    pub roots: Vec<PathBuf>,
    /// How this system empties it.
    pub empty: Empty,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Empty {
    /// Finder empties every Trash, on every volume, the way the user would,
    /// and refuses items it still considers in use.
    Finder,
    /// `gio trash --empty` empties the home Trash and the ones on other
    /// disks.
    Gio,
    /// No tool to ask: these folders of the home Trash are deleted, each one
    /// through the guard.
    Folders(Vec<PathBuf>),
}

impl Trash {
    /// The Trash of `os`, for the user whose home folder is `home`.
    pub fn for_os(os: Os, home: &Path, env: Env, has_bin: &dyn Fn(&str) -> bool) -> Trash {
        match os {
            Os::MacOs => Trash::finder(finder_roots(home)),
            Os::Linux => {
                // https://specifications.freedesktop.org/trash-spec/latest/
                let data = env("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .unwrap_or_else(|| home.join(".local/share"));
                let trash = data.join("Trash");
                let files = trash.join("files");
                let empty = if has_bin("gio") {
                    Empty::Gio
                } else {
                    let info = trash.join("info");
                    let folders = std::iter::once(files.clone())
                        .chain(info.is_dir().then_some(info))
                        .collect();
                    Empty::Folders(folders)
                };
                Trash {
                    roots: vec![files],
                    empty,
                }
            }
            // Windows keeps a Recycle Bin, not a folder to read.
            Os::Windows => Trash {
                roots: Vec::new(),
                empty: Empty::Folders(Vec::new()),
            },
        }
    }

    /// Finder's Trash over the given folders.
    pub fn finder(roots: Vec<PathBuf>) -> Trash {
        Trash {
            roots,
            empty: Empty::Finder,
        }
    }
}

/// `~/.Trash` first, then `/Volumes/*/.Trashes/<uid>`.
#[cfg(unix)]
fn finder_roots(home: &Path) -> Vec<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let mut roots = vec![home.join(".Trash")];
    if let Ok(uid) = std::fs::metadata(home).map(|m| m.uid())
        && let Ok(volumes) = std::fs::read_dir("/Volumes")
    {
        let mut extra: Vec<PathBuf> = volumes
            .flatten()
            .map(|v| v.path().join(".Trashes").join(uid.to_string()))
            .filter(|p| p.is_dir())
            .collect();
        extra.sort();
        roots.extend(extra);
    }
    roots
}

#[cfg(not(unix))]
fn finder_roots(home: &Path) -> Vec<PathBuf> {
    vec![home.join(".Trash")]
}

/// Entries the user put in the Trash, ignoring Finder's `.DS_Store`.
fn entries(root: &Path) -> std::io::Result<usize> {
    Ok(std::fs::read_dir(root)?
        .flatten()
        .filter(|e| e.file_name() != ".DS_Store")
        .count())
}

impl Scanner for Trash {
    fn source(&self) -> SourceId {
        SourceId::Trash
    }

    fn available(&self) -> bool {
        self.roots.first().is_some_and(|r| r.is_dir())
    }

    fn scan(&self, ctx: &ScanCtx, tx: &Sender<ScanEvent>) -> anyhow::Result<()> {
        let Some(home_trash) = self.roots.first() else {
            return Ok(());
        };
        // Reading ~/.Trash needs Full Disk Access on macOS; without it the
        // Trash would look empty, so say so instead.
        let mut count = match entries(home_trash) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let _ = tx.send(ScanEvent::NoAccess(SourceId::Trash));
                anyhow::bail!("permission denied reading the Trash")
            }
            Err(e) => return Err(e.into()),
        };
        // A volume's Trash we cannot read is left out, not an error.
        count += self.roots[1..]
            .iter()
            .filter_map(|r| entries(r).ok())
            .sum::<usize>();
        if count == 0 {
            return Ok(());
        }
        let noun = if count == 1 { "item" } else { "items" };
        let item = Item {
            id: ctx.next_id(),
            source: SourceId::Trash,
            label: "Trash".into(),
            path: Some(home_trash.clone()),
            size: None,
            status: vec![Status::Detail(format!("{count} {noun}"))],
            lock: ctx.inuse.lock_for(home_trash),
            // What is in the Trash can still be put back.
            safe: false,
            removal: match &self.empty {
                Empty::Finder => Removal::Command {
                    argv: vec![
                        "osascript".into(),
                        "-e".into(),
                        "tell application \"Finder\" to empty trash".into(),
                    ],
                    cwd: None,
                },
                Empty::Gio => Removal::Command {
                    argv: vec!["gio".into(), "trash".into(), "--empty".into()],
                    cwd: None,
                },
                Empty::Folders(folders) => Removal::RemovePaths(folders.clone()),
            },
            age_days: None,
            recheck: crate::model::Recheck::default(),
        };
        let id = item.id;
        let _ = tx.send(ScanEvent::Found(item));
        if matches!(self.empty, Empty::Folders(_)) {
            let note = "Trash folders on other disks are left alone (gio is not installed)";
            let _ = tx.send(ScanEvent::Note(SourceId::Trash, note.into()));
        }
        let (roots, tx) = (self.roots.clone(), tx.clone());
        rayon::spawn(move || {
            let bytes = roots.iter().map(|r| dir_size(r)).sum();
            let _ = tx.send(ScanEvent::Size(id, bytes));
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inuse::InUse;
    use std::fs;

    fn scan(roots: Vec<PathBuf>) -> anyhow::Result<(Vec<Item>, Vec<u64>)> {
        let ctx = ScanCtx::new(PathBuf::from("/w"), PathBuf::from("/h"), InUse::default());
        scan_trash(&Trash::finder(roots), &ctx)
    }

    fn scan_trash(trash: &Trash, ctx: &ScanCtx) -> anyhow::Result<(Vec<Item>, Vec<u64>)> {
        let (tx, rx) = crossbeam_channel::unbounded();
        trash.scan(ctx, &tx)?;
        drop(tx);
        let (mut items, mut sizes) = (Vec::new(), Vec::new());
        for ev in rx.iter() {
            match ev {
                ScanEvent::Found(i) => items.push(i),
                ScanEvent::Size(_, b) => sizes.push(b),
                _ => {}
            }
        }
        Ok((items, sizes))
    }

    #[test]
    fn trash_with_files_is_one_item_emptied_by_finder_and_never_safe() {
        let d = tempfile::tempdir().unwrap();
        let trash = d.path().join(".Trash");
        fs::create_dir_all(trash.join("old project/node_modules")).unwrap();
        fs::write(trash.join("report.pdf"), vec![1u8; 20_000]).unwrap();
        fs::write(trash.join(".DS_Store"), "x").unwrap();
        let (items, sizes) = scan(vec![trash]).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "Trash");
        assert_eq!(items[0].status, vec![Status::Detail("2 items".into())]);
        assert!(!items[0].safe);
        assert!(
            matches!(&items[0].removal, Removal::Command { argv, .. } if argv[0] == "osascript" && argv[2].contains("empty trash"))
        );
        assert!(sizes[0] >= 20_000);
    }

    fn no_env(_: &str) -> Option<std::ffi::OsString> {
        None
    }

    /// A freedesktop Trash under `home` holding one file and its record.
    fn linux_trash(home: &Path) -> PathBuf {
        let trash = home.join(".local/share/Trash");
        fs::create_dir_all(trash.join("files")).unwrap();
        fs::create_dir_all(trash.join("info")).unwrap();
        fs::write(trash.join("files/report.pdf"), vec![1u8; 20_000]).unwrap();
        fs::write(trash.join("info/report.pdf.trashinfo"), "[Trash Info]\n").unwrap();
        trash
    }

    fn scan_linux(home: &Path, trash: &Trash) -> (Vec<Item>, Vec<u64>) {
        let ctx = ScanCtx::new(home.to_path_buf(), home.to_path_buf(), InUse::default());
        scan_trash(trash, &ctx).unwrap()
    }

    #[test]
    fn linux_trash_counts_files_and_uses_gio_when_present() {
        let d = tempfile::tempdir().unwrap();
        let trash = linux_trash(d.path());
        fs::write(trash.join("files/second"), "x").unwrap();
        let found = Trash::for_os(Os::Linux, d.path(), &no_env, &|bin| bin == "gio");
        assert!(found.available());
        let (items, sizes) = scan_linux(d.path(), &found);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, vec![Status::Detail("2 items".into())]);
        assert!(!items[0].safe);
        assert_eq!(
            items[0].removal,
            Removal::Command {
                argv: vec!["gio".into(), "trash".into(), "--empty".into()],
                cwd: None,
            }
        );
        assert!(sizes[0] >= 20_000);
    }

    #[test]
    fn linux_trash_without_gio_removes_files_and_info() {
        let d = tempfile::tempdir().unwrap();
        let trash = linux_trash(d.path());
        let found = Trash::for_os(Os::Linux, d.path(), &no_env, &|_| false);
        let (items, _) = scan_linux(d.path(), &found);
        assert_eq!(
            items[0].removal,
            Removal::RemovePaths(vec![trash.join("files"), trash.join("info")])
        );
        assert!(!items[0].safe);
    }

    #[test]
    fn linux_trash_without_gio_says_other_disks_are_left_alone() {
        let d = tempfile::tempdir().unwrap();
        linux_trash(d.path());
        let ctx = ScanCtx::new(
            d.path().to_path_buf(),
            d.path().to_path_buf(),
            InUse::default(),
        );
        let notes = |has_gio: bool| -> Vec<String> {
            let (tx, rx) = crossbeam_channel::unbounded();
            Trash::for_os(Os::Linux, d.path(), &no_env, &move |_| has_gio)
                .scan(&ctx, &tx)
                .unwrap();
            drop(tx);
            rx.iter()
                .filter_map(|e| match e {
                    ScanEvent::Note(SourceId::Trash, note) => Some(note),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            notes(false),
            ["Trash folders on other disks are left alone (gio is not installed)"]
        );
        assert!(notes(true).is_empty());
    }

    #[test]
    fn linux_trash_follows_xdg_data_home() {
        let d = tempfile::tempdir().unwrap();
        let data = d.path().join("data");
        fs::create_dir_all(data.join("Trash/files")).unwrap();
        fs::write(data.join("Trash/files/a"), "a").unwrap();
        let value = data.clone().into_os_string();
        let env = move |key: &str| (key == "XDG_DATA_HOME").then(|| value.clone());
        let found = Trash::for_os(Os::Linux, &d.path().join("home"), &env, &|_| false);
        assert_eq!(found.roots, [data.join("Trash/files")]);
    }

    #[test]
    fn linux_without_a_trash_folder_has_no_trash_source() {
        let d = tempfile::tempdir().unwrap();
        assert!(!Trash::for_os(Os::Linux, d.path(), &no_env, &|_| true).available());
    }

    #[test]
    fn empty_linux_trash_lists_nothing() {
        let d = tempfile::tempdir().unwrap();
        let trash = linux_trash(d.path());
        fs::remove_file(trash.join("files/report.pdf")).unwrap();
        let found = Trash::for_os(Os::Linux, d.path(), &no_env, &|_| false);
        assert!(scan_linux(d.path(), &found).0.is_empty());
    }

    /// Removing the Trash by hand goes through the guard like any folder: a
    /// `files` folder, or a Trash, that is a symlink is never followed.
    #[cfg(unix)]
    #[test]
    fn symlinked_trash_folders_are_refused_by_the_guard() {
        use crate::remove::{Guard, RealExecutor, RemoveError, RemoveEvent, run_removals};
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), "keep").unwrap();
        fs::create_dir_all(outside.path().join("files")).unwrap();
        fs::write(outside.path().join("files/keep"), "keep").unwrap();

        let refused_by_guard = |home: &Path| -> RemoveError {
            let found = Trash::for_os(Os::Linux, home, &no_env, &|_| false);
            let (items, _) = scan_linux(home, &found);
            assert_eq!(items.len(), 1);
            let protected = Os::Linux.protected_dirs(home, &no_env);
            let guard = Guard::new(home.to_path_buf(), home.to_path_buf(), protected, vec![]);
            let (tx, rx) = crossbeam_channel::unbounded();
            run_removals(items, &RealExecutor, &guard, &|_| Ok(()), &|_, _| None, tx);
            rx.iter()
                .find_map(|e| match e {
                    RemoveEvent::Err(_, err) => Some(err),
                    _ => None,
                })
                .expect("the removal must be refused")
        };

        // `files` is a symlink to a folder outside the home folder.
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap();
        let trash = home.join(".local/share/Trash");
        fs::create_dir_all(&trash).unwrap();
        std::os::unix::fs::symlink(outside.path(), trash.join("files")).unwrap();
        assert_eq!(refused_by_guard(&home), RemoveError::Refused("symlink"));

        // The whole Trash is a symlink to a folder outside the home folder.
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap();
        fs::create_dir_all(home.join(".local/share")).unwrap();
        std::os::unix::fs::symlink(outside.path(), home.join(".local/share/Trash")).unwrap();
        assert_eq!(
            refused_by_guard(&home),
            RemoveError::Refused("outside allowed folders")
        );

        assert!(outside.path().join("keep").exists());
        assert!(outside.path().join("files/keep").exists());
    }

    #[test]
    fn empty_trash_lists_nothing() {
        let d = tempfile::tempdir().unwrap();
        let trash = d.path().join(".Trash");
        fs::create_dir_all(&trash).unwrap();
        fs::write(trash.join(".DS_Store"), "x").unwrap();
        assert!(scan(vec![trash]).unwrap().0.is_empty());
    }

    #[test]
    fn volume_trashes_count_toward_the_same_item() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join(".Trash");
        let volume = d.path().join("Volumes/Backup/.Trashes/501");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&volume).unwrap();
        fs::write(volume.join("big.mov"), vec![1u8; 50_000]).unwrap();
        let (items, sizes) = scan(vec![home, volume]).unwrap();
        assert_eq!(items[0].status, vec![Status::Detail("1 item".into())]);
        assert!(sizes[0] >= 50_000);
    }

    #[test]
    fn unreadable_trash_is_an_error_not_an_empty_trash() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let trash = d.path().join(".Trash");
        fs::create_dir_all(&trash).unwrap();
        fs::write(trash.join("a"), "a").unwrap();
        fs::set_permissions(&trash, fs::Permissions::from_mode(0o000)).unwrap();
        let result = scan(vec![trash.clone()]);
        fs::set_permissions(&trash, fs::Permissions::from_mode(0o755)).unwrap();
        let err = result.unwrap_err().to_string();
        assert!(err.contains("permission denied"), "{err}");
    }

    #[test]
    fn unreadable_trash_reports_no_access() {
        // The Full Disk Access prompt hangs on this event, not on the wording
        // of the error.
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let trash = d.path().join(".Trash");
        fs::create_dir_all(&trash).unwrap();
        fs::set_permissions(&trash, fs::Permissions::from_mode(0o000)).unwrap();
        let ctx = ScanCtx::new(
            d.path().to_path_buf(),
            d.path().to_path_buf(),
            InUse::default(),
        );
        let (tx, rx) = crossbeam_channel::unbounded();
        let result = Trash::finder(vec![trash.clone()]).scan(&ctx, &tx);
        fs::set_permissions(&trash, fs::Permissions::from_mode(0o755)).unwrap();
        drop(tx);
        assert!(result.is_err());
        assert!(
            rx.iter()
                .any(|e| matches!(e, ScanEvent::NoAccess(SourceId::Trash)))
        );
    }
}
