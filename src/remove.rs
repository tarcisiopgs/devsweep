//! Removing what the user confirmed, one item at a time, with a last check
//! before each deletion.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crossbeam_channel::Sender;

use crate::inuse::InUse;
use crate::model::{Item, ItemId, Removal, SourceId, Status};

pub trait Executor: Send + Sync {
    fn remove_dir(&self, p: &Path) -> Result<(), String>;
    fn clear_dir(&self, p: &Path) -> Result<(), String>;
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
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let result = if is_dir {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            result.map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(())
    }

    fn command(&self, argv: &[String], cwd: Option<&Path>) -> Result<(), String> {
        let (bin, args) = argv.split_first().ok_or("empty command")?;
        let mut cmd = Command::new(bin);
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let out = cmd.output().map_err(|e| format!("{bin}: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let first = stderr
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("failed")
                .trim();
            Err(first.to_string())
        }
    }
}

/// Last line of defence against a scanner bug: directory removals must stay
/// inside the home folder, the scanned folder or a known extra root, and
/// never hit one of those roots or a protected folder itself.
pub struct Guard {
    roots: Vec<PathBuf>,
    protected: Vec<PathBuf>,
}

fn canon(p: PathBuf) -> PathBuf {
    std::fs::canonicalize(&p).unwrap_or(p)
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
        let roots: Vec<PathBuf> = std::iter::once(home)
            .chain(target)
            .chain(extra_roots.into_iter().map(canon))
            .collect();
        let protected = protected.into_iter().map(canon).collect();
        Guard { roots, protected }
    }

    pub fn check(&self, p: &Path) -> Result<(), String> {
        if !p.is_absolute() {
            return Err("refused: relative path".into());
        }
        let real = std::fs::canonicalize(p).map_err(|_| "changed since scan".to_string())?;
        if !self.roots.iter().any(|root| real.starts_with(root)) {
            return Err("refused: outside allowed folders".into());
        }
        if self.roots.contains(&real) || self.protected.contains(&real) {
            return Err("refused: protected folder".into());
        }
        Ok(())
    }
}

pub enum RemoveEvent {
    Started(ItemId),
    Ok(ItemId, u64),
    Err(ItemId, String),
    Finished,
}

fn is_worktree_remove(removal: &Removal) -> Option<&Path> {
    match removal {
        Removal::Command {
            argv,
            cwd: Some(cwd),
        } if argv.len() >= 3 && argv[..3] == ["git", "worktree", "remove"] => Some(cwd.as_path()),
        _ => None,
    }
}

/// Remove every item in order. A failure never stops the batch.
pub fn run_removals(
    items: Vec<Item>,
    exec: &dyn Executor,
    guard: &Guard,
    recheck: &dyn Fn(&Item) -> Result<(), String>,
    tx: Sender<RemoveEvent>,
) {
    let mut repos_to_prune = BTreeSet::new();
    for item in &items {
        let _ = tx.send(RemoveEvent::Started(item.id));
        let result = recheck(item).and_then(|()| match &item.removal {
            Removal::RemoveDir(p) => guard.check(p).and_then(|()| exec.remove_dir(p)),
            Removal::ClearDir(p) => guard.check(p).and_then(|()| exec.clear_dir(p)),
            Removal::Command { argv, cwd } => exec.command(argv, cwd.as_deref()),
        });
        match result {
            Ok(()) => {
                if let Some(repo) = is_worktree_remove(&item.removal) {
                    repos_to_prune.insert(repo.to_path_buf());
                }
                let _ = tx.send(RemoveEvent::Ok(item.id, item.size.unwrap_or(0)));
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

/// Re-check an item right before removing it: its folder still exists, no
/// process moved into it, and a clean worktree is still clean.
pub fn default_recheck(inuse: &InUse) -> impl Fn(&Item) -> Result<(), String> + '_ {
    move |item: &Item| {
        let changed = || Err("changed since scan".to_string());
        let busy: Vec<&str> = item.recheck.busy.iter().map(String::as_str).collect();
        if !busy.is_empty() && inuse.busy(&busy).is_some() {
            return changed();
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
            let dirty = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(["status", "--porcelain"])
                .output()
                .map(|o| !o.status.success() || !o.stdout.is_empty())
                .unwrap_or(true);
            if dirty {
                return changed();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Item, Removal, SourceId};
    use std::fs;
    use std::path::{Path, PathBuf};
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
        let p = fs::canonicalize(d.path()).unwrap();
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
        recheck: &dyn Fn(&Item) -> Result<(), String>,
    ) -> Vec<RemoveEvent> {
        let (tx, rx) = crossbeam_channel::unbounded();
        run_removals(items, exec, g, recheck, tx);
        rx.iter().collect()
    }

    fn ok(_: &Item) -> Result<(), String> {
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
    fn guard_with_root_target_still_rejects_system_paths() {
        let (_d, h) = home();
        let g = Guard::new(h.clone(), PathBuf::from("/"), vec![], vec![]);
        assert!(g.check(Path::new("/etc")).is_err());
        assert!(g.check(Path::new("/usr/bin")).is_err());
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
            default_recheck(&dev_server)(&it),
            Err("changed since scan".into())
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
            default_recheck(&xcode)(&cache),
            Err("changed since scan".into())
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
        let ev = run(vec![it], &exec, &guard(&h), &|_| {
            Err("changed since scan".into())
        });
        assert!(matches!(&ev[1], RemoveEvent::Err(1, m) if m == "changed since scan"));
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
        let wt = |id: u64, name: &str| {
            let argv = vec![
                "git".into(),
                "worktree".into(),
                "remove".into(),
                h.join(name).display().to_string(),
            ];
            item(
                id,
                SourceId::Worktrees,
                Some(h.join(name)),
                Removal::Command {
                    argv,
                    cwd: Some(repo.clone()),
                },
                1,
            )
        };
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
        assert!(default_recheck(&crate::inuse::InUse::default())(&present).is_ok());
        assert_eq!(
            default_recheck(&crate::inuse::InUse::default())(&missing),
            Err("changed since scan".into())
        );
        assert_eq!(
            default_recheck(&inuse)(&present),
            Err("changed since scan".into())
        );
    }
}
