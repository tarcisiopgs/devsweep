//! The Trash: the home folder's and those of mounted volumes.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::fsutil::dir_size;
use crate::model::{Item, Removal, SourceId, Status};
use crate::scan::{ScanCtx, ScanEvent, Scanner, Sender};

pub struct Trash {
    /// `~/.Trash` first, then `/Volumes/*/.Trashes/<uid>`.
    pub roots: Vec<PathBuf>,
}

impl Trash {
    pub fn for_home(home: &Path) -> Trash {
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
        Trash { roots }
    }
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
        // Reading ~/.Trash needs Full Disk Access; without it the Trash
        // would look empty, so say so instead.
        let mut count = match entries(home_trash) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let _ = tx.send(ScanEvent::NoAccess(SourceId::Trash));
                anyhow::bail!("permission denied reading ~/.Trash")
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
            // Finder empties every Trash, on every volume, the way the user
            // would, and refuses items it still considers in use.
            removal: Removal::Command {
                argv: vec![
                    "osascript".into(),
                    "-e".into(),
                    "tell application \"Finder\" to empty trash".into(),
                ],
                cwd: None,
            },
            age_days: None,
            recheck: crate::model::Recheck::default(),
        };
        let id = item.id;
        let _ = tx.send(ScanEvent::Found(item));
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
        let (tx, rx) = crossbeam_channel::unbounded();
        Trash { roots }.scan(&ctx, &tx)?;
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
        let result = Trash {
            roots: vec![trash.clone()],
        }
        .scan(&ctx, &tx);
        fs::set_permissions(&trash, fs::Permissions::from_mode(0o755)).unwrap();
        drop(tx);
        assert!(result.is_err());
        assert!(
            rx.iter()
                .any(|e| matches!(e, ScanEvent::NoAccess(SourceId::Trash)))
        );
    }
}
