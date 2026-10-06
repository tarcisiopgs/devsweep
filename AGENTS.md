# AGENTS.md

Guidance for AI coding agents (Claude Code, Codex, Cursor, Gemini CLI, OpenCode…) working in this repository. This is the single source of instructions.

## What this is

devsweep is a Rust + ratatui terminal UI for macOS, Linux and Windows that finds what a developer's machine accumulates and removes what the user picks: project build artifacts and app builds (`.ipa`, `.apk`, `.aab`), git worktrees (including the ones coding agents leave behind) and the merged branches they leave in the repository, iOS simulators and runtimes, Android AVDs and system images, Docker leftovers, dev tool caches, Xcode data, Homebrew cleanup and the Trash. iOS, Xcode and Homebrew only exist on macOS. Windows support is experimental: it is validated by the test suite on a Windows runner, not on a real machine.

It deletes user data. **Correctness of what can be selected and removed outranks every other concern**, including speed and features.

Product context lives in `PRODUCT.md`. The user-facing overview is in `README.md`, and the contributor rules are in `CONTRIBUTING.md`.

## Commands

```sh
cargo test                                   # the whole suite, including insta snapshots
cargo clippy --all-targets -- -D warnings    # must be clean, CI enforces it
cargo fmt                                    # CI runs cargo fmt --check
cargo insta review                           # accept or reject changed TUI snapshots
cargo run -- ~/some/folder                   # run the TUI against a folder
scripts/test-linux.sh                        # the suite on Linux, in a container, as a regular user (needs Docker)
scripts/build-linux.sh                       # the static Linux release binary, in the same container
scripts/check-windows.sh                     # type-check and lint for Windows, in the same container (the Windows suite only runs in CI)
prek install -t pre-commit -t pre-push       # git hooks: fmt + clippy on commit, tests on push
```

CI (`.github/workflows/ci.yml`) runs for every pull request, in separate jobs: Lint (fmt and clippy), Typecheck (`cargo check`) and Test, each on `macos-latest`, `ubuntu-latest` and `windows-latest`, and Build (the release build for the two macOS, the two Linux and the two Windows targets). The `check` job only passes when all of them pass, and it is the required status check on `main`.

Never confirm a removal (`y` on the review screen) while testing against a real machine unless the user asks for it in that conversation.

## Layout

```
src/main.rs          CLI (clap) and the event loop: scan channel, removal channel, keys
src/model.rs         Item, SourceId, Section, Status, Removal, Recheck, size formatting
src/inuse.rs         lsof snapshot → which paths have a live process; excludes devsweep's own process chain
src/fsutil.rs        dir_size (never follows symlinks), days_since
src/scan/mod.rs      Scanner trait, ScanCtx, ScanEvent, spawn_all, run / run_timeout, which
src/scan/*.rs        one scanner per source (artifacts, worktrees, ios, android, docker, catalog, xcode, homebrew)
catalog/*.toml       declarative dev tool cache rules, embedded with include_str!
src/remove.rs        Executor (real + fake in tests), Guard, default_recheck, run_removals
src/app.rs           App: a pure reducer over ScanEvent, RemoveEvent and key events
src/ui/              rendering only: list.rs (main screen), review.rs (review, progress, done)
tests/fixtures/      captured real outputs of simctl, docker, lsof, git, brew
npm/                 npm shim package and the platform package template
```

## Architecture rules

- **Scanners never talk to the UI.** Each one runs on its own thread and only sends `ScanEvent`s. `Found` rows appear before their size, which arrives later as `Size`. A scanner that errors or panics becomes `Failed` for its source, and the rest keep going.
- **`App` is a pure reducer.** It takes no I/O, so every behaviour is testable without a terminal. The UI only reads `App`.
- **Adding a source** means a new scanner file, a `SourceId` variant with its label and section, and registration in `all_scanners`. Adding a dev tool cache is only a TOML rule (see below).
- **Platform differences** live in `src/platform.rs` as plain functions of `Os`, so the rules of every system are testable on any of them. `#[cfg]` is only for code that does not compile elsewhere (Unix metadata, the Windows process snapshot). A test that depends on the system says which one (`ScanCtx::with_os`, `app.os`); a test gated with `#[cfg(unix)]` or `#[cfg(windows)]` carries the reason next to the gate.
- **External tools** are called with argument vectors, never through a shell. Anything that can hang (git, notably when macOS asks for folder permission) goes through `run_timeout`.

## Safety invariants (do not weaken)

1. A locked item is never selectable, not even with `a`. Locks come from a live process inside the item's scope (`lsof`), a booted simulator, a running emulator, or a running process named in the rule's `busy_when`.
2. `safe` (preselected) only when the data regenerates by itself, nothing is lost, and recreating it costs nothing relevant. Download caches, Docker volumes, Xcode archives, AVDs, runtimes, project artifacts and app builds are never `safe`.
3. Nothing is removed without the review screen and `y`. The review lists the exact command of every item, and once it is open, new scan results cannot join the selection.
4. Right before each removal, `default_recheck` re-checks the same scope and busy names the scan used (`Item.recheck`). A path that changed is reported as `changed since scan` and left alone.
5. `Guard` limits directory removals to `$HOME`, the scanned folder (unless it is an ancestor of `$HOME`) and explicit extra roots, and refuses the roots and protected folders themselves.
6. A worktree is `broken` (and removed as a folder) only when its `gitdir:` target no longer exists and the place it lived in is certainly there: the closest existing folder resolves, holds something, is not the root of a volume and sits on a mounted volume. A relative `gitdir:`, a dangling symlink, an empty mount point or a path written by another system (a Windows drive read on Unix, a WSL or container path) proves nothing. When git fails or times out, the worktree is "status unknown", locked, and never removed.
7. A branch left behind by a removed worktree is offered only when deleting it loses nothing: its tip is in the default branch, or its whole diff landed there as one commit (a verified squash merge). An upstream that is merely gone is never enough, and the default branch is never offered. The branch goes through the review like any item, and the recheck refuses it when it no longer points at the commit that was verified.
8. A folder holding files git tracks is never an artifact, whatever its name (a committed `vendor` or `Pods`). When git does not answer, the folder is skipped and a note says so.
9. Prefer each tool's native removal command (`git worktree remove`, `xcrun simctl delete`, `docker … prune`, `pnpm store prune`, `brew cleanup`) over deleting files behind the tool's back.

Any change touching selection, locks, `safe`, the guard or removal needs a test that fails without it.

## Dev cache catalog

Rules live in `catalog/*.toml`, one file per ecosystem. The schema and the meaning of `safe` are documented in `CONTRIBUTING.md`. Every rule needs a `source` (`docs: <url>` or `observed on disk`), and the catalog tests fail without it.

**Clean-room policy:** devsweep is MIT. Never read, copy or translate the source of GPL cleaners such as Mole, not even to check how they did something. Facts are fine: documented paths, what exists on disk, and public bug reports that describe a pitfall.

## TUI conventions

- The visual reference is the author's Lisa TUI. Colors are the 16 ANSI names:
  - yellow: focus and cursor;
  - white bold: titles and names;
  - gray/dim: metadata;
  - green: selected and freed space;
  - red: dirty items and errors;
  - cyan: informational status.
- Every screen state has an insta snapshot in `src/ui/snapshots/`. When a render changes, review each snapshot with `cargo insta review` before accepting it; never bulk-accept.
- Handle small terminals: under 40×10 the UI shows "Terminal too small", and under 80 columns the sidebar becomes a tab row.
- UI copy, code, comments, README and all git text are in English.

## Tests

- Write the test first and watch it fail, then implement.
- Parsers are tested against real captured outputs in `tests/fixtures/`. Anonymize home paths to `/Users/u` before committing a fixture.
- Git and filesystem behaviour is tested on real temporary repos and folders (`tempfile`), not mocks.
- Removal is tested through the fake `Executor`, and nothing in the suite deletes outside a temp dir.
- Before a pull request, run the suite on Linux (`scripts/test-linux.sh`) and the Windows type-check (`scripts/check-windows.sh`) too. The Windows suite itself only runs in CI; a Mac cannot run a Windows container.

## Git and pull requests

- `main` is protected: every change goes through a branch and a pull request, the `check` job must pass, and nobody pushes to `main` directly (admins included).
- Branch names, commit messages, PR titles and PR descriptions are in English. Commits follow Conventional Commits (`feat:`, `fix:`, `ci:`, `docs:`, `chore:`…).

## Releasing

1. Bump `version` in `Cargo.toml` (and run `cargo check` to refresh `Cargo.lock`) in a pull request, and merge it.
2. Publish a GitHub Release tagged with that version in semver, with GitHub-generated notes:

   ```sh
   gh release create vX.Y.Z --generate-notes
   ```

`.github/workflows/release.yml` runs on `release: published`:
- it checks that the tag matches `Cargo.toml`;
- it builds `aarch64-apple-darwin`, `x86_64-apple-darwin`, `aarch64-unknown-linux-musl`, `x86_64-unknown-linux-musl`, `aarch64-pc-windows-msvc` and `x86_64-pc-windows-msvc` and attaches the archives to the release (`.tar.xz`, and `.zip` for Windows);
- it publishes `@tarcisiopgs/devsweep` and the platform packages `devsweep-darwin-arm64`, `devsweep-darwin-x64`, `devsweep-linux-arm64`, `devsweep-linux-x64`, `@tarcisiopgs/devsweep-win32-arm64` and `@tarcisiopgs/devsweep-win32-x64` to npm through trusted publishing (OIDC).

Each platform package needs its own trusted publisher on npmjs.com (repository `tarcisiopgs/devsweep`, workflow `release.yml`), configured once after the package exists. A new package name is therefore published once by hand (a `0.0.0` placeholder) before the first release that ships it; `npm trust github <package> --repo tarcisiopgs/devsweep --file release.yml` then configures the publisher. The Windows packages are scoped because npm's spam detection rejects the unscoped `devsweep-win32-*` names.

No npm token exists. npm trusts the workflow by its file name, so do not rename `release.yml`.
