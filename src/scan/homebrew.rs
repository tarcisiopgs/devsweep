//! Homebrew's own cleanup (old versions, downloads).

use crate::model::{Item, Removal, SourceId};
use crate::scan::docker::parse_human_size;
use crate::scan::{ScanCtx, ScanEvent, Scanner, Sender, run, which};

/// Bytes from `brew cleanup -n`'s "would free approximately X" line.
pub fn cleanup_bytes(output: &str) -> Option<u64> {
    let line = output
        .lines()
        .find(|l| l.contains("would free approximately"))?;
    let after = line
        .split("approximately")
        .nth(1)?
        .split_whitespace()
        .next()?;
    parse_human_size(after)
}

pub struct Homebrew;

impl Scanner for Homebrew {
    fn source(&self) -> SourceId {
        SourceId::Homebrew
    }

    fn available(&self) -> bool {
        which("brew").is_some()
    }

    fn scan(&self, ctx: &ScanCtx, tx: &Sender<ScanEvent>) -> anyhow::Result<()> {
        let out = run(&["brew", "cleanup", "-n"])?;
        if let Some(bytes) = cleanup_bytes(&out).filter(|b| *b > 0) {
            let item = Item {
                id: ctx.next_id(),
                source: SourceId::Homebrew,
                label: "Homebrew cleanup".into(),
                path: None,
                size: Some(bytes),
                status: vec![],
                lock: ctx.inuse.busy(&["brew"]),
                safe: true,
                removal: Removal::Command {
                    argv: vec!["brew".into(), "cleanup".into()],
                    cwd: None,
                },
                age_days: None,
            };
            let _ = tx.send(ScanEvent::Found(item));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brew_cleanup_size_parsed_from_fixture() {
        let out = include_str!("../../tests/fixtures/brew-cleanup-n.txt");
        assert_eq!(cleanup_bytes(out), Some(780_200_000));
    }

    #[test]
    fn nothing_to_clean_is_none() {
        assert_eq!(cleanup_bytes(""), None);
    }
}
