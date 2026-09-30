use std::path::PathBuf;

use clap::Parser;

/// Find and remove what a developer's Mac accumulates: build artifacts,
/// agent worktrees, simulators, emulators, Docker leftovers and dev caches.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Folder to scan for project artifacts and worktrees (defaults to the current directory).
    path: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let target = match cli.path {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let target = std::fs::canonicalize(&target)?;
    println!("{}", target.display());
    Ok(())
}
