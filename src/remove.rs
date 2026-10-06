//! Removing what the user confirmed, one item at a time, with a last check
//! before each deletion.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crossbeam_channel::Sender;

use crate::inuse::InUse;
use crate::model::{Item, ItemId, LeftoverBranch, Removal, SourceId, Status};

pub trait Executor: Send + Sync {
    fn remove_dir(&self, p: &Path) -> Result<(), String>;
    fn clear_dir(&self, p: &Path) -> Result<(), String>;
    fn remove_file(&self, p: &Path) -> Result<(), String>;
    fn command(&self, argv: &[String], cwd: Option<&Path>) -> Result<(), String>;
}

/// Executes removals for real.
pub struct RealExecutor;

impl Executor for RealExecutor {
    fn remove_dir(&self, p: &Path) -> Result<(), String> {
        // remove_dir_all deletes symlinks themselves, never their targets.
        std::fs::remove_dir_all(p).map_err(|e| e.to_string())
    }

    fn clear_dir(&self, p: &Path) -> Result<(), String> {
        for entry in std::fs::read_dir(p).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            let kind = entry.file_type().ok();
            let result = if kind.is_some_and(|t| t.is_dir()) {
                std::fs::remove_dir_all(&path)
            } else if kind.is_some_and(|t| t.is_symlink()) {
                // The link itself, never what it points at. Windows removes
                // a link to a folder (a junction) only as a folder.
                std::fs::remove_file(&path).or_else(|_| std::fs::remove_dir(&path))
            } else {
                std::fs::remove_file(&path)
            };
            result.map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(())
    }

    fn remove_file(&self, p: &Path) -> Result<(), String> {
        std::fs::remove_file(p).map_err(|e| e.to_string())
    }

    fn command(&self, argv: &[String], cwd: Option<&Path>) -> Result<(), String> {
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        crate::scan::run_timeout(&argv, cwd, REMOVAL_TIMEOUT)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Longest a native removal command may run (a large `docker system prune`
/// or runtime delete is slow, a wedged daemon is not coming back).
const REMOVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Last line of defence against a scanner bug: directory removals must stay
/// inside the home folder, the scanned folder or a known extra root, and
/// never hit one of those roots or a protected folder itself.
pub struct Guard {
    roots: Vec<PathBuf>,
    protected: Vec<PathBuf>,
}

fn canon(p: PathBuf) -> PathBuf {
    crate::platform::canonical(&p).unwrap_or(p)
}

impl Guard {
    pub fn new(
        home: PathBuf,
        target: PathBuf,
        protected: Vec<PathBuf>,
        extra_roots: Vec<PathBuf>,
    ) -> Guard {
        let (home, target) = (canon(home), canon(target));
        // A target above the home folder (`/`, `/Users`) must not widen the roots.
        let target = (!home.starts_with(&target)).then_some(target);
        // The same goes for an extra root, judged by where it resolves.
        let extra: Vec<PathBuf> = extra_roots
            .into_iter()
            .map(canon)
            .filter(|root| !home.starts_with(root))
            .collect();
        let roots: Vec<PathBuf> = std::iter::once(home).chain(target).chain(extra).collect();
        let protected = protected.into_iter().map(canon).collect();
        Guard { roots, protected }
    }

    pub fn check(&self, p: &Path) -> Result<(), RemoveError> {
        if !p.is_absolute() {
            return Err(RemoveError::Refused("relative path"));
        }
        // The checks below look at where `p` resolves; the removal then acts
        // through `p`. A symlink would make those two different folders.
        if std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(RemoveError::Refused("symlink"));
        }
        let real = crate::platform::canonical(p).map_err(|_| RemoveError::Changed)?;
        if !self.roots.iter().any(|root| real.starts_with(root)) {
            return Err(RemoveError::Refused("outside allowed folders"));
        }
        if self.roots.contains(&real) || self.protected.contains(&real) {
            return Err(RemoveError::Refused("protected folder"));
        }
        Ok(())
    }
}

/// Why an item was left alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoveError {
    /// It is no longer what the scan saw.
    Changed,
    /// The user quit before its turn.
    Skipped,
    /// The guard refused the path, for this reason.
    Refused(&'static str),
    /// The tool or the filesystem failed, in its own words.
    Failed(String),
}

impl std::fmt::Display for RemoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoveError::Changed => f.write_str("changed since scan"),
            RemoveError::Skipped => f.write_str("skipped: you quit"),
            RemoveError::Refused(why) => write!(f, "refused: {why}"),
            RemoveError::Failed(msg) => f.write_str(msg),
        }
    }
}

pub enum RemoveEvent {
    Started(ItemId),
    Ok(ItemId, u64),
    Err(ItemId, RemoveError),
    /// A worktree just removed left this merged branch in its repository.
    Leftover(LeftoverBranch),
    Finished,
}

/// The repository and the worktree of a worktree removal.
fn worktree_remove(removal: &Removal) -> Option<(&Path, &Path)> {
    match removal {
        Removal::Worktree { path, repo } => Some((repo.as_path(), path.as_path())),
        _ => None,
    }
}

/// Answers which merged branch a worktree (first argument) would leave in
/// its repository (second) once removed.
pub type Leftover<'a> = &'a dyn Fn(&Path, &Path) -> Option<LeftoverBranch>;

/// Every path passes the guard before any of them is touched.
fn remove_paths(paths: &[PathBuf], exec: &dyn Executor, guard: &Guard) -> Result<(), RemoveError> {
    paths.iter().try_for_each(|p| guard.check(p))?;
    paths.iter().try_for_each(|p| {
        // symlink_metadata: a symlink is removed as a file, never followed.
        if std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()) {
            exec.remove_dir(p)
        } else {
            exec.remove_file(p)
        }
        .map_err(RemoveError::Failed)
    })
}

/// Remove every item in order. A failure never stops the batch.
pub fn run_removals(
    items: &[Item],
    exec: &dyn Executor,
    guard: &Guard,
    recheck: &dyn Fn(&Item) -> Result<(), RemoveError>,
    leftover: Leftover,
    tx: Sender<RemoveEvent>,
) {
    let never = std::sync::atomic::AtomicBool::new(false);
    run_removals_until(items, exec, guard, recheck, leftover, &never, tx);
}

/// Like [`run_removals`], skipping every item not started yet once `stop`
/// is set. The item in progress always finishes: stopping a removal halfway
/// would leave it half deleted.
pub fn run_removals_until(
    items: &[Item],
    exec: &dyn Executor,
    guard: &Guard,
    recheck: &dyn Fn(&Item) -> Result<(), RemoveError>,
    leftover: Leftover,
    stop: &std::sync::atomic::AtomicBool,
    tx: Sender<RemoveEvent>,
) {
    let mut repos_to_prune = BTreeSet::new();
    for item in items {
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = tx.send(RemoveEvent::Err(item.id, RemoveError::Skipped));
            continue;
        }
        let _ = tx.send(RemoveEvent::Started(item.id));
        let checked = recheck(item);
        // Read while the worktree still exists, reported only once it is gone.
        let branch = match (&checked, worktree_remove(&item.removal)) {
            (Ok(()), Some((repo, wt))) => leftover(wt, repo),
            _ => None,
        };
        let result = checked.and_then(|()| match &item.removal {
            Removal::RemoveDir(p) => guard
                .check(p)
                .and_then(|()| exec.remove_dir(p).map_err(RemoveError::Failed)),
            Removal::ClearDir(p) => guard
                .check(p)
                .and_then(|()| exec.clear_dir(p).map_err(RemoveError::Failed)),
            Removal::RemovePaths(paths) => remove_paths(paths, exec, guard),
            Removal::Worktree { path, repo } => exec
                .command(
                    &[
                        "git".into(),
                        "worktree".into(),
                        "remove".into(),
                        path.display().to_string(),
                    ],
                    Some(repo),
                )
                .map_err(RemoveError::Failed),
            Removal::Command { argv, cwd } => exec
                .command(argv, cwd.as_deref())
                .map_err(RemoveError::Failed),
        });
        match result {
            Ok(()) => {
                if let Some((repo, _)) = worktree_remove(&item.removal) {
                    repos_to_prune.insert(repo.to_path_buf());
                }
                let _ = tx.send(RemoveEvent::Ok(item.id, item.size.unwrap_or(0)));
                if let Some(branch) = branch {
                    let _ = tx.send(RemoveEvent::Leftover(branch));
                }
            }
            Err(msg) => {
                let _ = tx.send(RemoveEvent::Err(item.id, msg));
            }
        }
    }
    for repo in repos_to_prune {
        let _ = exec.command(
            &["git".into(), "worktree".into(), "prune".into()],
            Some(&repo),
        );
    }
    let _ = tx.send(RemoveEvent::Finished);
}

/// Longest the recheck waits on git before refusing the item.
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Re-check an item right before removing it: its folder still exists, no
/// process moved into it, and a clean worktree is still clean.
pub fn default_recheck(snapshot: impl Fn() -> InUse) -> impl Fn(&Item) -> Result<(), RemoveError> {
    move |item: &Item| {
        let changed = || Err(RemoveError::Changed);
        // A fresh snapshot per item: a long batch must not decide on
        // processes as they were when it started.
        let inuse = snapshot();
        let busy: Vec<&str> = item.recheck.busy.iter().map(String::as_str).collect();
        if !busy.is_empty() && inuse.busy(&busy).is_some() {
            return changed();
        }
        if !item.recheck.args.is_empty() && inuse.args_containing(&item.recheck.args).is_some() {
            return changed();
        }
        if let Some((argv, needle)) = &item.recheck.probe {
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            match crate::scan::run_timeout(&argv, None, GIT_TIMEOUT) {
                Ok(out) if out.contains(needle.as_str()) => return changed(),
                Ok(_) => {}
                Err(err) => return Err(RemoveError::Failed(format!("could not check: {err}"))),
            }
        }
        if let Some((argv, value)) = &item.recheck.expect {
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            let same = crate::scan::run_timeout(&argv, None, GIT_TIMEOUT)
                .is_ok_and(|out| out.trim() == value.as_str());
            if !same {
                return changed();
            }
        }
        let Some(path) = &item.path else {
            return Ok(());
        };
        let scope = item.recheck.scope.as_ref().unwrap_or(path);
        if !path.exists() || inuse.lock_for(scope).is_some() {
            return changed();
        }
        let is_worktree = matches!(item.source, SourceId::Worktrees | SourceId::AgentWorktrees);
        if is_worktree && item.status.contains(&Status::Clean) {
            let dirty = crate::scan::run_timeout(
                &[
                    "git",
                    "-C",
                    &path.to_string_lossy(),
                    "status",
                    "--porcelain",
                ],
                None,
                GIT_TIMEOUT,
            )
            .map_or(true, |out| !out.trim().is_empty());
            // New ignored files (`.env`) would go with the worktree unseen.
            let shown = item.status.iter().find_map(|s| match s {
                Status::Ignored(n) => Some(*n),
                _ => None,
            });
            let ignored = crate::scan::worktrees::precious_ignored(path);
            if dirty || ignored.is_none_or(|n| n > shown.unwrap_or(0)) {
                return changed();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Item, LeftoverBranch, Removal, SourceId};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        fail_on: Option<String>,
    }

    impl Executor for Fake {
        fn remove_dir(&self, p: &Path) -> Result<(), String> {
            self.record(format!("rmdir {}", p.display()))
        }
        fn clear_dir(&self, p: &Path) -> Result<(), String> {
            self.record(format!("clear {}", p.display()))
        }
        fn remove_file(&self, p: &Path) -> Result<(), String> {
            self.record(format!("rm {}", p.display()))
        }
        fn command(&self, argv: &[String], cwd: Option<&Path>) -> Result<(), String> {
            let cwd = cwd
                .map(|c| format!(" @{}", c.display()))
                .unwrap_or_default();
            self.record(format!("{}{}", argv.join(" "), cwd))
        }
    }

    impl Fake {
        fn record(&self, call: String) -> Result<(), String> {
            let fail = self
                .fail_on
                .as_ref()
                .is_some_and(|f| call.contains(f.as_str()));
            self.calls.lock().unwrap().push(call);
            if fail { Err("boom".into()) } else { Ok(()) }
        }
    }

    fn item(id: u64, source: SourceId, path: Option<PathBuf>, removal: Removal, size: u64) -> Item {
        Item {
            id,
            source,
            label: format!("i{id}"),
            path,
            size: Some(size),
            status: vec![],
            lock: None,
            safe: false,
            removal,
            age_days: None,
            recheck: crate::model::Recheck::default(),
        }
    }

    fn home() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = crate::platform::canonical(d.path()).unwrap();
        (d, p)
    }

    fn guard(home: &Path) -> Guard {
        Guard::new(
            home.to_path_buf(),
            home.join("work"),
            vec![home.join("Library/Caches")],
            vec![],
        )
    }

    fn run(
        items: Vec<Item>,
        exec: &Fake,
        g: &Guard,
        recheck: &dyn Fn(&Item) -> Result<(), RemoveError>,
    ) -> Vec<RemoveEvent> {
        let (tx, rx) = crossbeam_channel::unbounded();
        run_removals(&items, exec, g, recheck, &|_, _| None, tx);
        rx.iter().collect()
    }

    fn worktree_item(h: &Path, id: u64, name: &str) -> Item {
        item(
            id,
            SourceId::Worktrees,
            Some(h.join(name)),
            Removal::Worktree {
                path: h.join(name),
                repo: h.join("r"),
            },
            1,
        )
    }

    fn feat(h: &Path) -> LeftoverBranch {
        LeftoverBranch {
            repo: h.join("r"),
            branch: "feat".into(),
            head: "abc".into(),
        }
    }

    #[test]
    fn leftover_branch_is_reported_once_its_worktree_is_removed() {
        let (_d, h) = home();
        let exec = Fake::default();
        let asked = Mutex::new(Vec::new());
        let leftover = |wt: &Path, repo: &Path| {
            // Asked while the worktree is still there to be read.
            assert!(exec.calls.lock().unwrap().is_empty());
            asked
                .lock()
                .unwrap()
                .push((wt.to_path_buf(), repo.to_path_buf()));
            Some(feat(&h))
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        run_removals(
            &[worktree_item(&h, 1, "w1")],
            &exec,
            &guard(&h),
            &ok,
            &leftover,
            tx,
        );
        let events: Vec<RemoveEvent> = rx.iter().collect();
        assert_eq!(*asked.lock().unwrap(), vec![(h.join("w1"), h.join("r"))]);
        let removed = events
            .iter()
            .position(|e| matches!(e, RemoveEvent::Ok(1, _)))
            .unwrap();
        let reported = events
            .iter()
            .position(|e| matches!(e, RemoveEvent::Leftover(b) if *b == feat(&h)))
            .unwrap();
        assert!(removed < reported);
    }

    #[test]
    fn no_leftover_branch_when_the_worktree_was_not_removed() {
        let (_d, h) = home();
        let leftover = |_: &Path, _: &Path| Some(feat(&h));
        let failing = Fake {
            fail_on: Some("worktree remove".into()),
            ..Default::default()
        };
        let refuse = |_: &Item| Err(RemoveError::Changed);
        for (exec, recheck) in [
            (&failing, &ok as &dyn Fn(&Item) -> Result<(), RemoveError>),
            (&Fake::default(), &refuse),
        ] {
            let (tx, rx) = crossbeam_channel::unbounded();
            run_removals(
                &[worktree_item(&h, 1, "w1")],
                exec,
                &guard(&h),
                recheck,
                &leftover,
                tx,
            );
            assert!(!rx.iter().any(|e| matches!(e, RemoveEvent::Leftover(_))));
        }
    }

    #[test]
    fn only_worktree_removals_are_asked_for_a_leftover_branch() {
        let (_d, h) = home();
        fs::create_dir_all(h.join("a/node_modules")).unwrap();
        let exec = Fake::default();
        let it = item(
            1,
            SourceId::Artifacts,
            Some(h.join("a/node_modules")),
            Removal::RemoveDir(h.join("a/node_modules")),
            1,
        );
        let leftover = |_: &Path, _: &Path| -> Option<LeftoverBranch> { panic!("asked") };
        let (tx, rx) = crossbeam_channel::unbounded();
        run_removals(&[it], &exec, &guard(&h), &ok, &leftover, tx);
        assert!(rx.iter().any(|e| matches!(e, RemoveEvent::Ok(1, _))));
    }

    #[test]
    fn recheck_refuses_when_the_expected_output_changed() {
        // A branch that moved after it was verified as merged.
        let mut it = item(
            1,
            SourceId::Branches,
            None,
            Removal::Command {
                argv: vec!["git".into(), "branch".into(), "-D".into(), "feat".into()],
                cwd: None,
            },
            0,
        );
        let none = crate::inuse::InUse::default;
        it.recheck.expect = Some((vec!["echo".into(), "abc".into()], "abc".into()));
        assert_eq!(default_recheck(none)(&it), Ok(()));
        it.recheck.expect = Some((vec!["echo".into(), "def".into()], "abc".into()));
        assert_eq!(default_recheck(none)(&it), Err(RemoveError::Changed));
        it.recheck.expect = Some((vec!["false".into()], "abc".into()));
        assert_eq!(default_recheck(none)(&it), Err(RemoveError::Changed));
    }

    fn ok(_: &Item) -> Result<(), RemoveError> {
        Ok(())
    }

    #[test]
    fn guard_rejects_relative_home_root_and_outside() {
        let (_d, h) = home();
        for p in ["a/node_modules", "work/b/node_modules", "Library/Caches"] {
            fs::create_dir_all(h.join(p)).unwrap();
        }
        let g = guard(&h);
        assert!(g.check(Path::new("rel/x")).is_err());
        assert!(g.check(&h).is_err());
        assert!(g.check(Path::new("/etc")).is_err());
        assert!(g.check(&h.join("Library/Caches")).is_err());
        assert!(g.check(&h.join("work")).is_err());
        assert!(g.check(&h.join("a/node_modules")).is_ok());
        assert!(g.check(&h.join("work/b/node_modules")).is_ok());
    }

    #[test]
    fn remove_paths_guards_and_removes_each_path() {
        let (_d, h) = home();
        fs::create_dir_all(h.join("avd/P.avd")).unwrap();
        fs::write(h.join("avd/P.ini"), "path=").unwrap();
        let exec = Fake::default();
        let it = item(
            1,
            SourceId::Android,
            Some(h.join("avd/P.avd")),
            Removal::RemovePaths(vec![h.join("avd/P.avd"), h.join("avd/P.ini")]),
            5,
        );
        let events = run(vec![it], &exec, &guard(&h), &ok);
        assert!(events.iter().any(|e| matches!(e, RemoveEvent::Ok(1, 5))));
        assert_eq!(
            *exec.calls.lock().unwrap(),
            vec![
                format!("rmdir {}", h.join("avd/P.avd").display()),
                format!("rm {}", h.join("avd/P.ini").display()),
            ]
        );
    }

    #[test]
    fn remove_paths_touches_nothing_when_one_path_is_refused() {
        let (_d, h) = home();
        fs::create_dir_all(h.join("avd/P.avd")).unwrap();
        let exec = Fake::default();
        let it = item(
            1,
            SourceId::Android,
            Some(h.join("avd/P.avd")),
            Removal::RemovePaths(vec![h.join("avd/P.avd"), PathBuf::from("/etc")]),
            5,
        );
        let events = run(vec![it], &exec, &guard(&h), &ok);
        assert!(events.iter().any(|e| matches!(e, RemoveEvent::Err(1, _))));
        assert!(exec.calls.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn guard_refuses_a_path_swapped_for_a_symlink() {
        // A cache folder replaced by a link to Documents must not be emptied
        // through the link, even though the target is inside $HOME.
        let (_d, h) = home();
        fs::create_dir_all(h.join("Documents")).unwrap();
        fs::create_dir_all(h.join("cache")).unwrap();
        std::os::unix::fs::symlink(h.join("Documents"), h.join("cache/data")).unwrap();
        let g = guard(&h);
        assert_eq!(
            g.check(&h.join("cache/data")),
            Err(RemoveError::Refused("symlink"))
        );
        let exec = Fake::default();
        let it = item(
            1,
            SourceId::DevCaches,
            Some(h.join("cache/data")),
            Removal::ClearDir(h.join("cache/data")),
            1,
        );
        run(vec![it], &exec, &g, &ok);
        assert!(exec.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn stop_request_skips_the_items_not_started_yet() {
        let (_d, h) = home();
        let exec = Fake::default();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let items = vec![
            item(
                1,
                SourceId::Docker,
                None,
                Removal::Command {
                    argv: vec!["a".into()],
                    cwd: None,
                },
                1,
            ),
            item(
                2,
                SourceId::Docker,
                None,
                Removal::Command {
                    argv: vec!["b".into()],
                    cwd: None,
                },
                1,
            ),
        ];
        let stop_after_first = |_: &Item| {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        run_removals_until(
            &items,
            &exec,
            &guard(&h),
            &stop_after_first,
            &|_, _| None,
            &stop,
            tx,
        );
        let events: Vec<RemoveEvent> = rx.iter().collect();
        assert_eq!(*exec.calls.lock().unwrap(), vec!["a".to_string()]);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, RemoveEvent::Err(2, RemoveError::Skipped)))
        );
        assert!(matches!(events.last(), Some(RemoveEvent::Finished)));
    }

    #[test]
    fn guard_with_root_target_still_rejects_system_paths() {
        let (_d, h) = home();
        let g = Guard::new(h.clone(), PathBuf::from("/"), vec![], vec![]);
        assert!(g.check(Path::new("/etc")).is_err());
        assert!(g.check(Path::new("/usr/bin")).is_err());
    }

    /// A guard built from a system's protected folders, as `main` does.
    fn guard_of(os: crate::platform::Os, home: &Path) -> Guard {
        let protected = os.protected_dirs(home, &|_| None);
        for dir in &protected {
            fs::create_dir_all(dir).unwrap();
        }
        Guard::new(home.to_path_buf(), home.join("work"), protected, vec![])
    }

    #[test]
    fn guard_refuses_linux_protected_dirs() {
        let (_d, h) = home();
        let g = guard_of(crate::platform::Os::Linux, &h);
        for rel in [
            ".cache",
            ".config",
            ".local",
            ".local/share",
            ".local/state",
        ] {
            assert_eq!(
                g.check(&h.join(rel)),
                Err(RemoveError::Refused("protected folder")),
                "{rel}"
            );
        }
        // What tools keep inside them is still removable.
        fs::create_dir_all(h.join(".cache/pip")).unwrap();
        assert_eq!(g.check(&h.join(".cache/pip")), Ok(()));
    }

    #[test]
    fn guard_refuses_windows_protected_dirs() {
        let (_d, h) = home();
        let g = guard_of(crate::platform::Os::Windows, &h);
        for rel in [
            "AppData",
            "AppData/Local",
            "AppData/LocalLow",
            "AppData/Roaming",
            "AppData/Local/Temp",
        ] {
            assert_eq!(
                g.check(&h.join(rel)),
                Err(RemoveError::Refused("protected folder")),
                "{rel}"
            );
        }
        fs::create_dir_all(h.join("AppData/Local/pip/Cache")).unwrap();
        assert_eq!(g.check(&h.join("AppData/Local/pip/Cache")), Ok(()));
    }

    /// A junction is Windows' own way of pointing a folder elsewhere: it is
    /// refused like a symlink, and removing through it never reaches the
    /// folder it points at.
    #[cfg(windows)]
    #[test]
    fn guard_refuses_a_junction() {
        let (_d, h) = home();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), "keep").unwrap();
        let junction = h.join("link");
        let made = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        let g = guard_of(crate::platform::Os::Windows, &h);
        assert_eq!(g.check(&junction), Err(RemoveError::Refused("symlink")));
        // Even asked directly, the executor removes the junction itself.
        RealExecutor.remove_dir(&junction).unwrap();
        assert!(!junction.exists());
        assert!(outside.path().join("keep").exists());
    }

    /// npm and pnpm link packages with junctions. Clearing a folder that
    /// holds one removes the junction itself and goes on with the rest; the
    /// folder it points at is left alone.
    #[cfg(windows)]
    #[test]
    fn clearing_a_folder_removes_a_junction_inside_it_not_its_target() {
        let (_d, h) = home();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), "keep").unwrap();
        let cache = h.join("cache");
        fs::create_dir_all(cache.join("sub")).unwrap();
        fs::write(cache.join("sub/file"), "x").unwrap();
        fs::write(cache.join("file"), "x").unwrap();
        let made = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(cache.join("link"))
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        RealExecutor.clear_dir(&cache).unwrap();
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 0);
        assert!(outside.path().join("keep").exists());
    }

    /// The same folder, spelled the plain way and the verbatim way.
    #[cfg(windows)]
    #[test]
    fn guard_accepts_a_verbatim_and_a_plain_path_alike() {
        let (_d, h) = home();
        let g = guard_of(crate::platform::Os::Windows, &h);
        let dir = h.join("work").join("target");
        fs::create_dir_all(&dir).unwrap();
        let verbatim = fs::canonicalize(&dir).unwrap();
        assert!(verbatim.to_string_lossy().starts_with("\\\\?\\"));
        assert_eq!(g.check(&dir), Ok(()));
        assert_eq!(g.check(&verbatim), Ok(()));
        let appdata = fs::canonicalize(h.join("AppData")).unwrap();
        assert_eq!(
            g.check(&appdata),
            Err(RemoveError::Refused("protected folder"))
        );
    }

    /// An extra root is judged by where it resolves: one that turns out to
    /// be the home folder or above it (`$HOME/..`) must not widen the guard,
    /// like a scan target above the home folder does not.
    #[test]
    fn extra_root_that_resolves_above_home_is_ignored() {
        let (_d, base) = home();
        let h = base.join("home");
        fs::create_dir_all(&h).unwrap();
        let sibling = base.join("sibling/cache");
        fs::create_dir_all(&sibling).unwrap();
        for extra in [h.join(".."), h.join("."), PathBuf::from("/")] {
            let g = Guard::new(h.clone(), h.join("work"), vec![], vec![extra.clone()]);
            assert_eq!(
                g.check(&sibling),
                Err(RemoveError::Refused("outside allowed folders")),
                "{extra:?}"
            );
        }
        // A real root elsewhere still counts.
        let g = Guard::new(
            h.clone(),
            h.join("work"),
            vec![],
            vec![sibling.parent().unwrap().to_path_buf()],
        );
        assert_eq!(g.check(&sibling), Ok(()));
    }

    #[test]
    fn recheck_refuses_clean_worktree_that_gained_ignored_files() {
        let (_d, h) = home();
        let git = |dir: &Path, args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        let r = h.join("r");
        fs::create_dir_all(&r).unwrap();
        git(&r, &["init", "-q", "-b", "main"]);
        fs::write(r.join(".gitignore"), ".env\n").unwrap();
        git(&r, &["add", "."]);
        git(
            &r,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "i",
            ],
        );
        let wt = h.join("wt");
        git(
            &r,
            &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
        );
        let mut it = item(
            1,
            SourceId::Worktrees,
            Some(wt.clone()),
            Removal::RemoveDir(wt.clone()),
            1,
        );
        it.status = vec![Status::Clean];
        let inuse = crate::inuse::InUse::default();
        assert_eq!(default_recheck(|| inuse.clone())(&it), Ok(()));
        fs::write(wt.join(".env"), "SECRET=1").unwrap();
        assert_eq!(
            default_recheck(|| inuse.clone())(&it),
            Err(RemoveError::Changed)
        );
    }

    #[test]
    fn recheck_takes_a_fresh_process_snapshot_for_each_item() {
        // A dev server started while earlier items were being removed.
        let (_d, h) = home();
        fs::create_dir_all(h.join("a/node_modules")).unwrap();
        fs::create_dir_all(h.join("b/node_modules")).unwrap();
        let snapshots = std::cell::Cell::new(0);
        let b = h.join("b");
        let fresh = || {
            snapshots.set(snapshots.get() + 1);
            if snapshots.get() == 1 {
                crate::inuse::InUse::default()
            } else {
                crate::inuse::InUse::parse(&format!("p8\ncnode\nn{}\n", b.display()))
            }
        };
        let recheck = default_recheck(fresh);
        let item_in = |dir: &str| {
            item(
                1,
                SourceId::Artifacts,
                Some(h.join(dir).join("node_modules")),
                Removal::RemoveDir(h.join(dir).join("node_modules")),
                1,
            )
        };
        let mut a = item_in("a");
        a.recheck.scope = Some(h.join("a"));
        let mut b_item = item_in("b");
        b_item.recheck.scope = Some(h.join("b"));
        assert_eq!(recheck(&a), Ok(()));
        assert_eq!(recheck(&b_item), Err(RemoveError::Changed));
    }

    #[test]
    fn recheck_refuses_when_the_scan_lock_came_back() {
        // An emulator started between the scan and `y`.
        let mut it = item(
            1,
            SourceId::Android,
            None,
            Removal::RemoveDir("/x".into()),
            1,
        );
        it.recheck.args = vec!["-avd Pixel_8".into()];
        let running = || {
            crate::inuse::InUse::parse("p7\ncqemu-system-aarch64\nn/\n").with_args(
                &crate::inuse::InUse::parse_ps("7 qemu-system-aarch64 -avd Pixel_8\n"),
            )
        };
        assert_eq!(default_recheck(running)(&it), Err(RemoveError::Changed));
    }

    #[test]
    fn recheck_refuses_when_the_probe_output_names_the_item() {
        // A simulator booted between the scan and `y`.
        let mut it = item(1, SourceId::Ios, None, Removal::RemoveDir("/x".into()), 1);
        it.recheck.probe = Some((
            vec!["echo".into(), "iPhone (ABC-123) (Booted)".into()],
            "ABC-123".into(),
        ));
        let none = crate::inuse::InUse::default;
        assert_eq!(default_recheck(none)(&it), Err(RemoveError::Changed));
        it.recheck.probe = Some((
            vec!["echo".into(), "nothing booted".into()],
            "ABC-123".into(),
        ));
        assert_eq!(default_recheck(none)(&it), Ok(()));
        it.recheck.probe = Some((vec!["false".into()], "ABC-123".into()));
        assert!(
            default_recheck(none)(&it).is_err(),
            "a failing probe fails closed"
        );
    }

    #[test]
    fn recheck_uses_lock_scope_and_busy_names() {
        let (_d, h) = home();
        fs::create_dir_all(h.join("proj/node_modules")).unwrap();
        fs::create_dir_all(h.join("proj/src")).unwrap();
        let mut it = item(
            1,
            SourceId::Artifacts,
            Some(h.join("proj/node_modules")),
            Removal::RemoveDir(h.join("proj/node_modules")),
            1,
        );
        it.recheck.scope = Some(h.join("proj"));
        let dev_server =
            crate::inuse::InUse::parse(&format!("p8\nnode\nn{}\n", h.join("proj/src").display()));
        assert_eq!(
            default_recheck(|| dev_server.clone())(&it),
            Err(RemoveError::Changed)
        );
        let mut cache = item(
            2,
            SourceId::DevCaches,
            Some(h.join("proj")),
            Removal::ClearDir(h.join("proj")),
            1,
        );
        cache.recheck.busy = vec!["Xcode".into()];
        let xcode = crate::inuse::InUse::parse("p9\ncXcode\nn/\n");
        assert_eq!(
            default_recheck(|| xcode.clone())(&cache),
            Err(RemoveError::Changed)
        );
    }

    #[test]
    fn guard_rejects_dotdot_escape() {
        let (_d, h) = home();
        fs::create_dir_all(h.join("a")).unwrap();
        let g = guard(&h);
        let escape = h.join("a/../../../../../../etc");
        assert!(g.check(&escape).is_err());
    }

    #[test]
    fn guard_allows_extra_roots() {
        let (_d, h) = home();
        let (_e, extra) = home();
        fs::create_dir_all(extra.join("clang")).unwrap();
        let g = Guard::new(h.clone(), h.clone(), vec![], vec![extra.clone()]);
        assert!(g.check(&extra.join("clang")).is_ok());
        assert!(g.check(&extra).is_err());
    }

    #[test]
    fn changed_item_is_not_touched() {
        let (_d, h) = home();
        fs::create_dir_all(h.join("a/node_modules")).unwrap();
        let exec = Fake::default();
        let it = item(
            1,
            SourceId::Artifacts,
            Some(h.join("a/node_modules")),
            Removal::RemoveDir(h.join("a/node_modules")),
            10,
        );
        let ev = run(vec![it], &exec, &guard(&h), &|_| Err(RemoveError::Changed));
        assert!(matches!(&ev[1], RemoveEvent::Err(1, RemoveError::Changed)));
        assert!(exec.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn guard_failure_is_reported_and_not_executed() {
        let (_d, h) = home();
        let exec = Fake::default();
        let it = item(
            1,
            SourceId::Artifacts,
            Some(h.clone()),
            Removal::RemoveDir(h.clone()),
            10,
        );
        let ev = run(vec![it], &exec, &guard(&h), &ok);
        assert!(matches!(&ev[1], RemoveEvent::Err(1, _)));
        assert!(exec.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn one_failure_does_not_stop_the_rest() {
        let (_d, h) = home();
        let exec = Fake {
            fail_on: Some("second".into()),
            ..Default::default()
        };
        let cmd = |name: &str| Removal::Command {
            argv: vec!["echo".into(), name.into()],
            cwd: None,
        };
        let items = vec![
            item(1, SourceId::Docker, None, cmd("first"), 1),
            item(2, SourceId::Docker, None, cmd("second"), 1),
            item(3, SourceId::Docker, None, cmd("third"), 1),
        ];
        let ev = run(items, &exec, &guard(&h), &ok);
        let outcomes: Vec<_> = ev
            .iter()
            .filter_map(|e| match e {
                RemoveEvent::Ok(id, _) => Some((*id, true)),
                RemoveEvent::Err(id, _) => Some((*id, false)),
                _ => None,
            })
            .collect();
        assert_eq!(outcomes, vec![(1, true), (2, false), (3, true)]);
        assert!(matches!(ev.last(), Some(RemoveEvent::Finished)));
    }

    #[test]
    fn ok_reports_item_size() {
        let (_d, h) = home();
        let exec = Fake::default();
        let it = item(
            7,
            SourceId::Docker,
            None,
            Removal::Command {
                argv: vec!["true".into()],
                cwd: None,
            },
            412_000_000,
        );
        let ev = run(vec![it], &exec, &guard(&h), &ok);
        assert!(
            ev.iter()
                .any(|e| matches!(e, RemoveEvent::Ok(7, 412_000_000)))
        );
    }

    #[test]
    fn prunes_each_main_repo_once_after_worktree_removals() {
        let (_d, h) = home();
        let exec = Fake::default();
        let repo = h.join("r");
        let wt = |id: u64, name: &str| worktree_item(&h, id, name);
        run(vec![wt(1, "w1"), wt(2, "w2")], &exec, &guard(&h), &ok);
        let calls = exec.calls.lock().unwrap();
        let prunes = calls
            .iter()
            .filter(|c| c.starts_with("git worktree prune"))
            .count();
        assert_eq!(prunes, 1);
        assert!(
            calls
                .last()
                .unwrap()
                .ends_with(&format!("@{}", repo.display()))
        );
    }

    // `touch` is not a Windows tool.
    #[cfg(unix)]
    #[test]
    fn command_args_are_not_shell_joined() {
        let (_d, h) = home();
        let target = h.join("a b");
        RealExecutor
            .command(&["touch".into(), target.display().to_string()], None)
            .unwrap();
        assert!(target.is_file());
    }

    #[test]
    fn failing_command_reports_error() {
        assert!(RealExecutor.command(&["false".into()], None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn remove_dir_keeps_symlink_target() {
        let (_d, h) = home();
        let (_o, outside) = home();
        fs::write(outside.join("keep"), "x").unwrap();
        fs::create_dir_all(h.join("nm")).unwrap();
        std::os::unix::fs::symlink(&outside, h.join("nm/link")).unwrap();
        RealExecutor.remove_dir(&h.join("nm")).unwrap();
        assert!(!h.join("nm").exists());
        assert!(outside.join("keep").is_file());
    }

    #[test]
    fn clear_dir_keeps_the_directory() {
        let (_d, h) = home();
        fs::create_dir_all(h.join("c/sub")).unwrap();
        fs::write(h.join("c/f"), "x").unwrap();
        RealExecutor.clear_dir(&h.join("c")).unwrap();
        assert!(h.join("c").is_dir());
        assert_eq!(fs::read_dir(h.join("c")).unwrap().count(), 0);
    }

    #[test]
    fn default_recheck_flags_missing_and_newly_locked_paths() {
        let (_d, h) = home();
        fs::create_dir_all(h.join("a")).unwrap();
        let present = item(
            1,
            SourceId::Artifacts,
            Some(h.join("a")),
            Removal::RemoveDir(h.join("a")),
            1,
        );
        let missing = item(
            2,
            SourceId::Artifacts,
            Some(h.join("gone")),
            Removal::RemoveDir(h.join("gone")),
            1,
        );
        let inuse = crate::inuse::InUse::parse(&format!("p3\ncnode\nn{}\n", h.join("a").display()));
        assert!(default_recheck(crate::inuse::InUse::default)(&present).is_ok());
        assert_eq!(
            default_recheck(crate::inuse::InUse::default)(&missing),
            Err(RemoveError::Changed)
        );
        assert_eq!(
            default_recheck(|| inuse.clone())(&present),
            Err(RemoveError::Changed)
        );
    }
}
