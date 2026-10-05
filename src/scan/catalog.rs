//! Declarative catalog of developer tool caches (`catalog/*.toml`).

use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde::Deserialize;

use crate::fsutil::dir_size;
use crate::model::{Item, Removal, SourceId, Status};
use crate::platform::Os;
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
    /// Paths on every system. `~` is the home folder; a path may instead
    /// start with a token ([`TOKENS`]); one component may contain one `*`.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Paths that only exist on one system, added to `paths` there.
    #[serde(default)]
    pub paths_macos: Vec<String>,
    #[serde(default)]
    pub paths_linux: Vec<String>,
    #[serde(default)]
    pub paths_windows: Vec<String>,
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

impl Rule {
    /// The path patterns that apply on `os`.
    pub fn paths_for(&self, os: Os) -> impl Iterator<Item = &str> {
        let own = match os {
            Os::MacOs => &self.paths_macos,
            Os::Linux => &self.paths_linux,
            Os::Windows => &self.paths_windows,
        };
        self.paths.iter().chain(own).map(String::as_str)
    }
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
type EnvFn = Box<dyn Fn(&str) -> Option<std::ffi::OsString> + Send + Sync>;

pub struct Catalog {
    pub rules: Vec<Rule>,
    pub has_bin: HasBin,
    pub runner: Runner,
    /// Environment lookup behind the path tokens.
    pub env: EnvFn,
}

impl Catalog {
    pub fn load() -> anyhow::Result<Catalog> {
        Ok(Catalog {
            rules: load_catalog()?,
            has_bin: Box::new(|b| which(b).is_some()),
            runner: Box::new(run),
            env: Box::new(crate::platform::process_env),
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
        rule.paths_for(ctx.os)
            .flat_map(|p| expand(p, ctx, &self.runner, &self.env))
            .collect()
    }
}

/// Tokens a path pattern may start with, each a folder the system chooses.
pub const TOKENS: [&str; 4] = [
    "{darwin_cache}",
    "{xdg_cache}",
    "{localappdata}",
    "{appdata}",
];

/// The folder behind a token. A token the system does not define, or
/// defines as something that is not an absolute path, has none.
fn token_dir(token: &str, ctx: &ScanCtx, runner: &Runner, env: &EnvFn) -> Option<PathBuf> {
    let from_env = |key: &str| env(key).map(PathBuf::from).filter(|p| p.is_absolute());
    match token {
        "{darwin_cache}" => runner(&["getconf", "DARWIN_USER_CACHE_DIR"])
            .ok()
            .map(|dir| PathBuf::from(dir.trim()))
            .filter(|p| p.is_absolute()),
        "{xdg_cache}" => {
            Some(from_env("XDG_CACHE_HOME").unwrap_or_else(|| ctx.home.join(".cache")))
        }
        "{localappdata}" => from_env("LOCALAPPDATA"),
        "{appdata}" => from_env("APPDATA"),
        _ => None,
    }
}

/// Expand `~`, a leading token and a `*` in one component.
fn expand(pattern: &str, ctx: &ScanCtx, runner: &Runner, env: &EnvFn) -> Vec<PathBuf> {
    let mut p = pattern.to_string();
    if let Some(rest) = p.strip_prefix("~/") {
        p = ctx.home.join(rest).display().to_string();
    }
    let token = TOKENS
        .iter()
        .find_map(|t| pattern.strip_prefix(t).map(|rest| (*t, rest)));
    if let Some((token, rest)) = token {
        let Some(dir) = token_dir(token, ctx, runner, env) else {
            return vec![];
        };
        p = dir.join(rest.trim_start_matches('/')).display().to_string();
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
                // `safe` describes the rule's command; its fallback wipes the
                // folder, which may be a download cache.
                safe: rule.safe
                    && (rule.command.is_none() || matches!(removal, Removal::Command { .. })),
                removal,
                age_days: None,
                recheck: crate::model::Recheck {
                    scope: None,
                    busy: rule.busy_when.clone(),
                    ..Default::default()
                },
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
        // The built-in rules are exercised through their macOS paths.
        let ctx = ScanCtx::new(home.to_path_buf(), home.to_path_buf(), inuse).with_os(Os::MacOs);
        let catalog = Catalog {
            rules,
            has_bin: Box::new(move |b| bins.contains(&b)),
            runner: Box::new(|_| anyhow::bail!("no runner in tests")),
            env: Box::new(|_| None),
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
        fill(&d.path().join("Library/Caches/Yarn"));
        let items = scan(d.path(), vec![by_id("yarn-cache")], &[], InUse::default());
        assert_eq!(
            items[0].removal,
            Removal::ClearDir(d.path().join("Library/Caches/Yarn"))
        );
    }

    #[test]
    fn safe_rule_is_not_safe_when_falling_back_from_its_command() {
        // `npm cache verify` is harmless; wiping the npm cache is not.
        let d = tempfile::tempdir().unwrap();
        fill(&d.path().join(".npm/_cacache"));
        let rule = by_id("npm-cacache");
        assert!(rule.safe && rule.command.is_some());
        let items = scan(d.path(), vec![rule], &[], InUse::default());
        assert!(matches!(items[0].removal, Removal::ClearDir(_)));
        assert!(!items[0].safe);
    }

    #[test]
    fn command_used_when_bin_present() {
        let d = tempfile::tempdir().unwrap();
        fill(&d.path().join("Library/Caches/Yarn"));
        let items = scan(
            d.path(),
            vec![by_id("yarn-cache")],
            &["yarn"],
            InUse::default(),
        );
        assert!(matches!(&items[0].removal, Removal::Command { argv, .. } if argv[0] == "yarn"));
    }

    #[test]
    fn bun_cache_is_cleared_directly_even_with_bun_installed() {
        // `bun pm cache rm` fails outside a folder with a package.json.
        let d = tempfile::tempdir().unwrap();
        fill(&d.path().join(".bun/install/cache"));
        let items = scan(
            d.path(),
            vec![by_id("bun-cache")],
            &["bun"],
            InUse::default(),
        );
        assert_eq!(
            items[0].removal,
            Removal::ClearDir(d.path().join(".bun/install/cache"))
        );
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

    /// Paths found by scanning `home` as `os`, with `env` as the environment.
    fn found_on(
        os: Os,
        env: &'static [(&'static str, &'static str)],
        home: &Path,
        rule: Rule,
    ) -> Vec<PathBuf> {
        let ctx =
            ScanCtx::new(home.to_path_buf(), home.to_path_buf(), InUse::default()).with_os(os);
        let catalog = Catalog {
            rules: vec![rule],
            has_bin: Box::new(|_| false),
            runner: Box::new(|_| anyhow::bail!("no runner in tests")),
            env: Box::new(move |key| {
                env.iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| std::ffi::OsString::from(v))
            }),
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        catalog.scan(&ctx, &tx).unwrap();
        drop(tx);
        let mut paths: Vec<PathBuf> = rx
            .iter()
            .filter_map(|e| match e {
                ScanEvent::Found(i) => i.path,
                _ => None,
            })
            .collect();
        paths.sort();
        paths
    }

    const PER_PLATFORM: &str = "[[rule]]\nid='p'\ngroup='G'\nlabel='l'\npaths=['~/a']\n\
        paths_macos=['~/Library/Caches/m']\npaths_linux=['{xdg_cache}/b']\n\
        paths_windows=['{localappdata}/w']\nsafe=false\nmode='clear'\nsource='s'\n";

    #[test]
    fn paths_follow_the_platform() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        for dir in ["a", "Library/Caches/m", ".cache/b", "local/w"] {
            fill(&home.join(dir));
        }
        assert_eq!(
            found_on(Os::Linux, &[], home, rule(PER_PLATFORM)),
            [home.join(".cache/b"), home.join("a")]
        );
        assert_eq!(
            found_on(Os::MacOs, &[], home, rule(PER_PLATFORM)),
            [home.join("Library/Caches/m"), home.join("a")]
        );
    }

    #[test]
    fn xdg_cache_home_overrides_the_default() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        fill(&home.join(".cache/b"));
        let xdg = d.path().join("xdg");
        fill(&xdg.join("b"));
        // Leaked on purpose: the environment closure needs a 'static value.
        let value: &'static str = Box::leak(xdg.to_str().unwrap().to_string().into_boxed_str());
        let env: &'static [(&str, &str)] = Box::leak(Box::new([("XDG_CACHE_HOME", value)]));
        assert_eq!(
            found_on(Os::Linux, env, &home, rule(PER_PLATFORM)),
            [xdg.join("b")]
        );
    }

    #[test]
    fn a_token_without_a_value_resolves_to_nothing() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        fill(&home.join("{localappdata}/w"));
        fill(&home.join("w"));
        assert!(found_on(Os::Windows, &[], home, rule(PER_PLATFORM)).is_empty());
        // An empty or relative value is no better than none.
        assert!(
            found_on(
                Os::Windows,
                &[("LOCALAPPDATA", "")],
                home,
                rule(PER_PLATFORM)
            )
            .is_empty()
        );
        assert!(
            found_on(
                Os::Windows,
                &[("LOCALAPPDATA", "w")],
                home,
                rule(PER_PLATFORM)
            )
            .is_empty()
        );
    }

    #[test]
    fn rule_without_a_path_for_this_platform_emits_nothing() {
        let d = tempfile::tempdir().unwrap();
        fill(&d.path().join("Library/Caches/m"));
        let only_mac = rule(
            "[[rule]]\nid='p'\ngroup='G'\nlabel='l'\npaths_macos=['~/Library/Caches/m']\n\
             safe=false\nmode='clear'\nsource='s'\n",
        );
        assert!(found_on(Os::Linux, &[], d.path(), only_mac.clone()).is_empty());
        assert_eq!(found_on(Os::MacOs, &[], d.path(), only_mac).len(), 1);
    }

    #[test]
    fn no_rule_reaches_into_library_outside_macos() {
        for r in load_catalog().unwrap() {
            for os in [Os::Linux, Os::Windows] {
                for p in r.paths_for(os) {
                    assert!(
                        !p.contains("Library/") && !p.contains("{darwin_cache}"),
                        "{} reaches {p} on {os:?}",
                        r.id
                    );
                }
            }
        }
    }

    #[test]
    fn every_platform_path_is_home_relative_or_a_known_token() {
        const ROOTS: [&str; 5] = [
            "~/",
            "{darwin_cache}/",
            "{xdg_cache}/",
            "{localappdata}/",
            "{appdata}/",
        ];
        for r in load_catalog().unwrap() {
            for os in Os::ALL {
                for p in r.paths_for(os) {
                    assert!(
                        ROOTS.iter().any(|root| p.starts_with(root)),
                        "{}: {p}",
                        r.id
                    );
                    assert!(!p.contains(".."), "{}: {p}", r.id);
                }
            }
        }
    }

    #[test]
    fn every_rule_has_a_path_on_some_platform() {
        for r in load_catalog().unwrap() {
            assert!(
                r.path_cmd.is_some() || Os::ALL.iter().any(|os| r.paths_for(*os).next().is_some()),
                "{} has no path",
                r.id
            );
        }
    }
}
