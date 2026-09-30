//! Which paths have a live process working inside them, from `lsof`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// lsof can block on a stale network mount; past this, everything stays locked.
const LSOF_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
struct Proc {
    pid: u32,
    name: String,
    cwd: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct InUse {
    procs: Vec<Proc>,
    home: Option<PathBuf>,
    /// Why the process snapshot could not be taken. When set, every path and
    /// every process name counts as busy: an unknown state never unlocks.
    failed: Option<String>,
}

impl InUse {
    /// Parse `lsof -Fpcn` field output (`p<pid>`, `c<command>`, `n<path>`).
    pub fn parse(output: &str) -> InUse {
        let mut procs = Vec::new();
        let (mut pid, mut name) = (None, String::new());
        for line in output.lines() {
            let (tag, value) = match line.split_at_checked(1) {
                Some(split) => split,
                None => continue,
            };
            match tag {
                "p" => {
                    pid = value.parse().ok();
                    name.clear();
                }
                "c" => name = value.to_string(),
                "n" => {
                    if let Some(pid) = pid {
                        procs.push(Proc {
                            pid,
                            name: name.clone(),
                            cwd: PathBuf::from(value),
                        });
                    }
                }
                _ => {}
            }
        }
        procs.sort_by_key(|p| p.pid);
        InUse {
            procs,
            ..InUse::default()
        }
    }

    /// Drop processes that must never lock anything (devsweep itself and the
    /// shell that launched it, which usually sits in the scanned folder).
    pub fn excluding(mut self, pids: &[u32]) -> InUse {
        self.procs.retain(|p| !pids.contains(&p.pid));
        self
    }

    pub fn with_home(mut self, home: impl Into<PathBuf>) -> InUse {
        self.home = Some(home.into());
        self
    }

    /// A snapshot that could not be taken; it locks everything.
    pub fn failed(reason: impl Into<String>) -> InUse {
        InUse {
            failed: Some(reason.into()),
            ..InUse::default()
        }
    }

    /// Parse the result of running lsof. An error or an output without any
    /// process fails closed.
    pub fn from_output(output: anyhow::Result<String>) -> InUse {
        match output {
            Ok(out) => {
                let parsed = InUse::parse(&out);
                if parsed.is_empty() {
                    InUse::failed("lsof listed no process")
                } else {
                    parsed
                }
            }
            Err(err) => InUse::failed(err.to_string()),
        }
    }

    /// Snapshot of every process cwd.
    pub fn collect() -> InUse {
        let output = crate::scan::run_timeout(
            &["lsof", "+c0", "-a", "-d", "cwd", "-Fpcn"],
            None,
            LSOF_TIMEOUT,
        );
        let parsed = InUse::from_output(output).excluding(&own_process_chain());
        match std::env::var_os("HOME") {
            Some(home) => parsed.with_home(home),
            None => parsed,
        }
    }

    fn failure(&self) -> Option<String> {
        self.failed
            .as_ref()
            .map(|reason| format!("process check failed: {reason}"))
    }

    pub fn is_empty(&self) -> bool {
        self.procs.is_empty()
    }

    /// Lock reason when some process works at or below `path`.
    pub fn lock_for(&self, path: &Path) -> Option<String> {
        if let Some(reason) = self.failure() {
            return Some(reason);
        }
        self.procs
            .iter()
            .filter(|p| p.cwd != Path::new("/") && Some(&p.cwd) != self.home.as_ref())
            .find(|p| p.cwd.starts_with(path))
            .map(|p| format!("{} · PID {}", p.name, p.pid))
    }

    /// Lock reason when a process with one of these exact names is running.
    pub fn busy(&self, names: &[&str]) -> Option<String> {
        if let Some(reason) = self.failure() {
            return Some(reason);
        }
        self.procs
            .iter()
            .find(|p| names.contains(&p.name.as_str()))
            .map(|p| format!("{} · PID {}", p.name, p.pid))
    }
}

/// This process and its ancestors (npx, node, the shell…), up to launchd.
pub fn own_process_chain() -> Vec<u32> {
    let mut chain = vec![std::process::id()];
    while let Some(&pid) = chain.last() {
        let parent = Command::new("ps")
            .args(["-o", "ppid=", "-p", &pid.to_string()])
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<u32>()
                    .ok()
            });
        match parent {
            Some(ppid) if ppid > 1 && !chain.contains(&ppid) && chain.len() < 32 => {
                chain.push(ppid)
            }
            _ => break,
        }
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn lock_for_path_inside_process_cwd() {
        let iu = InUse::parse("p4821\ncclaude\nn/Users/u/orca/ws/site/arowana/src\n");
        assert_eq!(
            iu.lock_for(Path::new("/Users/u/orca/ws/site/arowana"))
                .as_deref(),
            Some("claude · PID 4821")
        );
    }

    #[test]
    fn no_lock_for_sibling_prefix() {
        let iu = InUse::parse("p1\ncnode\nn/Users/u/app-2\n");
        assert_eq!(iu.lock_for(Path::new("/Users/u/app")), None);
    }

    #[test]
    fn root_cwd_locks_nothing() {
        let iu = InUse::parse("p1\ncsh\nn/\n");
        assert_eq!(iu.lock_for(Path::new("/Users/u/x")), None);
    }

    #[test]
    fn home_cwd_locks_nothing() {
        let iu = InUse::parse("p1\nczsh\nn/Users/u\n").with_home("/Users/u");
        assert_eq!(iu.lock_for(Path::new("/Users/u")), None);
    }

    #[test]
    fn lowest_pid_wins() {
        let iu = InUse::parse("p9\ncnode\nn/Users/u/a\np3\nczsh\nn/Users/u/a/b\n");
        assert_eq!(
            iu.lock_for(Path::new("/Users/u/a")).as_deref(),
            Some("zsh · PID 3")
        );
    }

    #[test]
    fn busy_matches_process_name_exactly() {
        let iu = InUse::parse("p77\ncpnpm\nn/Users/u/app\n");
        assert_eq!(iu.busy(&["pnpm"]).as_deref(), Some("pnpm · PID 77"));
        assert_eq!(iu.busy(&["pn"]), None);
    }

    #[test]
    fn excluded_pids_lock_nothing() {
        let iu = InUse::parse("p10\nczsh\nn/Users/u/app\n").excluding(&[10]);
        assert_eq!(iu.lock_for(Path::new("/Users/u/app")), None);
    }

    #[test]
    fn failed_process_check_locks_every_path_and_name() {
        let iu = InUse::failed("lsof timed out");
        assert_eq!(
            iu.lock_for(Path::new("/Users/u/app")).as_deref(),
            Some("process check failed: lsof timed out")
        );
        assert!(iu.busy(&["pnpm"]).is_some());
    }

    #[test]
    fn empty_lsof_output_counts_as_a_failed_check() {
        let iu = InUse::from_output(Ok(String::new()));
        assert!(iu.lock_for(Path::new("/Users/u/app")).is_some());
    }

    #[test]
    fn own_ancestors_include_parent() {
        let pids = own_process_chain();
        assert_eq!(pids[0], std::process::id());
        assert!(pids.len() >= 2);
    }

    #[test]
    fn parses_real_fixture() {
        assert!(!InUse::parse(include_str!("../tests/fixtures/lsof.txt")).is_empty());
    }
}
