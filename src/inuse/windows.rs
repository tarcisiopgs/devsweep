//! The process snapshot on Windows, through `sysinfo`: no system command
//! tells another process's working directory.

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use super::{InUse, Proc};

/// Longest chain of ancestors followed before giving up.
const MAX_ANCESTORS: usize = 32;

/// Snapshot of every process that shows its working directory, except
/// devsweep and its ancestors. Processes of other users, and protected
/// ones, do not show theirs and are skipped. Seeing nobody else's fails
/// closed: devsweep can always read its own.
pub fn collect() -> InUse {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cwd(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );
    let own = own_chain(&system);
    let procs: Vec<Proc> = system
        .processes()
        .iter()
        .filter(|(pid, _)| !own.contains(pid))
        .filter_map(|(pid, process)| {
            let cwd = process.cwd()?.to_path_buf();
            let argv: Vec<String> = process
                .cmd()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            Some(Proc {
                pid: pid.as_u32(),
                name: process.name().to_string_lossy().into_owned(),
                cwd,
                args: argv.join(" "),
                argv,
            })
        })
        .collect();
    if procs.is_empty() {
        return InUse::failed("no other process shows its working directory");
    }
    InUse::from_procs(procs)
}

/// This process and its ancestors (npx, node, the shell…).
fn own_chain(system: &System) -> Vec<Pid> {
    let mut chain = vec![Pid::from_u32(std::process::id())];
    while let Some(&pid) = chain.last() {
        match system.process(pid).and_then(|process| process.parent()) {
            Some(parent) if !chain.contains(&parent) && chain.len() < MAX_ANCESTORS => {
                chain.push(parent)
            }
            _ => break,
        }
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Os;
    use std::process::{Command, Stdio};

    /// A real process working in a folder is found there.
    #[test]
    fn own_cwd_is_seen() {
        let d = tempfile::tempdir().unwrap();
        let dir = dunce::canonicalize(d.path()).unwrap();
        let mut child = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .current_dir(&dir)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let inuse = collect().for_os(Os::Windows);
        let lock = inuse.lock_for(&dir);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(lock, Some(format!("ping · PID {}", child.id())));
        assert_eq!(inuse.lock_for(&dir.join("not-here")), None);
    }

    #[test]
    fn own_process_chain_is_excluded() {
        let inuse = collect();
        assert!(inuse.failed.is_none(), "{:?}", inuse.failed);
        assert!(inuse.procs.iter().all(|p| p.pid != std::process::id()));
    }
}
