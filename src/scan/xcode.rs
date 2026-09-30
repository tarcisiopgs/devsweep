//! Xcode data that needs its own rules: device support files and archives.

use std::path::PathBuf;

use crate::model::{Item, Removal, SourceId};
use crate::scan::{ScanCtx, ScanEvent, Scanner, Sender, size_later};

pub struct Xcode;

fn children(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

impl Scanner for Xcode {
    fn source(&self) -> SourceId {
        SourceId::Xcode
    }

    fn available(&self) -> bool {
        true
    }

    fn scan(&self, ctx: &ScanCtx, tx: &Sender<ScanEvent>) -> anyhow::Result<()> {
        let base = ctx.home.join("Library/Developer/Xcode");
        let mut found: Vec<(String, PathBuf)> = Vec::new();
        for dir in [
            "iOS DeviceSupport",
            "watchOS DeviceSupport",
            "tvOS DeviceSupport",
            "visionOS DeviceSupport",
        ] {
            for p in children(&base.join(dir)) {
                let name = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                found.push((format!("Device support · {name}"), p));
            }
        }
        for day in children(&base.join("Archives")) {
            for p in children(&day)
                .into_iter()
                .filter(|p| p.extension().is_some_and(|e| e == "xcarchive"))
            {
                let name = p
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                found.push((format!("Archive · {name}"), p));
            }
        }
        for (label, path) in found {
            let id = ctx.next_id();
            let item = Item {
                id,
                source: SourceId::Xcode,
                label,
                path: Some(path.clone()),
                size: None,
                status: vec![],
                lock: None,
                safe: false,
                removal: Removal::RemoveDir(path.clone()),
                age_days: None,
                recheck: crate::model::Recheck::default(),
            };
            let _ = tx.send(ScanEvent::Found(item));
            size_later(path, id, tx.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inuse::InUse;
    use crate::model::Removal;
    use std::fs;

    #[test]
    fn archives_are_never_safe_and_device_support_is_listed() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("Library/Developer/Xcode");
        let archive = base.join("Archives/2026-09-01/App 1.xcarchive");
        let support = base.join("iOS DeviceSupport/iPhone15,2 17.0 (21A329)");
        for p in [&archive, &support] {
            fs::create_dir_all(p).unwrap();
            fs::write(p.join("blob"), vec![1u8; 4096]).unwrap();
        }
        let ctx = ScanCtx::new(d.path().into(), d.path().into(), InUse::default());
        let (tx, rx) = crossbeam_channel::unbounded();
        Xcode.scan(&ctx, &tx).unwrap();
        drop(tx);
        let items: Vec<Item> = rx
            .iter()
            .filter_map(|e| {
                if let ScanEvent::Found(i) = e {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        let a = items.iter().find(|i| i.label == "Archive · App 1").unwrap();
        assert!(!a.safe);
        assert_eq!(a.removal, Removal::RemoveDir(archive.clone()));
        let s = items
            .iter()
            .find(|i| i.label == "Device support · iPhone15,2 17.0 (21A329)")
            .unwrap();
        assert!(!s.safe);
    }
}
