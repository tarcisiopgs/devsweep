//! Which paths have a live process working inside them: from `lsof` on
//! macOS and from `/proc` on Linux.

pub mod linux;
#[cfg(windows)]
pub mod windows;

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
    /// The same command line as separate arguments, where the system
    /// hands it over that way (a program path may hold spaces).
    argv: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct InUse {
    procs: Vec<Proc>,
    home: Option<PathBuf>,
    /// Why the process snapshot could not be taken. When set, every path and
    /// every process name counts as busy: an unknown state never unlocks.
    failed: Option<String>,
    /// Compare paths and names the way Windows does: no case, either
    /// separator, no `.exe`.
    windows: bool,
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
                            argv: Vec::new(),
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
            #[cfg(windows)]
            Os::Windows => windows::collect(),
            #[cfg(not(windows))]
            Os::Windows => InUse::failed(UNAVAILABLE),
        };
        let parsed = parsed.for_os(os);
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
        let at_home = |cwd: &Path| match &self.home {
            Some(home) if self.windows => windows_key(cwd) == windows_key(home),
            Some(home) => cwd == home,
            None => false,
        };
        let within = |cwd: &Path| {
            if self.windows {
                windows_within(cwd, path)
            } else {
                cwd.starts_with(path)
            }
        };
        self.procs
            .iter()
            // An empty working directory is one the system did not show.
            .filter(|p| !p.cwd.as_os_str().is_empty())
            .filter(|p| p.cwd != Path::new("/") && !at_home(&p.cwd))
            .find(|p| within(&p.cwd))
            .map(|p| format!("{} · PID {}", p.name, p.pid))
    }

    /// Lock reason when a process with one of these names is running: its
    /// own name, or the tool it runs (`npm exec …`, `node …/bin/pnpm`).
    pub fn busy(&self, names: &[&str]) -> Option<String> {
        if let Some(reason) = self.failure() {
            return Some(reason);
        }
        let wanted = |candidate: &str| {
            names.iter().copied().find(|name| {
                if self.windows {
                    name.eq_ignore_ascii_case(candidate)
                } else {
                    *name == candidate
                }
            })
        };
        self.procs.iter().find_map(|p| {
            std::iter::once(p.name.as_str())
                .chain(arg_names(p, self.windows))
                .find_map(wanted)
                .map(|name| format!("{name} · PID {}", p.pid))
        })
    }

    /// A snapshot of what a system API listed. A process that hides its
    /// working directory still counts by name, with an empty one that locks
    /// no path. When none shows a working directory, nothing is known about
    /// where anybody works, and that fails closed.
    #[cfg(any(windows, test))]
    fn from_seen(seen: Vec<Seen>) -> InUse {
        if !seen.iter().any(|s| s.cwd.is_some()) {
            return InUse::failed("no other process shows its working directory");
        }
        InUse::from_procs(
            seen.into_iter()
                .map(|s| Proc {
                    pid: s.pid,
                    name: s.name,
                    cwd: s.cwd.unwrap_or_default(),
                    args: s.argv.join(" "),
                    argv: s.argv,
                })
                .collect(),
        )
    }

    /// A snapshot of these processes.
    fn from_procs(mut procs: Vec<Proc>) -> InUse {
        procs.sort_by_key(|p| p.pid);
        InUse {
            procs,
            ..InUse::default()
        }
    }

    /// Compare paths and process names the way `os` does. On Windows a
    /// process is known without its `.exe`.
    pub fn for_os(mut self, os: Os) -> InUse {
        self.windows = os == Os::Windows;
        if self.windows {
            for p in &mut self.procs {
                let stem = p.name.len().saturating_sub(4);
                if p.name.is_char_boundary(stem) && p.name[stem..].eq_ignore_ascii_case(".exe") {
                    p.name.truncate(stem);
                }
            }
        }
        self
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
fn arg_names(p: &Proc, windows: bool) -> impl Iterator<Item = &str> {
    fn base(t: &str, windows: bool) -> &str {
        let name = t.rsplit('/').next().unwrap_or(t);
        let name = match windows {
            true => name.rsplit('\\').next().unwrap_or(name),
            false => name,
        };
        let name = name.split('.').next().unwrap_or(name);
        name.strip_suffix("-cli").unwrap_or(name)
    }
    // Separate arguments when the system gave them; a program path with a
    // space in it would otherwise be cut in two.
    let mut tokens: Box<dyn Iterator<Item = &str>> = if p.argv.is_empty() {
        Box::new(p.args.split_whitespace())
    } else {
        Box::new(p.argv.iter().map(String::as_str))
    };
    let first = tokens.next().map(|t| base(t, windows));
    let runs_scripts = first.is_some_and(|name| {
        ["node", "bun", "deno"].iter().any(|runner| match windows {
            true => runner.eq_ignore_ascii_case(name),
            false => *runner == name,
        })
    });
    let script = match runs_scripts {
        true => tokens
            .next()
            .filter(|t| !t.starts_with('-'))
            .map(|t| base(t, windows)),
        false => None,
    };
    first.into_iter().chain(script)
}

/// A Windows path in the one spelling used to compare it: no verbatim
/// prefix, backslashes, lower case, no trailing separator.
fn windows_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\").to_lowercase();
    // A share resolves to `\\?\UNC\server\share`, the same place as
    // `\\server\share`.
    let plain = match text.strip_prefix("\\\\?\\unc\\") {
        Some(share) => format!("\\\\{share}"),
        None => text.strip_prefix("\\\\?\\").unwrap_or(&text).to_string(),
    };
    plain.trim_end_matches('\\').to_string()
}

/// A process as the system's API reports it. `cwd` is `None` when the
/// system would not tell (on Windows, a process started elevated).
#[cfg(any(windows, test))]
struct Seen {
    pid: u32,
    name: String,
    cwd: Option<PathBuf>,
    argv: Vec<String>,
}

/// `start` and its ancestors. `info` gives a process's parent and the time
/// it started. A "parent" that started after its child is another process
/// that was handed the PID of the real one, and ends the chain.
#[cfg(any(windows, test))]
fn ancestors(start: u32, info: &dyn Fn(u32) -> Option<(Option<u32>, u64)>) -> Vec<u32> {
    const MAX_ANCESTORS: usize = 32;
    let mut chain = vec![start];
    while let Some(&pid) = chain.last() {
        let Some((Some(parent), started)) = info(pid) else {
            break;
        };
        let reused = info(parent).is_some_and(|(_, parent_started)| parent_started > started);
        if reused || chain.contains(&parent) || chain.len() >= MAX_ANCESTORS {
            break;
        }
        chain.push(parent);
    }
    chain
}

/// Whether `cwd` is `path` or a folder below it, on Windows.
fn windows_within(cwd: &Path, path: &Path) -> bool {
    let (cwd, path) = (windows_key(cwd), windows_key(path));
    cwd == path
        || cwd
            .strip_prefix(&path)
            .is_some_and(|rest| rest.starts_with('\\'))
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

    fn proc(pid: u32, name: &str, cwd: &str, argv: &[&str]) -> Proc {
        Proc {
            pid,
            name: name.to_string(),
            cwd: PathBuf::from(cwd),
            args: argv.join(" "),
            argv: argv.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn windows(procs: Vec<Proc>) -> InUse {
        InUse::from_procs(procs).for_os(Os::Windows)
    }

    /// Windows paths do not care about case: the scan and the process may
    /// spell the same folder differently.
    #[test]
    fn windows_paths_match_ignoring_case() {
        let iu = windows(vec![proc(7, "node.exe", "c:\\users\\u\\proj\\app", &[])]);
        assert_eq!(
            iu.lock_for(Path::new("C:\\Users\\U\\proj")).as_deref(),
            Some("node · PID 7")
        );
        assert_eq!(iu.lock_for(Path::new("C:\\Users\\U\\other")), None);
    }

    #[test]
    fn windows_paths_match_across_separators() {
        let iu = windows(vec![proc(7, "node.exe", "C:/Users/u/proj/app", &[])]);
        assert!(iu.lock_for(Path::new("C:\\Users\\u\\proj")).is_some());
        let iu = windows(vec![proc(7, "node.exe", "C:\\Users\\u\\proj", &[])]);
        assert!(iu.lock_for(Path::new("C:/Users/u/proj/")).is_some());
    }

    #[test]
    fn windows_verbatim_and_plain_paths_are_the_same_folder() {
        let iu = windows(vec![proc(7, "node.exe", "\\\\?\\C:\\Users\\u\\proj", &[])]);
        assert!(iu.lock_for(Path::new("C:\\Users\\u\\proj")).is_some());
    }

    /// A folder on a network share has two spellings, and resolving a path
    /// yields the verbatim one.
    #[test]
    fn windows_network_paths_match_in_either_spelling() {
        let iu = windows(vec![proc(7, "node.exe", "\\\\?\\UNC\\nas\\code\\app", &[])]);
        assert!(iu.lock_for(Path::new("\\\\nas\\code")).is_some());
        let iu = windows(vec![proc(7, "node.exe", "\\\\NAS\\code\\app", &[])]);
        assert!(iu.lock_for(Path::new("\\\\?\\UNC\\nas\\code")).is_some());
        assert_eq!(iu.lock_for(Path::new("\\\\nas\\other")), None);
    }

    /// Windows hides the working directory of a process started elevated,
    /// but not its name: `npm` running there still makes the npm cache busy.
    #[test]
    fn a_process_that_hides_its_cwd_still_counts_by_name() {
        let iu = InUse::from_seen(vec![
            seen(7, "node.exe", Some("C:\\x"), &[]),
            seen(8, "npm.exe", None, &[]),
        ])
        .for_os(Os::Windows);
        assert_eq!(iu.busy(&["npm"]).as_deref(), Some("npm · PID 8"));
        assert_eq!(iu.lock_for(Path::new("C:\\")), Some("node · PID 7".into()));
        assert_eq!(iu.lock_for(Path::new("C:\\elsewhere")), None);
    }

    /// Names alone say nothing about where anybody works.
    #[test]
    fn seeing_no_working_directory_at_all_locks_everything() {
        let iu = InUse::from_seen(vec![seen(8, "npm.exe", None, &[])]).for_os(Os::Windows);
        let lock = iu.lock_for(Path::new("C:\\anything")).unwrap();
        assert!(lock.starts_with("process check failed"), "{lock}");
        assert!(InUse::from_seen(vec![]).busy(&["x"]).is_some());
    }

    fn seen(pid: u32, name: &str, cwd: Option<&str>, argv: &[&str]) -> Seen {
        Seen {
            pid,
            name: name.to_string(),
            cwd: cwd.map(PathBuf::from),
            argv: argv.iter().map(|a| a.to_string()).collect(),
        }
    }

    #[test]
    fn ancestors_follow_parents() {
        let table = |pid: u32| match pid {
            30 => Some((Some(20), 300)),
            20 => Some((Some(10), 200)),
            10 => Some((None, 100)),
            _ => None,
        };
        assert_eq!(ancestors(30, &table), [30, 20, 10]);
    }

    /// Windows keeps the PID of a parent that exited, and gives that PID
    /// to a new process later. Something that started after its "child" is
    /// not its parent, and must stay in the snapshot.
    #[test]
    fn a_reused_parent_pid_is_not_an_ancestor() {
        let table = |pid: u32| match pid {
            30 => Some((Some(20), 300)),
            20 => Some((Some(10), 200)),
            10 => Some((Some(5), 900)),
            _ => None,
        };
        assert_eq!(ancestors(30, &table), [30, 20]);
    }

    #[test]
    fn ancestors_survive_a_loop_and_a_missing_parent() {
        let looped = |pid: u32| match pid {
            30 => Some((Some(20), 300)),
            20 => Some((Some(30), 200)),
            _ => None,
        };
        assert_eq!(ancestors(30, &looped), [30, 20]);
        let orphan = |pid: u32| (pid == 30).then_some((Some(20), 300));
        assert_eq!(ancestors(30, &orphan), [30, 20]);
    }

    #[test]
    fn windows_sibling_prefix_does_not_lock() {
        let iu = windows(vec![proc(7, "node.exe", "C:\\Users\\u\\app-2", &[])]);
        assert_eq!(iu.lock_for(Path::new("C:\\Users\\u\\app")), None);
    }

    #[test]
    fn windows_home_cwd_locks_nothing() {
        let iu = windows(vec![proc(7, "pwsh.exe", "c:\\users\\u", &[])]).with_home("C:\\Users\\u");
        assert_eq!(iu.lock_for(Path::new("C:\\Users\\u")), None);
    }

    #[test]
    fn windows_process_names_drop_exe() {
        let iu = windows(vec![proc(7, "Java.EXE", "C:\\x", &[])]);
        assert_eq!(iu.busy(&["java"]).as_deref(), Some("java · PID 7"));
        assert_eq!(iu.busy(&["jav"]), None);
    }

    /// The program path has a space in it; the arguments arrive as a list,
    /// so the script it runs is still the second one.
    #[test]
    fn windows_script_runner_is_recognised() {
        let iu = windows(vec![proc(
            7,
            "node.exe",
            "C:\\x",
            &[
                "C:\\Program Files\\nodejs\\node.exe",
                "C:\\x\\node_modules\\pnpm\\bin\\pnpm.cjs",
                "dev",
            ],
        )]);
        assert_eq!(iu.busy(&["pnpm"]).as_deref(), Some("pnpm · PID 7"));
        assert_eq!(iu.busy(&["yarn"]), None);
    }

    #[test]
    fn unix_paths_stay_case_sensitive() {
        let iu = InUse::from_procs(vec![proc(7, "node", "/home/u/Proj", &[])]).for_os(Os::Linux);
        assert_eq!(iu.lock_for(Path::new("/home/u/proj")), None);
        assert_eq!(iu.busy(&["Node"]), None);
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

    // The chain behind the macOS snapshot is read with `ps`.
    #[cfg(unix)]
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
