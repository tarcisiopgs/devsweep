//! iOS simulators and simulator runtimes, through `xcrun simctl`.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::model::{Item, Removal, SourceId, Status};
use crate::scan::{ScanCtx, ScanEvent, Scanner, Sender, run, size_later, which};

#[derive(Deserialize)]
struct DeviceList {
    devices: HashMap<String, Vec<Device>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Device {
    udid: String,
    name: String,
    state: String,
    is_available: bool,
    data_path: Option<String>,
    data_path_size: Option<u64>,
    last_used_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Runtime {
    identifier: String,
    runtime_identifier: Option<String>,
    deletable: Option<bool>,
    size_bytes: Option<u64>,
    last_used_at: Option<String>,
}

/// `com.apple.CoreSimulator.SimRuntime.iOS-27-0` → `iOS 27.0`.
fn os_name(runtime_id: &str) -> String {
    let tail = runtime_id.rsplit('.').next().unwrap_or(runtime_id);
    match tail.split_once('-') {
        Some((platform, version)) => format!("{platform} {}", version.replace('-', ".")),
        None => tail.to_string(),
    }
}

/// Days between an ISO-8601 UTC timestamp (`2026-09-29T11:06:41Z`) and `now`.
pub fn days_ago_from(iso: &str, now_secs: u64) -> Option<u32> {
    let date = iso.get(..10)?;
    let mut parts = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (parts.next()??, parts.next()??, parts.next()??);
    // Days from civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let now_days = (now_secs / 86_400) as i64;
    Some((now_days - days).max(0) as u32)
}

fn days_ago(iso: &str) -> Option<u32> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    days_ago_from(iso, now)
}

pub fn devices_from_json(json: &str, ctx: &ScanCtx) -> anyhow::Result<Vec<Item>> {
    let list: DeviceList = serde_json::from_str(json)?;
    let mut items = Vec::new();
    for (runtime, devices) in list.devices {
        for dev in devices {
            let last_used = dev.last_used_at.as_deref().and_then(days_ago);
            let mut status = Vec::new();
            if !dev.is_available {
                status.push(Status::Unavailable);
            }
            if dev.state == "Booted" {
                status.push(Status::Booted);
            }
            if let Some(days) = last_used {
                status.push(Status::LastUsed(days));
            }
            let lock = (dev.state == "Booted").then(|| "booted".to_string());
            items.push(Item {
                id: ctx.next_id(),
                source: SourceId::Ios,
                label: format!("{} · {}", dev.name, os_name(&runtime)),
                path: dev.data_path.as_ref().map(|p| {
                    PathBuf::from(p)
                        .parent()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from(p))
                }),
                size: dev.data_path_size,
                status,
                safe: !dev.is_available && lock.is_none(),
                lock,
                removal: Removal::Command {
                    argv: vec!["xcrun".into(), "simctl".into(), "delete".into(), dev.udid],
                    cwd: None,
                },
                age_days: last_used,
                recheck: crate::model::Recheck::default(),
            });
        }
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(items)
}

/// Runtime identifiers that still have at least one simulator.
pub fn runtimes_in_use(devices_json: &str) -> anyhow::Result<HashSet<String>> {
    let list: DeviceList = serde_json::from_str(devices_json)?;
    Ok(list
        .devices
        .into_iter()
        .filter(|(_, d)| !d.is_empty())
        .map(|(k, _)| k)
        .collect())
}

pub fn runtimes_from_json(
    json: &str,
    used: &HashSet<String>,
    ctx: &ScanCtx,
) -> anyhow::Result<Vec<Item>> {
    let map: HashMap<String, Runtime> = serde_json::from_str(json)?;
    let mut items: Vec<Item> = map
        .into_values()
        .filter(|rt| rt.deletable != Some(false))
        .map(|rt| {
            let runtime_id = rt.runtime_identifier.clone().unwrap_or_default();
            let last_used = rt.last_used_at.as_deref().and_then(days_ago);
            let mut status = Vec::new();
            if !used.contains(&runtime_id) {
                status.push(Status::Orphan);
            }
            if let Some(days) = last_used {
                status.push(Status::LastUsed(days));
            }
            Item {
                id: ctx.next_id(),
                source: SourceId::Ios,
                label: format!("{} runtime", os_name(&runtime_id)),
                path: None,
                size: rt.size_bytes,
                status,
                lock: None,
                safe: false,
                removal: Removal::Command {
                    argv: vec![
                        "xcrun".into(),
                        "simctl".into(),
                        "runtime".into(),
                        "delete".into(),
                        rt.identifier,
                    ],
                    cwd: None,
                },
                age_days: last_used,
                recheck: crate::model::Recheck::default(),
            }
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(items)
}

pub struct Ios;

impl Scanner for Ios {
    fn source(&self) -> SourceId {
        SourceId::Ios
    }

    fn available(&self) -> bool {
        which("xcrun").is_some() && run(&["xcrun", "simctl", "help"]).is_ok()
    }

    fn scan(&self, ctx: &ScanCtx, tx: &Sender<ScanEvent>) -> anyhow::Result<()> {
        let devices_json = run(&["xcrun", "simctl", "list", "devices", "-j"])?;
        for item in devices_from_json(&devices_json, ctx)? {
            let pending = item.size.is_none().then(|| (item.path.clone(), item.id));
            let _ = tx.send(ScanEvent::Found(item));
            if let Some((Some(path), id)) = pending {
                size_later(path, id, tx.clone());
            }
        }
        let used = runtimes_in_use(&devices_json)?;
        if let Ok(runtimes_json) = run(&["xcrun", "simctl", "runtime", "list", "-j"]) {
            for item in runtimes_from_json(&runtimes_json, &used, ctx)? {
                let _ = tx.send(ScanEvent::Found(item));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inuse::InUse;
    use crate::model::{Removal, Status};
    use std::path::PathBuf;

    const DEVICES: &str = include_str!("../../tests/fixtures/simctl-devices.json");
    const RUNTIMES: &str = include_str!("../../tests/fixtures/simctl-runtimes.json");

    fn ctx() -> ScanCtx {
        ScanCtx::new(
            PathBuf::from("/tmp"),
            PathBuf::from("/Users/u"),
            InUse::default(),
        )
    }

    fn devices() -> Vec<Item> {
        devices_from_json(DEVICES, &ctx()).unwrap()
    }

    #[test]
    fn booted_device_is_locked() {
        let d = devices();
        let booted = d
            .iter()
            .find(|i| i.label.starts_with("iPhone 18 Pro Max"))
            .unwrap();
        assert_eq!(booted.lock.as_deref(), Some("booted"));
        assert!(!booted.safe);
    }

    #[test]
    fn unavailable_device_is_safe() {
        let d = devices();
        let gone = d
            .iter()
            .find(|i| i.label.starts_with("iPhone 17e"))
            .unwrap();
        assert!(gone.status.contains(&Status::Unavailable));
        assert!(gone.safe);
    }

    #[test]
    fn available_device_is_not_safe_and_has_last_used() {
        let d = devices();
        let dev = d
            .iter()
            .find(|i| i.label.starts_with("iPhone 18 Pro ·"))
            .unwrap();
        assert!(!dev.safe);
        assert!(dev.status.iter().any(|s| matches!(s, Status::LastUsed(_))));
        assert!(dev.size.is_some_and(|s| s > 0));
    }

    #[test]
    fn device_label_is_name_and_os() {
        assert!(
            devices()
                .iter()
                .any(|i| i.label == "iPhone 18 Pro · iOS 27.0")
        );
    }

    #[test]
    fn device_removal_is_simctl_delete_udid() {
        let d = devices();
        let dev = d
            .iter()
            .find(|i| i.label == "iPhone 18 Pro · iOS 27.0")
            .unwrap();
        match &dev.removal {
            Removal::Command { argv, .. } => {
                assert_eq!(&argv[..3], &["xcrun", "simctl", "delete"]);
                assert_eq!(argv[3].len(), 36);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn runtime_without_devices_is_orphan_not_safe() {
        let used = runtimes_in_use(DEVICES).unwrap();
        let rts = runtimes_from_json(RUNTIMES, &used, &ctx()).unwrap();
        let orphan = rts.iter().find(|i| i.label == "iOS 18.2 runtime").unwrap();
        assert!(orphan.status.contains(&Status::Orphan));
        assert!(!orphan.safe);
        match &orphan.removal {
            Removal::Command { argv, .. } => {
                assert_eq!(&argv[..4], &["xcrun", "simctl", "runtime", "delete"])
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn runtime_with_devices_is_not_orphan() {
        let used = runtimes_in_use(DEVICES).unwrap();
        let rts = runtimes_from_json(RUNTIMES, &used, &ctx()).unwrap();
        let rt = rts.iter().find(|i| i.label == "iOS 27.0 runtime").unwrap();
        assert!(!rt.status.contains(&Status::Orphan));
    }

    #[test]
    fn iso_date_to_days_ago() {
        let now = 1_790_000_000u64; // fixed "now"
        assert_eq!(
            days_ago_from("1970-01-02T00:00:00Z", now),
            Some((now / 86_400 - 1) as u32)
        );
        assert_eq!(days_ago_from("garbage", now), None);
    }
}
