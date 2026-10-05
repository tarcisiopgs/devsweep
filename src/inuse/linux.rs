//! The process snapshot on Linux, read straight from `/proc`: `lsof` is not
//! installed on many distributions, and a missing tool would lock everything.

use std::path::{Path, PathBuf};

use super::{InUse, Proc};

/// Longest chain of ancestors followed before giving up.
const MAX_ANCESTORS: usize = 32;

/// Snapshot of every process whose working directory can be read under
/// `root` (`/proc`). Another user's processes do not show theirs and are
/// skipped, like `lsof` does without root. A `root` that cannot be listed,
/// or that shows no working directory at all, fails closed.
pub fn from_proc(root: &Path) -> InUse {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) => return InUse::failed(format!("cannot read {}: {err}", root.display())),
    };
    let procs: Vec<Proc> = entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let dir = entry.path();
            let cwd = std::fs::read_link(dir.join("cwd")).ok()?;
            // `comm` is cut at 15 bytes; `busy` also looks at the command line.
            let name = std::fs::read_to_string(dir.join("comm"))
                .map(|s| s.trim_end().to_string())
                .unwrap_or_default();
            let args = std::fs::read(dir.join("cmdline"))
                .map(|bytes| command_line(&bytes))
                .unwrap_or_default();
            Some(Proc {
                pid,
                name,
                cwd: without_deleted_mark(cwd),
                args,
                argv: Vec::new(),
            })
        })
        .collect();
    if procs.is_empty() {
        return InUse::failed(format!(
            "{} shows no process working directory",
            root.display()
        ));
    }
    InUse::from_procs(procs)
}

/// The snapshot devsweep works with: every process under `root` except
/// `own_pid` and its ancestors, which sit in the scanned folder and must
/// never lock it. devsweep can always read its own working directory, so
/// seeing nobody else's means the others are hidden (a restricted `/proc`),
/// not that nothing runs: that fails closed too.
pub fn snapshot(root: &Path, own_pid: u32) -> InUse {
    let all = from_proc(root);
    if all.failed.is_some() {
        return all;
    }
    let others = all.excluding(&parent_chain(root, own_pid));
    if others.is_empty() {
        return InUse::failed(format!(
            "{} shows no working directory of another process",
            root.display()
        ));
    }
    others
}

/// `cmdline` separates arguments with NUL; one line with spaces, like `ps`.
fn command_line(bytes: &[u8]) -> String {
    bytes
        .split(|b| *b == 0)
        .filter(|arg| !arg.is_empty())
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The kernel appends ` (deleted)` to the target of a removed directory.
fn without_deleted_mark(cwd: PathBuf) -> PathBuf {
    match cwd.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(path) => PathBuf::from(path),
        None => cwd,
    }
}

/// `pid` and its ancestors, from the parent field of `<root>/<pid>/stat`.
pub fn parent_chain(root: &Path, pid: u32) -> Vec<u32> {
    let mut chain = vec![pid];
    while let Some(&pid) = chain.last() {
        match parent_of(root, pid) {
            Some(ppid) if ppid > 1 && !chain.contains(&ppid) && chain.len() < MAX_ANCESTORS => {
                chain.push(ppid)
            }
            _ => break,
        }
    }
    chain
}

/// The process name sits in parentheses and may itself hold spaces and
/// parentheses, so the fields are counted from the last `)`.
fn parent_of(root: &Path, pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(root.join(pid.to_string()).join("stat")).ok()?;
    let (_, after_name) = stat.rsplit_once(')')?;
    after_name.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// A fake `/proc/<pid>`: `cwd` is a symlink, like the real one.
    fn process(root: &Path, pid: u32, cwd: Option<&Path>, comm: &str, cmdline: &[u8]) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        if let Some(cwd) = cwd {
            std::os::unix::fs::symlink(cwd, dir.join("cwd")).unwrap();
        }
        fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
        fs::write(dir.join("cmdline"), cmdline).unwrap();
    }

    fn stat(root: &Path, pid: u32, comm: &str, ppid: u32) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        let line = format!("{pid} ({comm}) S {ppid} {pid} {pid} 0 -1 4194304 100\n");
        fs::write(dir.join("stat"), line).unwrap();
    }

    #[test]
    fn reads_cwd_name_and_args() {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join("proj");
        process(
            d.path(),
            101,
            Some(&proj),
            "node",
            b"node\0/usr/bin/pnpm\0dev\0",
        );
        let inuse = from_proc(d.path());
        assert_eq!(inuse.lock_for(&proj).as_deref(), Some("node · PID 101"));
        assert_eq!(inuse.busy(&["pnpm"]).as_deref(), Some("pnpm · PID 101"));
        assert_eq!(
            inuse.args_containing(&["pnpm dev"]).as_deref(),
            Some("node · PID 101")
        );
        assert_eq!(inuse.lock_for(&d.path().join("other")), None);
    }

    #[test]
    fn unreadable_proc_root_locks_everything() {
        let inuse = from_proc(Path::new("/nonexistent/devsweep/proc"));
        let lock = inuse.lock_for(Path::new("/anything")).unwrap();
        assert!(lock.starts_with("process check failed"), "{lock}");
        assert!(inuse.busy(&["anything"]).is_some());
    }

    #[test]
    fn no_readable_cwd_locks_everything() {
        let d = tempfile::tempdir().unwrap();
        process(d.path(), 101, None, "node", b"node\0");
        let inuse = from_proc(d.path());
        let lock = inuse.lock_for(Path::new("/anything")).unwrap();
        assert!(lock.starts_with("process check failed"), "{lock}");
    }

    #[test]
    fn process_without_cwd_is_skipped_when_others_are_readable() {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join("proj");
        process(d.path(), 101, Some(&proj), "node", b"node\0");
        process(d.path(), 102, None, "sshd", b"sshd\0");
        let inuse = from_proc(d.path());
        assert!(inuse.lock_for(&proj).is_some());
        assert_eq!(inuse.busy(&["sshd"]), None);
    }

    #[test]
    fn entries_that_are_not_processes_are_ignored() {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join("proj");
        process(d.path(), 101, Some(&proj), "node", b"node\0");
        fs::create_dir_all(d.path().join("self")).unwrap();
        fs::write(d.path().join("uptime"), "1 1\n").unwrap();
        assert!(from_proc(d.path()).lock_for(&proj).is_some());
    }

    #[test]
    fn deleted_cwd_keeps_its_path() {
        let d = tempfile::tempdir().unwrap();
        let gone = d.path().join("gone");
        let marked = d.path().join("gone (deleted)");
        process(d.path(), 101, Some(&marked), "node", b"node\0");
        assert!(from_proc(d.path()).lock_for(&gone).is_some());
    }

    /// The kernel cuts `comm` at 15 bytes; the full name is in `cmdline`.
    #[test]
    fn long_process_name_is_matched_through_its_command_line() {
        let d = tempfile::tempdir().unwrap();
        process(
            d.path(),
            101,
            Some(&d.path().join("w")),
            "chrome_crashpad",
            b"/opt/chrome/chrome_crashpad_handler\0--monitor\0",
        );
        assert!(
            from_proc(d.path())
                .busy(&["chrome_crashpad_handler"])
                .is_some()
        );
    }

    /// devsweep can always read its own working directory, so a `/proc`
    /// that hides everybody else's still shows a few: its own chain. That
    /// is no evidence that nothing else is running.
    #[test]
    fn seeing_only_its_own_chain_locks_everything() {
        let d = tempfile::tempdir().unwrap();
        let cwd = d.path().join("w");
        process(d.path(), 300, Some(&cwd), "devsweep", b"devsweep\0");
        stat(d.path(), 300, "devsweep", 200);
        process(d.path(), 200, Some(&cwd), "bash", b"bash\0");
        stat(d.path(), 200, "bash", 1);
        process(d.path(), 400, None, "node", b"node\0");
        let inuse = snapshot(d.path(), 300);
        let lock = inuse.lock_for(Path::new("/anything")).unwrap();
        assert!(lock.starts_with("process check failed"), "{lock}");
        assert!(inuse.busy(&["anything"]).is_some());
    }

    #[test]
    fn snapshot_leaves_its_own_chain_out() {
        let d = tempfile::tempdir().unwrap();
        let (own, other) = (d.path().join("own"), d.path().join("other"));
        process(d.path(), 300, Some(&own), "devsweep", b"devsweep\0");
        stat(d.path(), 300, "devsweep", 200);
        process(d.path(), 200, Some(&own), "bash", b"bash\0");
        stat(d.path(), 200, "bash", 1);
        process(d.path(), 101, Some(&other), "node", b"node\0");
        let inuse = snapshot(d.path(), 300);
        assert_eq!(inuse.lock_for(&own), None);
        assert_eq!(inuse.lock_for(&other).as_deref(), Some("node · PID 101"));
    }

    #[test]
    fn parent_chain_follows_ppid() {
        let d = tempfile::tempdir().unwrap();
        // A name with spaces and parentheses: the parent comes after the
        // last `)`, not the first.
        stat(d.path(), 300, "(a b) c", 200);
        stat(d.path(), 200, "bash", 1);
        assert_eq!(parent_chain(d.path(), 300), [300, 200]);
    }

    #[test]
    fn parent_chain_stops_at_an_unreadable_parent() {
        let d = tempfile::tempdir().unwrap();
        stat(d.path(), 300, "x", 200);
        assert_eq!(parent_chain(d.path(), 300), [300, 200]);
        assert_eq!(parent_chain(d.path(), 999), [999]);
    }

    #[test]
    fn parent_chain_survives_a_loop() {
        let d = tempfile::tempdir().unwrap();
        stat(d.path(), 300, "x", 200);
        stat(d.path(), 200, "y", 300);
        assert_eq!(parent_chain(d.path(), 300), [300, 200]);
    }
}
