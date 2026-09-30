//! Rebuildable project artifacts: `node_modules`, `.venv`, `Pods`, …

use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use ignore::WalkState;

use crate::fsutil::days_since;
use crate::model::{Item, Removal, SourceId};
use crate::scan::{ScanCtx, ScanEvent, Scanner, Sender, size_later};

/// Names that are always a rebuildable artifact.
const UNAMBIGUOUS: &[&str] = &[
    "node_modules",
    ".venv",
    "Pods",
    ".next",
    ".expo",
    ".gradle",
    ".cxx",
    ".turbo",
    ".parcel-cache",
    "__pycache__",
    ".pytest_cache",
    ".ruff_cache",
];

/// Names that are an artifact only when git ignores them.
const AMBIGUOUS: &[&str] = &["dist", "build", "out"];

/// Files that mark the root of a project.
const MANIFESTS: &[&str] = &[
    "package.json",
    "pyproject.toml",
    "Podfile",
    "Cargo.toml",
    "composer.json",
    ".git",
];

/// Artifact kind for a directory, when its name (and a sibling manifest,
/// where needed) makes it unambiguous.
pub fn classify(dir: &Path) -> Option<&'static str> {
    let name = dir.file_name()?.to_str()?;
    if let Some(kind) = UNAMBIGUOUS.iter().find(|k| **k == name) {
        return Some(kind);
    }
    let parent = dir.parent()?;
    match name {
        "vendor" if parent.join("composer.json").is_file() => Some("vendor"),
        "target" if parent.join("Cargo.toml").is_file() => Some("target"),
        _ => None,
    }
}

pub struct Artifacts;

impl Scanner for Artifacts {
    fn source(&self) -> SourceId {
        SourceId::Artifacts
    }

    fn available(&self) -> bool {
        true
    }

    fn scan(&self, ctx: &ScanCtx, tx: &Sender<ScanEvent>) -> anyhow::Result<()> {
        let skipped = AtomicU64::new(0);
        let ambiguous = Mutex::new(Vec::new());

        ignore::WalkBuilder::new(&ctx.target)
            .standard_filters(false)
            .follow_links(false)
            .same_file_system(true)
            .build_parallel()
            .run(|| {
                Box::new(|entry| {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(_) => {
                            skipped.fetch_add(1, Ordering::Relaxed);
                            return WalkState::Continue;
                        }
                    };
                    if !entry.file_type().is_some_and(|t| t.is_dir()) {
                        return WalkState::Continue;
                    }
                    let path = entry.path();
                    if is_pruned(path, ctx) {
                        return WalkState::Skip;
                    }
                    if classify(path).is_some() {
                        emit(ctx, tx, path);
                        return WalkState::Skip;
                    }
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if AMBIGUOUS.contains(&name) {
                        ambiguous.lock().unwrap().push(path.to_path_buf());
                        return WalkState::Skip;
                    }
                    WalkState::Continue
                })
            });

        let (ignored, unchecked) = git_ignored(ambiguous.into_inner().unwrap());
        for path in ignored {
            emit(ctx, tx, &path);
        }
        if unchecked > 0 {
            let note =
                format!("dist/build/out skipped in {unchecked} repositories (git did not answer)");
            let _ = tx.send(ScanEvent::Note(SourceId::Artifacts, note));
        }

        let skipped = skipped.into_inner();
        if skipped > 0 {
            let note = format!("{skipped} folders skipped (no permission)");
            let _ = tx.send(ScanEvent::Note(SourceId::Artifacts, note));
        }
        Ok(())
    }
}

fn is_pruned(path: &Path, ctx: &ScanCtx) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name == ".git" {
        return true;
    }
    // Right under the home folder, `Library` and dot-folders hold tools
    // (global npm, editor extensions, caches), not projects.
    path.parent() == Some(ctx.home.as_path()) && (name == "Library" || name.starts_with('.'))
}

fn emit(ctx: &ScanCtx, tx: &Sender<ScanEvent>, path: &Path) {
    let project = owning_project(path, &ctx.target);
    let project_label = match project.strip_prefix(&ctx.target) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => project
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| project.display().to_string()),
    };
    let artifact_rel = path
        .strip_prefix(&project)
        .unwrap_or(path)
        .display()
        .to_string();
    let id = ctx.next_id();
    let item = Item {
        id,
        source: SourceId::Artifacts,
        label: format!("{project_label} · {artifact_rel}"),
        path: Some(path.to_path_buf()),
        size: None,
        status: vec![],
        lock: ctx.inuse.lock_for(&project),
        safe: false,
        removal: Removal::RemoveDir(path.to_path_buf()),
        age_days: age_days(path, &project),
        recheck: crate::model::Recheck {
            scope: Some(project.clone()),
            busy: vec![],
            ..Default::default()
        },
    };
    let _ = tx.send(ScanEvent::Found(item));
    size_later(path.to_path_buf(), id, tx.clone());
}

/// Nearest ancestor (up to `target`) holding a project manifest; the
/// artifact's parent when none is found.
fn owning_project(artifact: &Path, target: &Path) -> PathBuf {
    let parent = artifact.parent().unwrap_or(artifact);
    let mut dir = parent;
    loop {
        if MANIFESTS.iter().any(|m| dir.join(m).exists()) {
            return dir.to_path_buf();
        }
        if dir == target {
            return parent.to_path_buf();
        }
        match dir.parent() {
            Some(up) if up.starts_with(target) => dir = up,
            _ => return parent.to_path_buf(),
        }
    }
}

fn age_days(artifact: &Path, project: &Path) -> Option<u32> {
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let newest = MANIFESTS
        .iter()
        .filter_map(|m| mtime(&project.join(m)))
        .chain(mtime(artifact))
        .max()?;
    Some(days_since(newest))
}

/// Keep only the candidates git ignores, asking once per repository. The
/// second value counts the repositories git could not answer for.
fn git_ignored(candidates: Vec<PathBuf>) -> (Vec<PathBuf>, usize) {
    let mut by_repo: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for path in candidates {
        if let Some(repo) = path.ancestors().skip(1).find(|d| d.join(".git").exists()) {
            by_repo.entry(repo.to_path_buf()).or_default().push(path);
        }
    }
    let mut failed = 0;
    let mut ignored = Vec::new();
    for (repo, paths) in by_repo {
        match check_ignore(&repo, &paths) {
            Ok(found) => ignored.extend(found),
            Err(_) => failed += 1,
        }
    }
    (ignored, failed)
}

/// `git check-ignore` exits 1 when nothing is ignored; anything else that
/// is not a success is a failure, never "nothing ignored".
fn check_ignore(repo: &Path, paths: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(p.as_os_str().as_encoded_bytes());
        input.push(0);
    }
    let repo_arg = repo.to_string_lossy();
    let out = crate::scan::exec(
        &["git", "-C", &repo_arg, "check-ignore", "--stdin", "-z"],
        None,
        Some(input),
        crate::scan::RUN_TIMEOUT,
    )?;
    match out.status.code() {
        Some(0) | Some(1) => {}
        _ => anyhow::bail!("git check-ignore failed in {}", repo.display()),
    }
    Ok(out
        .stdout
        .split(|b| *b == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| {
            let p = PathBuf::from(std::ffi::OsStr::from_bytes(chunk));
            if p.is_absolute() { p } else { repo.join(p) }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inuse::InUse;
    use crate::model::Removal;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    fn mk(root: &Path, rel: &str) {
        fs::create_dir_all(root.join(rel)).unwrap();
        fs::write(root.join(rel).join("f"), b"data").unwrap();
    }

    fn scan_with(root: &Path, inuse: InUse) -> Vec<ScanEvent> {
        let ctx = ScanCtx::new(root.to_path_buf(), root.to_path_buf(), inuse);
        let (tx, rx) = crossbeam_channel::unbounded();
        Artifacts.scan(&ctx, &tx).unwrap();
        drop(tx);
        // Size events come from the rayon pool; wait for the channel to close.
        rx.iter().collect()
    }

    fn items(events: &[ScanEvent]) -> Vec<&Item> {
        events
            .iter()
            .filter_map(|e| match e {
                ScanEvent::Found(i) => Some(i),
                _ => None,
            })
            .collect()
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {:?}", args);
    }

    fn init_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "t@t"]);
        git(dir, &["config", "user.name", "t"]);
    }

    #[test]
    fn finds_top_level_node_modules_only() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("package.json"), "{}").unwrap();
        mk(d.path(), "a/node_modules/x/node_modules");
        let ev = scan_with(d.path(), InUse::default());
        let found = items(&ev);
        assert_eq!(found.len(), 1);
        assert!(found[0].path.as_ref().unwrap().ends_with("a/node_modules"));
    }

    #[test]
    fn finds_unambiguous_artifacts() {
        let d = tempfile::tempdir().unwrap();
        for rel in ["a/.venv", "a/ios/Pods", "a/.next"] {
            mk(d.path(), rel);
        }
        assert_eq!(items(&scan_with(d.path(), InUse::default())).len(), 3);
    }

    #[test]
    fn target_needs_cargo_toml() {
        let d = tempfile::tempdir().unwrap();
        mk(d.path(), "b/target");
        assert!(items(&scan_with(d.path(), InUse::default())).is_empty());
        fs::write(d.path().join("b/Cargo.toml"), "").unwrap();
        assert_eq!(items(&scan_with(d.path(), InUse::default())).len(), 1);
    }

    #[test]
    fn ignored_dist_is_an_artifact() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("r");
        init_repo(&repo);
        fs::write(repo.join(".gitignore"), "dist\n").unwrap();
        mk(&repo, "dist");
        assert_eq!(items(&scan_with(d.path(), InUse::default())).len(), 1);
    }

    #[test]
    fn accented_ignored_dist_is_found_with_real_path() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("projé");
        init_repo(&repo);
        fs::write(repo.join(".gitignore"), "build\n").unwrap();
        mk(&repo, "build");
        let ev = scan_with(d.path(), InUse::default());
        let found = items(&ev);
        assert_eq!(found.len(), 1);
        assert!(found[0].path.as_ref().unwrap().exists());
    }

    #[test]
    fn dot_folders_under_home_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        mk(d.path(), ".nvm/versions/node/v22/lib/node_modules");
        mk(d.path(), ".vscode/extensions/x/node_modules");
        mk(d.path(), "code/.hidden-proj/node_modules");
        let ev = scan_with(d.path(), InUse::default());
        let found = items(&ev);
        assert_eq!(found.len(), 1);
        assert!(
            found[0]
                .path
                .as_ref()
                .unwrap()
                .ends_with("code/.hidden-proj/node_modules")
        );
    }

    #[test]
    fn check_ignore_answers_for_thousands_of_paths() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("r");
        init_repo(&repo);
        fs::write(repo.join(".gitignore"), "dist\n").unwrap();
        let paths: Vec<PathBuf> = (0..5000).map(|i| repo.join(format!("p{i}/dist"))).collect();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(check_ignore(&repo, &paths).map(|v| v.len()));
        });
        let found = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("git check-ignore deadlocked");
        assert_eq!(found.unwrap(), 5000);
    }

    #[test]
    fn check_ignore_failure_is_an_error_not_an_empty_answer() {
        let d = tempfile::tempdir().unwrap();
        assert!(check_ignore(d.path(), &[d.path().join("dist")]).is_err());
    }

    #[test]
    fn tracked_dist_is_not_an_artifact() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("r");
        init_repo(&repo);
        mk(&repo, "dist");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "x"]);
        assert!(items(&scan_with(d.path(), InUse::default())).is_empty());
    }

    #[test]
    fn label_is_project_and_relative_artifact() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("a")).unwrap();
        fs::write(d.path().join("a/package.json"), "{}").unwrap();
        mk(d.path(), "a/ios/Pods");
        let ev = scan_with(d.path(), InUse::default());
        assert_eq!(items(&ev)[0].label, "a · ios/Pods");
    }

    #[test]
    fn locked_when_process_inside_project() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("a/src")).unwrap();
        fs::write(d.path().join("a/package.json"), "{}").unwrap();
        mk(d.path(), "a/node_modules");
        let cwd = d.path().join("a/src");
        let iu = InUse::parse(&format!("p5\ncnode\nn{}\n", cwd.display()));
        let ev = scan_with(d.path(), iu);
        assert_eq!(items(&ev)[0].lock.as_deref(), Some("node · PID 5"));
    }

    #[test]
    fn never_safe() {
        let d = tempfile::tempdir().unwrap();
        mk(d.path(), "a/node_modules");
        let ev = scan_with(d.path(), InUse::default());
        let it = items(&ev)[0];
        assert!(!it.safe);
        assert_eq!(
            it.removal,
            Removal::RemoveDir(d.path().join("a/node_modules"))
        );
    }

    #[test]
    fn walk_skips_library_and_volumes() {
        let d = tempfile::tempdir().unwrap();
        mk(d.path(), "Library/x/node_modules");
        mk(d.path(), ".Trash/y/node_modules");
        mk(d.path(), "code/z/node_modules");
        let ev = scan_with(d.path(), InUse::default());
        let found = items(&ev);
        assert_eq!(found.len(), 1);
        assert!(
            found[0]
                .path
                .as_ref()
                .unwrap()
                .ends_with("code/z/node_modules")
        );
    }

    #[test]
    fn emits_size_event_after_found() {
        let d = tempfile::tempdir().unwrap();
        mk(d.path(), "a/node_modules");
        let ev = scan_with(d.path(), InUse::default());
        let id = items(&ev)[0].id;
        assert!(
            ev.iter()
                .any(|e| matches!(e, ScanEvent::Size(i, n) if *i == id && *n > 0))
        );
    }
}
