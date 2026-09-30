//! Declarative catalog of developer tool caches (`catalog/*.toml`).

use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde::Deserialize;

use crate::fsutil::dir_size;
use crate::model::{Item, Removal, SourceId, Status};
use crate::scan::{ScanCtx, ScanEvent, Scanner, Sender, run, which};

const FILES: &[&str] = &[
    include_str!("../../catalog/javascript.toml"),
    include_str!("../../catalog/python.toml"),
    include_str!("../../catalog/native.toml"),
    include_str!("../../catalog/mobile.toml"),
    include_str!("../../catalog/apple.toml"),
    include_str!("../../catalog/tools.toml"),
    include_str!("../../catalog/ai.toml"),
];

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Delete the path itself.
    Remove,
    /// Delete what is inside the path, keeping the folder.
    Clear,
    /// Same as `clear`, kept for rules that think in terms of children.
    Children,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    pub group: String,
    pub label: String,
    /// `~` is the home folder, `{darwin_cache}` the per-user cache folder;
    /// the last component may contain one `*`.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Command printing the path, for tools that choose it at runtime.
    #[serde(default)]
    pub path_cmd: Option<Vec<String>>,
    pub safe: bool,
    pub mode: Mode,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub requires_bin: Option<String>,
    #[serde(default)]
    pub busy_when: Vec<String>,
    pub source: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleFile {
    pub rule: Vec<Rule>,
}

pub fn load_catalog() -> anyhow::Result<Vec<Rule>> {
    let mut rules = Vec::new();
    for src in FILES {
        let file: RuleFile = toml::from_str(src)?;
        rules.extend(file.rule);
    }
    Ok(rules)
}

type HasBin = Box<dyn Fn(&str) -> bool + Send + Sync>;
type Runner = Box<dyn Fn(&[&str]) -> anyhow::Result<String> + Send + Sync>;

pub struct Catalog {
    pub rules: Vec<Rule>,
    pub has_bin: HasBin,
    pub runner: Runner,
}

impl Catalog {
    pub fn load() -> anyhow::Result<Catalog> {
        Ok(Catalog {
            rules: load_catalog()?,
            has_bin: Box::new(|b| which(b).is_some()),
            runner: Box::new(run),
        })
    }

    fn resolve(&self, rule: &Rule, ctx: &ScanCtx) -> Vec<PathBuf> {
        if let Some(cmd) = &rule.path_cmd {
            if let Some(bin) = cmd.first()
                && !(self.has_bin)(bin)
            {
                return vec![];
            }
            let argv: Vec<&str> = cmd.iter().map(String::as_str).collect();
            return (self.runner)(&argv)
                .ok()
                .map(|out| out.trim().to_string())
                .filter(|p| !p.is_empty())
                .map(|p| vec![PathBuf::from(p)])
                .unwrap_or_default();
        }
        rule.paths
            .iter()
            .flat_map(|p| expand(p, ctx, &self.runner))
            .collect()
    }
}

/// Expand `~`, `{darwin_cache}` and a `*` in the last component.
fn expand(pattern: &str, ctx: &ScanCtx, runner: &Runner) -> Vec<PathBuf> {
    let mut p = pattern.to_string();
    if let Some(rest) = p.strip_prefix("~/") {
        p = ctx.home.join(rest).display().to_string();
    }
    if p.contains("{darwin_cache}") {
        let Some(dir) = runner(&["getconf", "DARWIN_USER_CACHE_DIR"]).ok() else {
            return vec![];
        };
        p = p.replace("{darwin_cache}", dir.trim().trim_end_matches('/'));
    }
    let path = PathBuf::from(&p);
    if !p.contains('*') {
        return vec![path];
    }
    // Only one `*` is supported, in any single component.
    let comps: Vec<_> = path.components().collect();
    let Some(star) = comps
        .iter()
        .position(|c| c.as_os_str().to_string_lossy().contains('*'))
    else {
        return vec![];
    };
    let base: PathBuf = comps[..star].iter().collect();
    let pat = comps[star].as_os_str().to_string_lossy().to_string();
    let rest: PathBuf = comps[star + 1..].iter().collect();
    let (prefix, suffix) = pat.split_once('*').unwrap_or((&pat, ""));
    let Ok(entries) = std::fs::read_dir(&base) else {
        return vec![];
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with(prefix) && name.ends_with(suffix)
        })
        .map(|e| e.path().join(&rest))
        .collect();
    out.sort();
    out
}

impl Scanner for Catalog {
    fn source(&self) -> SourceId {
        SourceId::DevCaches
    }

    fn available(&self) -> bool {
        true
    }

    fn scan(&self, ctx: &ScanCtx, tx: &Sender<ScanEvent>) -> anyhow::Result<()> {
        let targets: Vec<(&Rule, PathBuf, usize)> = self
            .rules
            .iter()
            .flat_map(|rule| {
                let paths: Vec<PathBuf> = self
                    .resolve(rule, ctx)
                    .into_iter()
                    .filter(|p| p.is_dir())
                    .collect();
                let n = paths.len();
                paths.into_iter().map(move |p| (rule, p, n))
            })
            .collect();

        targets.par_iter().for_each(|(rule, path, siblings)| {
            let bytes = dir_size(path);
            if bytes == 0 {
                return;
            }
            let removal = match (&rule.command, &rule.requires_bin) {
                (Some(cmd), Some(bin)) if (self.has_bin)(bin) => Removal::Command {
                    argv: cmd.clone(),
                    cwd: None,
                },
                (Some(cmd), None) => Removal::Command {
                    argv: cmd.clone(),
                    cwd: None,
                },
                _ => match rule.mode {
                    Mode::Remove => Removal::RemoveDir(path.clone()),
                    Mode::Clear | Mode::Children => Removal::ClearDir(path.clone()),
                },
            };
            let label = if *siblings > 1 {
                format!("{} · {}", rule.label, short(path))
            } else {
                rule.label.clone()
            };
            let names: Vec<&str> = rule.busy_when.iter().map(String::as_str).collect();
            let item = Item {
                id: ctx.next_id(),
                source: SourceId::DevCaches,
                label,
                path: Some(path.clone()),
                size: Some(bytes),
                status: vec![Status::Detail(rule.group.clone())],
                lock: ctx.inuse.busy(&names),
                safe: rule.safe,
                removal,
                age_days: None,
            };
            let _ = tx.send(ScanEvent::Found(item));
        });
        Ok(())
    }
}

/// Last two components, enough to tell sibling paths apart.
fn short(path: &Path) -> String {
    let comps: Vec<_> = path.components().rev().take(2).collect();
    comps
        .iter()
        .rev()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inuse::InUse;
    use crate::model::Removal;
    use std::fs;
    use std::path::Path;

    fn rule(toml_src: &str) -> Rule {
        let file: RuleFile = toml::from_str(toml_src).unwrap();
        file.rule.into_iter().next().unwrap()
    }

    fn scan(
        home: &Path,
        rules: Vec<Rule>,
        bins: &'static [&'static str],
        inuse: InUse,
    ) -> Vec<Item> {
        let ctx = ScanCtx::new(home.to_path_buf(), home.to_path_buf(), inuse);
        let catalog = Catalog {
            rules,
            has_bin: Box::new(move |b| bins.contains(&b)),
            runner: Box::new(|_| anyhow::bail!("no runner in tests")),
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        catalog.scan(&ctx, &tx).unwrap();
        drop(tx);
        rx.iter()
            .filter_map(|e| {
                if let ScanEvent::Found(i) = e {
                    Some(i)
                } else {
                    None
                }
            })
            .collect()
    }

    fn fill(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("blob"), vec![1u8; 8192]).unwrap();
    }

    fn by_id(id: &str) -> Rule {
        load_catalog()
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap()
    }

    #[test]
    fn every_rule_parses_and_has_source() {
        let rules = load_catalog().unwrap();
        assert!(rules.len() >= 15);
        for r in rules {
            assert!(!r.source.trim().is_empty(), "{}", r.id);
            assert!(
                !r.paths.is_empty() || r.path_cmd.is_some(),
                "{} has no path",
                r.id
            );
        }
    }

    #[test]
    fn rule_ids_are_unique() {
        let rules = load_catalog().unwrap();
        let mut ids: Vec<_> = rules.iter().map(|r| r.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), rules.len());
    }

    #[test]
    fn unknown_field_is_rejected() {
        let src = "[[rule]]\nid='x'\ngroup='g'\nlabel='l'\npaths=['~/x']\nsafee=true\nsafe=true\nmode='clear'\nsource='s'\n";
        assert!(toml::from_str::<RuleFile>(src).is_err());
    }

    #[test]
    fn required_rules_exist() {
        let rules = load_catalog().unwrap();
        for id in [
            "npm-cacache",
            "npm-npx",
            "npm-logs",
            "pnpm-store",
            "bun-cache",
            "yarn-cache",
            "gradle-caches",
            "cargo-registry",
            "go-mod",
            "go-build",
            "cocoapods",
            "playwright",
            "xcode-derived-data",
            "coresim-caches",
            "coresim-logs",
        ] {
            assert!(rules.iter().any(|r| r.id == id), "missing {id}");
        }
    }

    #[test]
    fn download_caches_are_never_safe() {
        for id in [
            "bun-cache",
            "yarn-cache",
            "gradle-caches",
            "cargo-registry",
            "go-mod",
            "cocoapods",
            "playwright",
        ] {
            assert!(!by_id(id).safe, "{id}");
        }
    }

    #[test]
    fn pnpm_store_uses_prune_and_is_safe() {
        let r = by_id("pnpm-store");
        assert!(r.safe);
        assert_eq!(
            r.command.as_deref(),
            Some(&["pnpm".to_string(), "store".into(), "prune".into()][..])
        );
    }

    #[test]
    fn command_falls_back_to_mode_when_bin_missing() {
        let d = tempfile::tempdir().unwrap();
        fill(&d.path().join(".bun/install/cache"));
        let items = scan(d.path(), vec![by_id("bun-cache")], &[], InUse::default());
        assert_eq!(
            items[0].removal,
            Removal::ClearDir(d.path().join(".bun/install/cache"))
        );
    }

    #[test]
    fn command_used_when_bin_present() {
        let d = tempfile::tempdir().unwrap();
        fill(&d.path().join(".bun/install/cache"));
        let items = scan(
            d.path(),
            vec![by_id("bun-cache")],
            &["bun"],
            InUse::default(),
        );
        assert!(matches!(&items[0].removal, Removal::Command { argv, .. } if argv[0] == "bun"));
    }

    #[test]
    fn busy_process_locks_item() {
        let d = tempfile::tempdir().unwrap();
        fill(&d.path().join(".bun/install/cache"));
        let iu = InUse::parse("p9\ncbun\nn/tmp\n");
        let items = scan(d.path(), vec![by_id("bun-cache")], &[], iu);
        assert_eq!(items[0].lock.as_deref(), Some("bun · PID 9"));
    }

    #[test]
    fn missing_path_emits_nothing() {
        let d = tempfile::tempdir().unwrap();
        assert!(
            scan(
                d.path(),
                vec![by_id("gradle-caches")],
                &[],
                InUse::default()
            )
            .is_empty()
        );
    }

    #[test]
    fn empty_dir_emits_nothing() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".gradle/caches")).unwrap();
        assert!(
            scan(
                d.path(),
                vec![by_id("gradle-caches")],
                &[],
                InUse::default()
            )
            .is_empty()
        );
    }

    #[test]
    fn glob_in_last_component_expands() {
        let d = tempfile::tempdir().unwrap();
        fill(&d.path().join(".gem/ruby/3.3.0/cache"));
        fill(&d.path().join(".gem/ruby/4.0.0/cache"));
        let r = rule(
            "[[rule]]\nid='g'\ngroup='Ruby'\nlabel='gems'\npaths=['~/.gem/ruby/*/cache']\nsafe=false\nmode='clear'\nsource='s'\n",
        );
        assert_eq!(scan(d.path(), vec![r], &[], InUse::default()).len(), 2);
    }

    #[test]
    fn item_carries_group_and_rule_label() {
        let d = tempfile::tempdir().unwrap();
        fill(&d.path().join(".npm/_npx"));
        let items = scan(d.path(), vec![by_id("npm-npx")], &[], InUse::default());
        assert_eq!(items[0].label, "npx cache");
        assert!(
            items[0]
                .status
                .contains(&crate::model::Status::Detail("JavaScript".into()))
        );
        assert!(items[0].safe);
        assert!(items[0].size.is_some_and(|s| s > 0));
    }
}
