//! The process snapshot on Windows, through `sysinfo`: no system command
//! tells another process's working directory.

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use super::{InUse, Seen, ancestors};

/// Snapshot of every process except devsweep and its ancestors. A process
/// started elevated, or by another user, shows its name but not its working
/// directory: it still counts by name and locks no path. Seeing nobody's
/// working directory fails closed: devsweep can always read its own.
pub fn collect() -> InUse {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cwd(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );
    let own = ancestors(std::process::id(), &|pid| {
        let process = system.process(Pid::from_u32(pid))?;
        Some((process.parent().map(Pid::as_u32), process.start_time()))
    });
    let seen = system
        .processes()
        .iter()
        .filter(|(pid, _)| !own.contains(&pid.as_u32()))
        .map(|(pid, process)| Seen {
            pid: pid.as_u32(),
            name: process.name().to_string_lossy().into_owned(),
            // A process reports the folder as it was given: through a
            // junction, a `subst` drive, a short name. Items are known by
            // their resolved path, so the comparison needs this one resolved
            // too. A folder that no longer resolves keeps its spelling.
            cwd: process
                .cwd()
                .map(|cwd| dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())),
            argv: process
                .cmd()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
        })
        .collect();
    InUse::from_seen(seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Os;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};

    /// A process that stays in `dir` for a while.
    fn working_in(dir: &Path) -> Child {
        Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .current_dir(dir)
            .stdout(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn stop(mut child: Child) {
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// A real process working in a folder is found there.
    #[test]
    fn own_cwd_is_seen() {
        let d = tempfile::tempdir().unwrap();
        let dir = dunce::canonicalize(d.path()).unwrap();
        let child = working_in(&dir);
        let pid = child.id();
        let inuse = collect().for_os(Os::Windows);
        let lock = inuse.lock_for(&dir);
        stop(child);
        // Windows reports the name as the file is spelled: `PING.EXE`.
        assert_eq!(
            lock.map(|l| l.to_lowercase()),
            Some(format!("ping · pid {pid}"))
        );
        assert_eq!(inuse.lock_for(&dir.join("not-here")), None);
    }

    /// A process reports its working directory the way it was given: through
    /// a junction, or with a short `RUNNER~1` name. Items are found by their
    /// resolved path, and the process must still lock them.
    #[test]
    fn a_process_that_entered_through_a_junction_locks_the_real_folder() {
        let d = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(d.path()).unwrap();
        let (real, junction) = (base.join("real"), base.join("link"));
        std::fs::create_dir_all(&real).unwrap();
        let made = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&real)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        let child = working_in(&junction);
        let lock = collect().for_os(Os::Windows).lock_for(&real);
        stop(child);
        assert!(lock.is_some());
    }

    #[test]
    fn own_process_chain_is_excluded() {
        let inuse = collect();
        assert!(inuse.failed.is_none(), "{:?}", inuse.failed);
        assert!(inuse.procs.iter().all(|p| p.pid != std::process::id()));
    }
}
