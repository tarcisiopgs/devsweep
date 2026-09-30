//! Which paths have a live process working inside them, from `lsof`.

use std::path::{Path, PathBuf};
use std::process::Command;

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
        InUse { procs, home: None }
    }

    pub fn with_home(mut self, home: impl Into<PathBuf>) -> InUse {
        self.home = Some(home.into());
        self
    }

    /// Snapshot of every process cwd. Any failure yields an empty map.
    pub fn collect() -> InUse {
        let output = Command::new("lsof")
            .args(["-a", "-d", "cwd", "-Fpcn"])
            .output();
        let parsed = match output {
            Ok(out) => InUse::parse(&String::from_utf8_lossy(&out.stdout)),
            Err(_) => InUse::default(),
        };
        match std::env::var_os("HOME") {
            Some(home) => parsed.with_home(home),
            None => parsed,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.procs.is_empty()
    }

    /// Lock reason when some process works at or below `path`.
    pub fn lock_for(&self, path: &Path) -> Option<String> {
        self.procs
            .iter()
            .filter(|p| p.cwd != Path::new("/") && Some(&p.cwd) != self.home.as_ref())
            .find(|p| p.cwd.starts_with(path))
            .map(|p| format!("{} · PID {}", p.name, p.pid))
    }

    /// Lock reason when a process with one of these exact names is running.
    pub fn busy(&self, names: &[&str]) -> Option<String> {
        self.procs
            .iter()
            .find(|p| names.contains(&p.name.as_str()))
            .map(|p| format!("{} · PID {}", p.name, p.pid))
    }
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
    fn parses_real_fixture() {
        assert!(!InUse::parse(include_str!("../tests/fixtures/lsof.txt")).is_empty());
    }
}
