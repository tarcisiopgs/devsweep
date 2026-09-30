use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Parser;
use crossbeam_channel::{Receiver, TryRecvError, unbounded};
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::DefaultTerminal;

use devsweep::app::{Action, App, Screen};
use devsweep::fsutil::disk_free;
use devsweep::inuse::InUse;
use devsweep::model::Item;
use devsweep::remove::{Guard, RealExecutor, RemoveEvent, default_recheck, run_removals_until};
use devsweep::scan::worktrees::AGENT_ROOTS;
use devsweep::scan::{ScanCtx, ScanEvent, all_scanners, run, spawn_all};

/// Find and remove what a developer's Mac accumulates: build artifacts,
/// agent worktrees, simulators, emulators, Docker leftovers and dev caches.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Folder to scan for project artifacts and worktrees (defaults to the current directory).
    path: Option<PathBuf>,
    /// Do not post a macOS notification when a removal finishes.
    #[arg(long)]
    no_notify: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let target = match cli.path {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let target = std::fs::canonicalize(&target)?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let home = std::fs::canonicalize(&home).unwrap_or(home);

    // ratatui::init installs a panic hook that restores the terminal.
    let mut terminal = ratatui::init();
    let result = run_app(&mut terminal, target, home, !cli.no_notify);
    ratatui::restore();
    result
}

/// Start every available scanner; returns the app state and its event stream.
fn start_scan(
    target: &Path,
    home: &Path,
    app: Option<&mut App>,
) -> (Option<App>, Receiver<ScanEvent>) {
    let scanners: Vec<_> = all_scanners(home)
        .into_iter()
        .filter(|s| s.available())
        .collect();
    let sources = scanners.iter().map(|s| s.source()).collect();
    let ctx = Arc::new(ScanCtx::new(
        target.to_path_buf(),
        home.to_path_buf(),
        InUse::collect(),
    ));
    let (tx, rx) = unbounded();
    spawn_all(ctx, scanners, tx);
    match app {
        Some(_) => (None, rx),
        None => (Some(App::new(target.to_path_buf(), sources)), rx),
    }
}

fn start_removal(
    items: Vec<Item>,
    target: &Path,
    home: &Path,
) -> (Receiver<RemoveEvent>, Arc<AtomicBool>) {
    let protected = [
        "",
        "Library",
        "Library/Caches",
        "Library/Developer",
        "Library/Logs",
    ]
    .iter()
    .map(|p| home.join(p))
    .chain(AGENT_ROOTS.iter().map(|r| home.join(r)))
    .collect();
    let extra: Vec<PathBuf> = run(&["getconf", "DARWIN_USER_CACHE_DIR"])
        .ok()
        .map(|d| PathBuf::from(d.trim()))
        .filter(|p| p.is_absolute())
        .into_iter()
        .collect();
    let guard = Guard::new(home.to_path_buf(), target.to_path_buf(), protected, extra);
    let (tx, rx) = unbounded();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    std::thread::spawn(move || {
        let recheck = default_recheck(InUse::collect);
        run_removals_until(items, &RealExecutor, &guard, &recheck, &stop_flag, tx);
    });
    (rx, stop)
}

fn run_app(
    terminal: &mut DefaultTerminal,
    target: PathBuf,
    home: PathBuf,
    notify: bool,
) -> anyhow::Result<()> {
    let (app, mut scan_rx) = start_scan(&target, &home, None);
    let mut app = app.expect("fresh app");
    let mut remove_rx: Option<(Receiver<RemoveEvent>, Arc<AtomicBool>)> = None;

    loop {
        for ev in scan_rx.try_iter() {
            app.on_scan(ev);
        }
        if let Some((rx, _)) = &remove_rx {
            let events: Vec<RemoveEvent> = rx.try_iter().collect();
            // The thread is gone without `Finished` (it panicked).
            let vanished = events.is_empty()
                && app.screen == Screen::Removing
                && matches!(rx.try_recv(), Err(TryRecvError::Disconnected));
            if vanished {
                app.removal_aborted();
            }
            for ev in events {
                let finished = matches!(ev, RemoveEvent::Finished);
                if finished {
                    app.disk_free.1 = disk_free(&home);
                }
                let action = app.on_remove(ev);
                if finished && notify {
                    devsweep::notify::post("devsweep", &app.completion_summary());
                    // The bell marks the terminal tab, too.
                    let _ = std::io::stdout().write_all(b"\x07");
                    let _ = std::io::stdout().flush();
                }
                if action == Action::Quit {
                    return Ok(());
                }
            }
        }
        app.spinner_tick = app.spinner_tick.wrapping_add(1);
        terminal.draw(|f| devsweep::ui::draw(f, &app))?;

        if !event::poll(Duration::from_millis(80))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match app.on_key(key) {
            Action::None => {}
            Action::Quit => return Ok(()),
            Action::StartRemoval(items) => {
                app.disk_free.0 = disk_free(&home);
                remove_rx = Some(start_removal(items, &target, &home));
            }
            Action::StopRemoval => {
                if let Some((_, stop)) = &remove_rx {
                    stop.store(true, Ordering::SeqCst);
                }
            }
            Action::Rescan => {
                remove_rx = None;
                scan_rx = start_scan(&target, &home, Some(&mut app)).1;
            }
        }
    }
}
