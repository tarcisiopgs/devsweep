//! Scanner contract and parallel orchestration.

pub mod android;
pub mod artifacts;
pub mod catalog;
pub mod docker;
pub mod homebrew;
pub mod ios;
pub mod trash;
pub mod worktrees;
pub mod xcode;

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

pub use crossbeam_channel::Sender;

use crate::fsutil::dir_size;
use crate::inuse::InUse;
use crate::model::{Item, ItemId, SourceId};
use crate::platform::Os;

#[expect(
    clippy::large_enum_variant,
    reason = "Found dominates the traffic anyway; boxing it would only add an allocation"
)]
pub enum ScanEvent {
    Found(Item),
    Size(ItemId, u64),
    Note(SourceId, String),
    /// macOS refused access to what the source reads; only Full Disk Access
    /// for the terminal fixes that.
    NoAccess(SourceId),
    Done(SourceId),
    Failed(SourceId, String),
}

/// Lock a mutex even after a scanner thread panicked while holding it. What
/// these mutexes guard is a plain collection or flag, still good to read,
/// and one failed scanner must not take the others down with it.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct ScanCtx {
    pub target: PathBuf,
    pub home: PathBuf,
    pub inuse: InUse,
    /// The system whose rules the scanners follow.
    pub os: Os,
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
            os: Os::current(),
            seen_worktrees: Mutex::new(HashSet::new()),
            worktrees_done: (Mutex::new(false), Condvar::new()),
            ids: AtomicU64::new(1),
        }
    }

    /// Follow another system's rules than the one this binary runs on.
    pub fn with_os(mut self, os: Os) -> ScanCtx {
        self.os = os;
        self
    }

    /// Signal that the folder worktree scan finished (or never runs).
    pub fn mark_worktrees_done(&self) {
        let (flag, cvar) = &self.worktrees_done;
        *lock(flag) = true;
        cvar.notify_all();
    }

    /// Block until the folder worktree scan finished.
    pub fn wait_worktrees_done(&self) {
        let (flag, cvar) = &self.worktrees_done;
        let mut done = lock(flag);
        while !*done {
            done = cvar.wait(done).unwrap_or_else(PoisonError::into_inner);
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

/// Every scanner that ships with devsweep and exists on `os`.
pub fn all_scanners(home: &std::path::Path, os: Os) -> Vec<Box<dyn Scanner>> {
    let mut scanners: Vec<Box<dyn Scanner>> = vec![
        Box::new(artifacts::Artifacts),
        Box::new(worktrees::Worktrees),
        Box::new(worktrees::AgentWorktrees::for_home(home)),
        Box::new(ios::Ios),
        Box::new(android::Android::detect(home)),
        Box::new(docker::Docker),
        Box::new(xcode::Xcode),
        Box::new(homebrew::Homebrew),
        Box::new(trash::Trash::for_home(home)),
    ];
    if let Ok(catalog) = catalog::Catalog::load() {
        scanners.push(Box::new(catalog));
    }
    scanners.retain(|s| os.has_source(s.source()));
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

/// Longest a scan-time tool call (docker, simctl, brew…) may take.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(60);

/// Run `argv` in `cwd`, feeding `stdin`, and kill it after `timeout`.
/// stdin, stdout and stderr each get their own thread, so a chatty command
/// or a large input can never stall both sides of a pipe.
pub fn exec(
    argv: &[&str],
    cwd: Option<&std::path::Path>,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
) -> anyhow::Result<std::process::Output> {
    use std::io::{Read, Write};
    use std::process::Stdio;
    let (bin, args) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("empty command"))?;
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn()?;
    let writer = match (child.stdin.take(), stdin) {
        (Some(mut pipe), Some(input)) => Some(std::thread::spawn(move || {
            let _ = pipe.write_all(&input);
        })),
        _ => None,
    };
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{bin} timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    Ok(std::process::Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// Like [`run`], in `cwd`, killing the command after `timeout`. Protects the
/// scan from tools that block (a `git` waiting on a macOS privacy prompt).
pub fn run_timeout(
    argv: &[&str],
    cwd: Option<&std::path::Path>,
    timeout: Duration,
) -> anyhow::Result<String> {
    let out = exec(argv, cwd, None, timeout)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let first = stderr
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        anyhow::bail!("{} failed: {}", argv[0], first);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a command and return its stdout, within [`RUN_TIMEOUT`]; a non-zero
/// exit becomes an error carrying the first line of stderr.
pub fn run(argv: &[&str]) -> anyhow::Result<String> {
    run_timeout(argv, None, RUN_TIMEOUT)
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

    #[test]
    fn scanners_follow_the_platform() {
        use crate::platform::Os;
        let sources = |os| -> Vec<SourceId> {
            all_scanners(std::path::Path::new("/tmp"), os)
                .iter()
                .map(|s| s.source())
                .collect()
        };
        let apple = [SourceId::Ios, SourceId::Xcode, SourceId::Homebrew];
        let mac = sources(Os::MacOs);
        assert!(apple.iter().all(|s| mac.contains(s)));
        for os in [Os::Linux, Os::Windows] {
            let found = sources(os);
            assert!(apple.iter().all(|s| !found.contains(s)), "{os:?}");
            assert!(found.contains(&SourceId::Artifacts));
            assert!(found.contains(&SourceId::Docker));
        }
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
    fn run_timeout_kills_slow_commands() {
        let started = std::time::Instant::now();
        let out = run_timeout(&["sleep", "5"], None, std::time::Duration::from_millis(200));
        assert!(out.is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn run_timeout_returns_stdout() {
        let out = run_timeout(
            &["pwd"],
            Some(std::path::Path::new("/")),
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out.trim(), "/");
    }

    #[test]
    fn exec_feeds_a_large_stdin_without_deadlocking() {
        // Writing everything before reading stdout used to block both sides.
        let input = vec![b'x'; 4 * 1024 * 1024];
        let out = exec(
            &["cat"],
            None,
            Some(input.clone()),
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(out.stdout.len(), input.len());
    }

    #[test]
    fn run_timeout_error_carries_the_first_stderr_line() {
        let err = run_timeout(
            &["sh", "-c", "echo 'fatal: boom' >&2; exit 3"],
            None,
            std::time::Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "sh failed: fatal: boom");
    }

    #[test]
    fn which_finds_sh() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-binary-devsweep").is_none());
    }
}
