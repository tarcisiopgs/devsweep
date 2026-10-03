//! Docker images, build cache, stopped containers and orphan volumes.

use serde::Deserialize;

use crate::model::{Item, Removal, SourceId, Status};
use crate::scan::{ScanCtx, ScanEvent, Scanner, Sender, run, which};

type Runner<'a> = &'a dyn Fn(&[&str]) -> anyhow::Result<String>;

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct DiskUsage {
    images: Vec<Image>,
    containers: Vec<Container>,
    volumes: Vec<Volume>,
    build_cache: Vec<Cache>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Image {
    containers: String,
    #[serde(rename = "ID")]
    id: String,
    repository: String,
    tag: String,
    size: String,
    unique_size: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Container {
    state: String,
    status: String,
    size: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Volume {
    labels: String,
    links: String,
    name: String,
    size: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Cache {
    size: String,
}

/// Docker's human sizes (`1.623GB`, `512kB`, `63B`) in bytes, base 1000.
pub fn parse_human_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (number, unit) = s.split_at(split);
    let value: f64 = number.parse().ok()?;
    let factor = match unit.to_ascii_uppercase().as_str() {
        "B" => 1.0,
        "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => return None,
    };
    Some((value * factor).round() as u64)
}

fn size(s: &str) -> u64 {
    parse_human_size(s).unwrap_or(0)
}

fn command(argv: &[&str]) -> Removal {
    Removal::Command {
        argv: argv.iter().map(|a| a.to_string()).collect(),
        cwd: None,
    }
}

/// One item per kind of leftover `docker system df` reports.
fn items(ctx: &ScanCtx, df: &DiskUsage) -> Vec<Item> {
    let item =
        |label: String, bytes: u64, status: Vec<Status>, safe: bool, removal: Removal| Item {
            id: ctx.next_id(),
            source: SourceId::Docker,
            label,
            path: None,
            size: Some(bytes),
            status,
            lock: None,
            safe,
            removal,
            age_days: None,
            recheck: crate::model::Recheck::default(),
        };
    let mut items = Vec::new();

    let unused: Vec<&Image> = df.images.iter().filter(|i| i.containers == "0").collect();
    let dangling: u64 = unused
        .iter()
        .filter(|i| i.repository == "<none>")
        .map(|i| size(&i.size))
        .sum();
    if dangling > 0 {
        items.push(item(
            "Dangling images".into(),
            dangling,
            vec![],
            true,
            command(&["docker", "image", "prune", "-f"]),
        ));
    }
    for img in unused.iter().filter(|i| i.repository != "<none>") {
        let short = img
            .id
            .trim_start_matches("sha256:")
            .chars()
            .take(12)
            .collect::<String>();
        items.push(item(
            format!("{}:{}", img.repository, img.tag),
            size(&img.unique_size),
            vec![Status::Detail("unused".into())],
            false,
            command(&["docker", "rmi", &short]),
        ));
    }

    let cache: u64 = df.build_cache.iter().map(|c| size(&c.size)).sum();
    if cache > 0 {
        items.push(item(
            "Build cache".into(),
            cache,
            vec![],
            true,
            command(&["docker", "builder", "prune", "-f"]),
        ));
    }

    let stopped: Vec<&Container> = df
        .containers
        .iter()
        .filter(|c| c.state != "running" && !c.status.starts_with("Up"))
        .collect();
    if !stopped.is_empty() {
        let bytes = stopped.iter().map(|c| size(&c.size)).sum();
        let detail = Status::Detail(format!("{} containers", stopped.len()));
        items.push(item(
            "Stopped containers".into(),
            bytes,
            vec![detail],
            false,
            command(&["docker", "container", "prune", "-f"]),
        ));
    }

    for vol in df.volumes.iter().filter(|v| v.links == "0") {
        let label = if vol.labels.contains("com.docker.volume.anonymous") {
            format!(
                "{} (anonymous)",
                vol.name.chars().take(12).collect::<String>()
            )
        } else {
            vol.name.clone()
        };
        items.push(item(
            label,
            size(&vol.size),
            vec![Status::Orphan],
            false,
            command(&["docker", "volume", "rm", &vol.name]),
        ));
    }
    items
}

pub struct Docker;

impl Docker {
    pub fn scan_with(
        &self,
        ctx: &ScanCtx,
        tx: &Sender<ScanEvent>,
        runner: Runner,
    ) -> anyhow::Result<()> {
        if runner(&["docker", "info"]).is_err() {
            let _ = tx.send(ScanEvent::Note(SourceId::Docker, "daemon stopped".into()));
            return Ok(());
        }
        let df: DiskUsage = serde_json::from_str(&runner(&[
            "docker",
            "system",
            "df",
            "-v",
            "--format",
            "{{json .}}",
        ])?)?;
        for it in items(ctx, &df) {
            let _ = tx.send(ScanEvent::Found(it));
        }
        Ok(())
    }
}

impl Scanner for Docker {
    fn source(&self) -> SourceId {
        SourceId::Docker
    }

    fn available(&self) -> bool {
        which("docker").is_some()
    }

    fn scan(&self, ctx: &ScanCtx, tx: &Sender<ScanEvent>) -> anyhow::Result<()> {
        self.scan_with(ctx, tx, &|argv| run(argv))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inuse::InUse;
    use crate::model::{Removal, Status};

    const DF: &str = include_str!("../../tests/fixtures/docker-df-v.json");

    fn scan(info_ok: bool, df: &str) -> (Vec<Item>, Vec<String>) {
        let ctx = ScanCtx::new("/tmp".into(), "/tmp".into(), InUse::default());
        let (tx, rx) = crossbeam_channel::unbounded();
        let df = df.to_string();
        let runner = move |argv: &[&str]| -> anyhow::Result<String> {
            match argv.get(1).copied() {
                Some("info") if info_ok => Ok(String::new()),
                Some("info") => anyhow::bail!("Cannot connect to the Docker daemon"),
                _ => Ok(df.clone()),
            }
        };
        Docker.scan_with(&ctx, &tx, &runner).unwrap();
        drop(tx);
        let (mut items, mut notes) = (vec![], vec![]);
        for e in rx {
            match e {
                ScanEvent::Found(i) => items.push(i),
                ScanEvent::Note(_, n) => notes.push(n),
                _ => {}
            }
        }
        (items, notes)
    }

    fn cmd(item: &Item) -> Vec<String> {
        match &item.removal {
            Removal::Command { argv, .. } => argv.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_human_size_variants() {
        assert_eq!(parse_human_size("1.623GB"), Some(1_623_000_000));
        assert_eq!(parse_human_size("512kB"), Some(512_000));
        assert_eq!(parse_human_size("0B"), Some(0));
        assert_eq!(parse_human_size("63B"), Some(63));
        assert_eq!(parse_human_size("N/A"), None);
    }

    #[test]
    fn daemon_down_emits_note_not_failed() {
        let (items, notes) = scan(false, DF);
        assert!(items.is_empty());
        assert_eq!(notes, vec!["daemon stopped".to_string()]);
    }

    #[test]
    fn dangling_images_item_is_safe_prune() {
        let (items, _) = scan(true, DF);
        let it = items.iter().find(|i| i.label == "Dangling images").unwrap();
        assert!(it.safe);
        assert_eq!(it.size, Some(300_000_000));
        assert_eq!(cmd(it), ["docker", "image", "prune", "-f"]);
    }

    #[test]
    fn build_cache_item_is_safe_prune() {
        let (items, _) = scan(true, DF);
        let it = items.iter().find(|i| i.label == "Build cache").unwrap();
        assert!(it.safe);
        assert_eq!(it.size, Some(150_000_000));
        assert_eq!(cmd(it), ["docker", "builder", "prune", "-f"]);
    }

    #[test]
    fn stopped_containers_item_not_safe() {
        let (items, _) = scan(true, DF);
        let it = items
            .iter()
            .find(|i| i.label == "Stopped containers")
            .unwrap();
        assert!(!it.safe);
        assert_eq!(cmd(it), ["docker", "container", "prune", "-f"]);
    }

    #[test]
    fn unused_tagged_image_is_listed_not_safe() {
        let (items, _) = scan(true, DF);
        let it = items.iter().find(|i| i.label == "mysql:8.4").unwrap();
        assert!(!it.safe);
        assert_eq!(it.size, Some(1_116_000_000));
        assert_eq!(cmd(it)[..2], ["docker", "rmi"]);
    }

    #[test]
    fn image_with_container_is_not_listed() {
        let (items, _) = scan(true, DF);
        assert!(!items.iter().any(|i| i.label.starts_with("postgres")));
    }

    #[test]
    fn each_orphan_volume_is_item_never_safe() {
        let (items, _) = scan(true, DF);
        let vols: Vec<_> = items
            .iter()
            .filter(|i| cmd(i)[..3] == ["docker", "volume", "rm"])
            .collect();
        assert_eq!(vols.len(), 2);
        assert!(
            vols.iter()
                .all(|v| !v.safe && v.status.contains(&Status::Orphan))
        );
        assert!(vols.iter().any(|v| v.label == "glowz_pgdata"));
        assert!(vols.iter().any(|v| v.label == "7c07e325aba6 (anonymous)"));
    }

    #[test]
    fn zero_size_groups_are_omitted() {
        let empty = r#"{"Images":[],"Containers":[],"Volumes":[],"BuildCache":[]}"#;
        let (items, _) = scan(true, empty);
        assert!(items.is_empty());
    }
}
