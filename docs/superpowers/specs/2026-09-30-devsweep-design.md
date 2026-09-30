# devsweep — Design

- **Date:** 2026-09-30
- **Status:** approved in brainstorm, pending written-spec review
- **Repo:** `tarcisiopgs/devsweep` (public, MIT)
- **Package:** `devsweep` on npm, run with `npx devsweep`

## 1. Intent

### What the user said

- Today disk space is reclaimed by running two CLIs in sequence: `npx npkill` (lists and deletes `node_modules`) and `npx mac-cleaner-cli` (caches, logs, Homebrew leftovers, Xcode DerivedData).
- Other developer resources also eat disk and have no good tool: dev caches, Android emulators, iOS simulators, Docker dangling images and other Docker data not tied to running containers, and git worktrees created by coding agents.
- One tool should cover all of it, in a friendly terminal UI, letting the user choose what to delete. The visual reference is the TUI of Lisa (`@tarcisiopgs/lisa`).
- Public, open source, installable from npm. The primary user is the author, who will run it often.

### Success criteria

- `npx devsweep` replaces both `npx npkill` and `npx mac-cleaner-cli` in the author's routine.
- Nothing is deleted without an explicit review step that shows the total and the exact commands.
- Nothing in active use (a worktree with a live agent, a booted simulator, a running emulator) can be selected.

### Measured baseline (author's machine, 2026-09-30)

| Resource | Size |
|---|---|
| iOS simulators (`CoreSimulator/Devices`) | 19 GB |
| Docker local volumes (97, none in use) | 14 GB |
| Gradle caches | 8.8 GB |
| pnpm store | 8.2 GB |
| bun cache | 4.6 GB |
| Docker images (99% reclaimable) | 1.6 GB |
| `~/.codex/worktrees` (14 worktrees) | 933 MB |
| Homebrew cache | 780 MB |
| Playwright browsers | 557 MB |
| Go module cache | 500 MB |
| npm cache | 450 MB |

## 2. Decisions

| Topic | Decision |
|---|---|
| Worktrees | All are listed with visible status: merged, dirty, stale N days. Dirty ones are never preselected. |
| Worktrees in progress | A worktree with a live process whose cwd is inside it (agent, shell, editor) is shown **locked**, with the reason, and cannot be selected. |
| Scope of the UI | One TUI with two sections: **This folder** (depends on cwd) and **Machine** (independent of cwd). |
| Removal | Native command whenever one exists; plain directories are removed permanently. A summary with total size and exact commands is shown before anything runs. |
| Preselection | Only items that are safe are preselected (see §4). |
| Stack | Rust + ratatui, distributed through npm with per-platform binaries. |
| Name | `devsweep` (free on npm and GitHub on 2026-09-30). |
| Visual authority | Lisa's TUI (no PRODUCT.md/DESIGN.md existed in either repo). |

## 3. Architecture

A single binary crate, split into modules with one responsibility each:

```
src/
  main.rs          # CLI (clap): target dir, flags; starts the TUI
  model.rs         # Item, Section, Status, Lock, Removal
  scan/
    mod.rs         # Scanner trait + parallel orchestration
    node_modules.rs
    worktrees.rs
    caches.rs      # package manager caches, logs (mac-cleaner-cli parity)
    xcode.rs       # DerivedData, iOS DeviceSupport, Archives
    android.rs     # AVDs + system images
    ios.rs         # simulators + runtimes
    docker.rs
    homebrew.rs
  inuse.rs         # lsof → map of paths with a live process
  remove.rs        # Executor trait; runs Removal, reports progress
  app.rs           # TUI state (selection, filter, focus, sort)
  ui/              # ratatui widgets
```

### Core contract

```rust
trait Scanner: Send {
    fn section(&self) -> Section;              // Folder | Machine
    fn scan(&self, ctx: &ScanCtx, tx: Sender<ScanEvent>);
}

enum ScanEvent {
    Found(Item),
    Size(ItemId, u64),
    Done(SourceId),
    Failed(SourceId, String),
}

struct Item {
    id: ItemId,
    source: SourceId,
    label: String,
    path: Option<PathBuf>,
    size: Option<u64>,        // arrives asynchronously
    status: Vec<Status>,      // Merged, Dirty(n), Stale(days), Booted, Unavailable, Orphan, LastUsed(date)
    lock: Option<String>,     // e.g. "claude · PID 4821" → not selectable
    safe: bool,               // true → preselected
    removal: Removal,
}

enum Removal {
    RemoveDir(PathBuf),
    Command { argv: Vec<String>, cwd: Option<PathBuf> },
}
```

`ScanCtx` carries the target directory, the `$HOME` path and the in-use map from `inuse.rs`.

### Data flow

1. `main` runs `inuse.rs` once (`lsof -d cwd -Fpcn`) and builds a map of path → process (name, PID).
2. Every scanner starts on its own thread and sends `ScanEvent`s over a single channel.
3. The TUI loop drains the channel and redraws. The TUI never calls a scanner.
4. The `node_modules` walk uses the `ignore` crate (parallel walker, ripgrep's engine) and does not descend into a `node_modules` it has found. Directory sizes are computed on a separate pool and arrive as `Size` events, so rows appear before their size.
5. Each scanner fills `lock` by checking whether any live-process cwd is inside the item's path.

## 4. Sources

Safe (preselected) means: **it regenerates by itself, loses no data, and costs nothing relevant to recreate.**

| Source | Section | Detection | Status / lock | Safe | Removal |
|---|---|---|---|---|---|
| `node_modules` | Folder | walk from target dir, not descending into found `node_modules` | lock if a live process cwd is inside the owning project | no | `RemoveDir` |
| Worktrees | Folder + Machine | Folder: `git worktree list --porcelain` for each repo found. Machine: `~/.codex/worktrees`, `~/orca/workspaces`, `~/.claude-squad/worktrees`, `*/.claude/worktrees` | Merged (branch merged into default branch or deleted on remote), Dirty (uncommitted changes), Stale N days (last commit/mtime); lock via `lsof` | only **merged + clean + unlocked** | `git worktree remove <path>` then `git worktree prune`, run in the main repo |
| iOS simulators | Machine | `xcrun simctl list devices -j` | Booted = lock; Unavailable; last used | only **unavailable** | `xcrun simctl delete <udid>` |
| iOS runtimes | Machine | `xcrun simctl runtime list -j` | no simulator using it | no | `xcrun simctl runtime delete <id>` |
| Android AVDs | Machine | `avdmanager list avd` (SDK from `$ANDROID_HOME` or `~/Library/Android/sdk`) | running emulator = lock; last used | no | `avdmanager delete avd -n <name>` |
| Android system images | Machine | `<sdk>/system-images/*/*/*` | Orphan (no AVD references it) | yes, if orphan | `RemoveDir` |
| Docker images / build cache / stopped containers | Machine | `docker system df -v` | anything tied to a running container is excluded | **dangling images + build cache** | `docker image prune -f`, `docker builder prune -f`, `docker container prune -f` |
| Docker volumes | Machine | same | Orphan (no container) | **never** (may hold database data) | `docker volume rm <name>` |
| Package caches | Machine | npm, pnpm, bun, yarn, gradle, cargo registry, Go modules, CocoaPods, Playwright | size | only **`pnpm store prune`** and **`npm cache verify`** (they remove only unreferenced content) | native command when one exists, otherwise `RemoveDir` |
| Xcode | Machine | DerivedData, iOS DeviceSupport, Archives | size | DerivedData yes; DeviceSupport no; Archives **never** | `RemoveDir` |
| Homebrew | Machine | `brew cleanup -n` | size | yes | `brew cleanup` |
| Logs | Machine | `~/Library/Logs` | size | yes | `RemoveDir` on children |

### Cross-cutting rules

- A source whose tool is not installed (no Docker, no Android SDK, no Xcode) is not shown. It is not an error.
- Docker installed but daemon down: the source is shown with a gray "daemon stopped" note and no items.
- The in-use check applies to **every** item with a path, not only worktrees.
- Worktrees found in the Folder section are not repeated in Machine.

## 5. TUI

Lisa is the visual authority: yellow for focus and accent, gray/dim for secondary info, white bold for titles, green for success, red for errors, single borders.

### Main layout

Source sidebar on the left (Lisa's `sidebar.tsx` pattern), item list for the focused source on the right, status bar at the bottom.

```
┌ devsweep ─ ~/Workspace ─────────────────────────────────────────────────────┐
│ THIS FOLDER           │ Worktrees · 18 items                     sort: size │
│ ▸ node_modules  14.2G │ ─────────────────────────────────────────────────── │
│   Worktrees      2.1G │ [x] codex/glowz-robots       merged · clean    412M │
│                       │ [x] codex/customer-module    merged · clean    388M │
│ MACHINE               │ [ ] codex/glowz-dev-deploy   dirty · 3 files   301M │
│   Simulators    19.0G │ [ ] orca/website/arowana     ⊘ claude · PID 4821 290M │
│   Docker        15.6G │ [ ] codex/090b               stale 41d          95M │
│   Caches        23.7G │                                                     │
│   Android        ···  │                                                     │
│   Xcode          0B   │                                                     │
│   Homebrew      780M  │                                                     │
├───────────────────────┴─────────────────────────────────────────────────────┤
│ 7 selected · 1.9 GB   space toggle · tab pane · a all · / filter · ⏎ review · q quit │
└─────────────────────────────────────────────────────────────────────────────┘
```

UI copy language is English (public tool).

### Color roles

| Role | Color |
|---|---|
| Focus: active pane border, cursor row | yellow |
| Titles, item names | white bold |
| Metadata | gray / dim |
| Selected, space freed | green |
| Dirty, errors | red |
| Informational ("stale 41d") | cyan |

A locked row is fully dim, shows `⊘` and the reason in place of the status, and the cursor passes over it without being able to toggle it.

### States

- **Scanning:** the source shows a spinner (`throbber-widgets-tui`) instead of its total; items stream in; size column shows `…` until the size arrives. Navigation and selection work during the scan.
- **Empty / missing:** zero items → source dim with `0B`. Tool missing → source hidden. Docker daemon down → gray "daemon stopped".
- **Review (⏎):** full screen summary grouped by source, with total to free and the exact commands that will run. `y` confirms; any other key goes back.
- **Removing:** the same screen turns into progress: spinner → green `✓ 412M` or red `✗ reason`. One failure does not stop the rest.
- **Done:** "Freed 23.1 GB in 14 items", followed by the list of failures if any. `q` quits, `r` rescans.
- **Narrow terminal (< 80 cols):** the sidebar becomes a tab row at the top and the list takes the full width.

### Keys

| Key | Action |
|---|---|
| `↑` `↓` / `j` `k` | move |
| `tab` | switch pane |
| `space` | toggle item |
| `a` | toggle all unlocked items of the source |
| `s` | cycle sort: size / name / age |
| `/` | text filter |
| `⏎` | review |
| `q` | quit |

No mouse in v1.

## 6. Error handling

- **Failing scanner** (non-zero exit, unexpected JSON, permission denied): sends `Failed`; the source shows in red with a short message; other sources continue. No panic may bring down the TUI.
- **macOS privacy (TCC):** unreadable protected folders are skipped silently and counted; at the end of the scan a gray note says "N folders skipped (no permission)".
- **Revalidation before removal:** each item is re-checked right before its removal: path still exists, still not locked, worktree still clean. If anything changed since the scan, the item becomes `✗ changed since scan` and is not touched.
- **Path guard:** `RemoveDir` only accepts absolute paths inside `$HOME` or known tool roots, never `$HOME` itself nor a source root. This protects against scanner bugs.
- **Terminal restore:** raw mode and alternate screen are restored on every exit path: `q`, Ctrl-C, and a panic hook.

## 7. Testing

- **Parsers:** real outputs of `simctl list -j`, `simctl runtime list -j`, `docker system df -v`, `git worktree list --porcelain`, `avdmanager list avd` and `lsof -Fpcn` are captured once as fixtures; parsers are tested against them without the tools installed.
- **Worktrees and `node_modules`:** integration tests in a temp directory with real git repos created by the test (merged, dirty, stale, locked via a child process with that cwd).
- **Safe and lock rules:** pure unit tests on `Item` construction.
- **TUI:** snapshot tests with ratatui's `TestBackend` and `insta` for every state in §5: scanning, list, locked row, review, progress, done, narrow terminal.
- **Removal:** an `Executor` trait; tests use a fake executor that records commands. Nothing is deleted in CI.

## 8. Distribution

- `cargo-dist` generates the GitHub Actions release on tag push: binaries for `aarch64-apple-darwin` and `x86_64-apple-darwin`, the npm package `devsweep` with a small JS shim, and per-platform packages as `optionalDependencies` (Biome/esbuild pattern). `npx devsweep` downloads only the binary for the current Mac.
- PR CI on `macos-latest`: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.
- License: MIT.

## 9. Out of scope for v1

- Linux and Windows.
- Mouse support.
- Config file or ignore list.
- Non-interactive mode (`--yes`, JSON output) and scheduling.
