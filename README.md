# devsweep

Find and remove what a developer's Mac accumulates, in one terminal UI:
project build artifacts, git worktrees left behind by coding agents, iOS
simulators, Android emulators, Docker leftovers and dev tool caches.

```sh
npx @tarcisiopgs/devsweep            # scan the current folder and the machine
npx @tarcisiopgs/devsweep ~/code     # scan another folder
```

Or install it once and run `devsweep`:

```sh
npm install -g @tarcisiopgs/devsweep
```

macOS only (Apple silicon and Intel).

## Why another cleaner

Most cleaners either target one thing (`node_modules`) or clean a fixed list
of caches in bulk. devsweep understands the lifecycle of what developers
create, and lets you act item by item:

- **Worktrees** show whether they are merged, dirty or stale, and a worktree
  with a live process inside it (an agent, a shell, an editor) is **locked**
  and cannot be selected.
- **Simulators and emulators** show when they were last used; a booted
  simulator or running emulator is locked.
- **Docker** volumes that no container uses are listed, but never preselected:
  they may hold a database.
- Everything is removed with the tool's own command when one exists
  (`git worktree remove`, `xcrun simctl delete`, `avdmanager delete avd`,
  `docker … prune`, `pnpm store prune`, `brew cleanup`, Finder's empty trash).
- Nothing is deleted before a review screen that shows the total and the
  exact commands. Each item is checked again right before removal.

## What it finds

| Source | Section | Preselected when |
|---|---|---|
| Project artifacts: `node_modules`, `.venv`, `Pods`, `.next`, `.expo`, `.gradle`, `.cxx`, `target`, `vendor`, `__pycache__`…; `dist`/`build`/`out` only when git ignores them | This folder | never |
| Git worktrees of the repositories in the folder, outside the agent folders below | This folder | merged, clean, no ignored files beyond build artifacts, and unlocked |
| Agent worktrees in `~/.codex/worktrees`, `~/orca/workspaces`, `~/.claude-squad/worktrees` | Machine | merged, clean, no ignored files beyond build artifacts, and unlocked |
| iOS simulators and runtimes | Machine | the simulator is unavailable |
| Android AVDs and system images | Machine | never |
| Docker dangling images, build cache, unused images, stopped containers, orphan volumes | Machine | dangling images and build cache |
| Dev tool caches (npm, pnpm, bun, Yarn, pip, uv, Go, Cargo, Gradle, RubyGems, Expo, CocoaPods, Xcode, Clang, Playwright, Claude, Codex…) | Machine | the cache regenerates and costs nothing to rebuild |
| Xcode device support files and archives | Machine | never |
| Homebrew cleanup | Machine | always |
| The Trash, including mounted volumes' (emptied by Finder) | Machine | never |

**Safe** means: it regenerates by itself, nothing is lost, and recreating it
costs nothing relevant. Download caches regenerate too, but the next build
downloads everything again, so they are listed and left for you to decide.

## Keys

| Key | Action |
|---|---|
| `↑` `↓` / `j` `k` | move |
| `tab` / `←` `→` | switch pane |
| `space` | toggle item |
| `a` | toggle all unlocked items of the source |
| `s` | sort by size, name or age |
| `/` | filter |
| `r` | scan again |
| `?` | key and mark legend |
| `o` | open the Full Disk Access settings, when a source needs it |
| `⏎` | review what will be removed |
| `y` | confirm on the review screen |
| `q` | quit |

When a removal finishes, devsweep posts a macOS notification with the result.
Pass `--no-notify` to turn it off.

## How it compares

- [npkill](https://github.com/voidcosmos/npkill) and
  [kondo](https://github.com/tbillington/kondo) are great at project
  artifacts; devsweep covers them and adds the machine-level sources.
- [Mole](https://github.com/tw93/Mole) is a broad Mac cleaner (apps,
  browsers, system caches, uninstalls). Its dev cleanup runs in bulk and it
  only reports simulators, Docker volumes and agent worktrees. devsweep is
  narrower, focused on developer resources, and acts on those item by item.
  They work well together.

## Contributing

Cache rules are data in `catalog/*.toml`; see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT
