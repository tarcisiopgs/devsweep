//! Scanner contract and parallel orchestration.

pub mod android;
pub mod artifacts;
pub mod catalog;
pub mod docker;
pub mod homebrew;
pub mod ios;
pub mod worktrees;
pub mod xcode;

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

pub use crossbeam_channel::Sender;

use crate::fsutil::dir_size;
use crate::inuse::InUse;
use crate::model::{Item, ItemId, SourceId};

pub enum ScanEvent {
    Found(Item),
    Size(ItemId, u64),
    Note(SourceId, String),
    Done(SourceId),
    Failed(SourceId, String),
}

pub struct ScanCtx {
    pub target: PathBuf,
    pub home: PathBuf,
    pub inuse: InUse,
    /// Worktree paths already reported by the folder scan.
    pub seen_worktrees: Mutex<HashSet<PathBuf>>,
    worktrees_done: (Mutex<bool>, Condvar),
    ids: AtomicU64,
}

impl ScanCtx {
    pub fn new(target: PathBuf, home: PathBuf, inuse: InUse) -> ScanCtx {
        ScanCtx {
            target,
            home,
            inuse,
            seen_worktrees: Mutex::new(HashSet::new()),
            worktrees_done: (Mutex::new(false), Condvar::new()),
            ids: AtomicU64::new(1),
        }
    }

    /// Signal that the folder worktree scan finished (or never runs).
    pub fn mark_worktrees_done(&self) {
        let (lock, cvar) = &self.worktrees_done;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }

    /// Block until the folder worktree scan finished.
    pub fn wait_worktrees_done(&self) {
        let (lock, cvar) = &self.worktrees_done;
        let mut done = lock.lock().unwrap();
        while !*done {
            done = cvar.wait(done).unwrap();
        }
    }

    pub fn next_id(&self) -> ItemId {
        self.ids.fetch_add(1, Ordering::Relaxed)
    }
}

pub trait Scanner: Send + Sync {
    fn source(&self) -> SourceId;
    /// Whether the tool behind this source exists on this machine.
    fn available(&self) -> bool;
    fn scan(&self, ctx: &ScanCtx, tx: &Sender<ScanEvent>) -> anyhow::Result<()>;
}

/// Every scanner that ships with devsweep.
pub fn all_scanners(home: &std::path::Path) -> Vec<Box<dyn Scanner>> {
    let mut scanners: Vec<Box<dyn Scanner>> = vec![
        Box::new(artifacts::Artifacts),
        Box::new(worktrees::Worktrees),
        Box::new(worktrees::AgentWorktrees::for_home(home)),
        Box::new(ios::Ios),
        Box::new(android::Android::detect(home)),
        Box::new(docker::Docker),
        Box::new(xcode::Xcode),
        Box::new(homebrew::Homebrew),
    ];
    if let Ok(catalog) = catalog::Catalog::load() {
        scanners.push(Box::new(catalog));
    }
    scanners
}

/// Run each available scanner on its own thread. Every scanner ends with
/// `Done`, preceded by `Failed` when it errored or panicked.
pub fn spawn_all(ctx: Arc<ScanCtx>, scanners: Vec<Box<dyn Scanner>>, tx: Sender<ScanEvent>) {
    for scanner in scanners.into_iter().filter(|s| s.available()) {
        let (ctx, tx) = (Arc::clone(&ctx), tx.clone());
        std::thread::spawn(move || {
            let source = scanner.source();
            match catch_unwind(AssertUnwindSafe(|| scanner.scan(&ctx, &tx))) {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    let _ = tx.send(ScanEvent::Failed(source, err.to_string()));
                }
                Err(_) => {
                    let _ = tx.send(ScanEvent::Failed(source, "internal error".into()));
                }
            }
            let _ = tx.send(ScanEvent::Done(source));
        });
    }
}

/// Compute a directory size in the background and report it as `Size`.
pub fn size_later(path: PathBuf, id: ItemId, tx: Sender<ScanEvent>) {
    rayon::spawn(move || {
        let _ = tx.send(ScanEvent::Size(id, dir_size(&path)));
    });
}

/// Look up an executable on `PATH`.
pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_file())
}

/// Run a command and return its stdout; a non-zero exit becomes an error
/// carrying the first line of stderr.
pub fn run(argv: &[&str]) -> anyhow::Result<String> {
    let (bin, args) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("empty command"))?;
    let out = Command::new(bin).args(args).output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let first = stderr.lines().next().unwrap_or("").trim();
        anyhow::bail!("{} failed: {}", bin, first);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inuse::InUse;
    use crate::model::SourceId;
    use std::path::PathBuf;
    use std::sync::Arc;

    struct Fake {
        source: SourceId,
        available: bool,
        behavior: &'static str,
    }

    impl Scanner for Fake {
        fn source(&self) -> SourceId {
            self.source
        }
        fn available(&self) -> bool {
            self.available
        }
        fn scan(&self, _ctx: &ScanCtx, _tx: &Sender<ScanEvent>) -> anyhow::Result<()> {
            match self.behavior {
                "err" => anyhow::bail!("boom"),
                "panic" => panic!("kaboom"),
                _ => Ok(()),
            }
        }
    }

    fn ctx() -> Arc<ScanCtx> {
        Arc::new(ScanCtx::new(
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp"),
            InUse::default(),
        ))
    }

    fn collect(scanners: Vec<Box<dyn Scanner>>) -> Vec<ScanEvent> {
        let (tx, rx) = crossbeam_channel::unbounded();
        spawn_all(ctx(), scanners, tx);
        rx.iter().collect()
    }

    #[test]
    fn spawn_all_emits_done_for_each_and_failed_for_errors() {
        let events = collect(vec![
            Box::new(Fake {
                source: SourceId::DevCaches,
                available: true,
                behavior: "ok",
            }),
            Box::new(Fake {
                source: SourceId::Docker,
                available: true,
                behavior: "err",
            }),
        ]);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ScanEvent::Done(SourceId::DevCaches)))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ScanEvent::Failed(SourceId::Docker, m) if m.contains("boom")))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ScanEvent::Done(SourceId::Docker)))
        );
    }

    #[test]
    fn unavailable_scanner_emits_nothing() {
        let events = collect(vec![Box::new(Fake {
            source: SourceId::Android,
            available: false,
            behavior: "ok",
        })]);
        assert!(events.is_empty());
    }

    #[test]
    fn panicking_scanner_becomes_failed() {
        let events = collect(vec![Box::new(Fake {
            source: SourceId::Ios,
            available: true,
            behavior: "panic",
        })]);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ScanEvent::Failed(SourceId::Ios, m) if m == "internal error"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ScanEvent::Done(SourceId::Ios)))
        );
    }

    #[test]
    fn ids_are_unique() {
        let c = ctx();
        assert_ne!(c.next_id(), c.next_id());
    }

    #[test]
    fn run_returns_stdout_and_errors_on_failure() {
        assert_eq!(run(&["echo", "hi"]).unwrap().trim(), "hi");
        assert!(run(&["false"]).is_err());
    }

    #[test]
    fn which_finds_sh() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-binary-devsweep").is_none());
    }
}
