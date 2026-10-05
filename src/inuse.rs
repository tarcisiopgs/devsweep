//! Which paths have a live process working inside them: from `lsof` on
//! macOS and from `/proc` on Linux.

pub mod linux;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::platform::Os;

/// Why a system this binary does not run on has no snapshot.
const UNAVAILABLE: &str = "process check is not available on this system";

/// lsof can block on a stale network mount; past this, everything stays locked.
const LSOF_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
struct Proc {
    pid: u32,
    name: String,
    cwd: PathBuf,
    /// Full command line from `ps`, when known.
    args: String,
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
                            args: String::new(),
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

    /// Parse `ps -axo pid=,args=` into command lines by PID.
    pub fn parse_ps(output: &str) -> HashMap<u32, String> {
        output
            .lines()
            .filter_map(|line| {
                let line = line.trim_start();
                let (pid, args) = line.split_once(char::is_whitespace)?;
                Some((pid.parse().ok()?, args.trim().to_string()))
            })
            .collect()
    }

    /// Attach each process's command line, so tools that run under `node`
    /// (npm, npx, pnpm) can be recognised by what they run.
    pub fn with_args(mut self, args: &HashMap<u32, String>) -> InUse {
        for p in &mut self.procs {
            if let Some(a) = args.get(&p.pid) {
                p.args = a.clone();
            }
        }
        self
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

    /// Snapshot of every process cwd, the way `os` exposes it. Asking for a
    /// system other than the one running fails closed.
    pub fn collect(os: Os) -> InUse {
        let parsed = match os {
            _ if os != Os::current() => InUse::failed(UNAVAILABLE),
            Os::MacOs => InUse::collect_lsof(),
            Os::Linux => linux::snapshot(Path::new("/proc"), std::process::id()),
            Os::Windows => InUse::failed(UNAVAILABLE),
        };
        match os.home_dir(&crate::platform::process_env) {
            Some(home) => parsed.with_home(home),
            None => parsed,
        }
    }

    /// The macOS snapshot: `lsof` for the working directories, `ps` for the
    /// command lines.
    fn collect_lsof() -> InUse {
        let output = crate::scan::run_timeout(
            &["lsof", "+c0", "-a", "-d", "cwd", "-Fpcn"],
            None,
            LSOF_TIMEOUT,
        );
        let ps = crate::scan::run_timeout(&["ps", "-axo", "pid=,args="], None, LSOF_TIMEOUT);
        let parsed = match ps {
            Ok(ps) => InUse::from_output(output).with_args(&InUse::parse_ps(&ps)),
            Err(err) => InUse::failed(err.to_string()),
        };
        parsed.excluding(&own_process_chain())
    }

    pub(crate) fn failure(&self) -> Option<String> {
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

    /// Lock reason when a process with one of these names is running: its
    /// own name, or the tool it runs (`npm exec …`, `node …/bin/pnpm`).
    pub fn busy(&self, names: &[&str]) -> Option<String> {
        if let Some(reason) = self.failure() {
            return Some(reason);
        }
        self.procs.iter().find_map(|p| {
            std::iter::once(p.name.as_str())
                .chain(arg_names(&p.args))
                .find(|n| names.contains(n))
                .map(|n| format!("{n} · PID {}", p.pid))
        })
    }

    /// Lock reason when a process command line contains one of `patterns`.
    pub fn args_containing<S: AsRef<str>>(&self, patterns: &[S]) -> Option<String> {
        if let Some(reason) = self.failure() {
            return Some(reason);
        }
        self.procs
            .iter()
            .find(|p| patterns.iter().any(|pat| p.args.contains(pat.as_ref())))
            .map(|p| format!("{} · PID {}", p.name, p.pid))
    }
}

/// Tool names in a command line: the program, and for a script runner
/// (`node`, `bun`, `deno`) the script it runs, without extension or `-cli`.
fn arg_names(args: &str) -> impl Iterator<Item = &str> {
    fn base(t: &str) -> &str {
        let name = t.rsplit('/').next().unwrap_or(t);
        let name = name.split('.').next().unwrap_or(name);
        name.strip_suffix("-cli").unwrap_or(name)
    }
    let mut tokens = args.split_whitespace();
    let first = tokens.next().map(base);
    let script = match first {
        Some("node" | "bun" | "deno") => tokens.next().filter(|t| !t.starts_with('-')).map(base),
        _ => None,
    };
    first.into_iter().chain(script)
}

/// This process and its ancestors (npx, node, the shell…), up to launchd.
pub fn own_process_chain() -> Vec<u32> {
    let mut chain = vec![std::process::id()];
    while let Some(&pid) = chain.last() {
        let parent = crate::scan::run_timeout(
            &["ps", "-o", "ppid=", "-p", &pid.to_string()],
            None,
            LSOF_TIMEOUT,
        )
        .ok()
        .and_then(|out| out.trim().parse::<u32>().ok());
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
    fn busy_matches_tools_running_under_node_by_their_arguments() {
        // lsof names these processes `node` or `npm`; ps knows what they run.
        let iu = InUse::parse("p30\ncnode\nn/Users/u\np31\ncnode\nn/Users/u\n").with_args(
            &InUse::parse_ps(
                "   30 npm exec firecrawl-mcp\n   31 node /opt/homebrew/bin/pnpm install\n",
            ),
        );
        assert_eq!(iu.busy(&["npx", "npm"]).as_deref(), Some("npm · PID 30"));
        assert_eq!(iu.busy(&["pnpm"]).as_deref(), Some("pnpm · PID 31"));
        assert_eq!(iu.busy(&["yarn"]), None);
    }

    #[test]
    fn args_containing_finds_a_running_emulator() {
        let iu = InUse::parse("p7\ncqemu-system-aarch64\nn/\n").with_args(&InUse::parse_ps(
            "7 /sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64 -avd Pixel_8 -netdelay none\n",
        ));
        assert!(iu.args_containing(&["-avd Pixel_8"]).is_some());
        assert!(iu.args_containing(&["-avd Pixel_9"]).is_none());
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

    /// A system this binary cannot inspect is an unknown state, not a free one.
    #[test]
    fn collecting_for_another_system_fails_closed() {
        use crate::platform::Os;
        let other = Os::ALL.into_iter().find(|os| *os != Os::current()).unwrap();
        let inuse = InUse::collect(other);
        assert!(inuse.lock_for(Path::new("/anything")).is_some());
        assert!(inuse.busy(&["anything"]).is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_snapshot_sees_a_process_working_in_a_folder() {
        use crate::platform::Os;
        let d = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(d.path()).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&dir)
            .spawn()
            .unwrap();
        let inuse = InUse::collect(Os::Linux);
        let lock = inuse.lock_for(&dir);
        let own = inuse.lock_for(&std::env::current_dir().unwrap());
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(lock, Some(format!("sleep · PID {}", child.id())));
        // devsweep and what launched it never lock anything.
        assert!(own.is_none_or(|l| !l.contains(&format!("PID {}", std::process::id()))));
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
