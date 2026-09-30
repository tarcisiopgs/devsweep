# Product

<!-- impeccable:product-schema 1 -->

## Platform

terminal (TUI, macOS). The schema's `web`/`ios`/`android`/`adaptive` values do not apply; this is a ratatui terminal app.

## Stack

Rust + ratatui + crossterm, distributed on npm as per-platform binaries via cargo-dist (user decision, 2026-09-30).

## Users

Developers on macOS who juggle many projects, package managers, mobile toolchains and coding agents, and periodically run out of disk. The primary user is the author, who today runs `npx npkill` and `npx mac-cleaner-cli` back to back to reclaim space.

## Product Purpose

devsweep finds everything a developer's machine accumulates that can go — `node_modules`, git worktrees, iOS simulators and runtimes, Android AVDs and system images, Docker leftovers, package manager caches, Xcode data, Homebrew and logs — and lets the user pick what to delete in one terminal UI. Success is replacing npkill and mac-cleaner-cli with a single `npx devsweep`, without ever deleting something still in use.

## Positioning

It is built for developers, not for generic Mac cleaning: it understands git worktrees created by coding agents (Codex, Orca, claude-squad, Claude Code) and blocks the ones with a live process, and it removes simulators, emulators, Docker data and worktrees through their native commands instead of deleting files behind the tools' backs.

## Operating Context

Run from a terminal inside a projects folder (e.g. `~/Workspace`). The UI shows two sections: **This folder** (depends on the cwd) and **Machine** (independent of cwd). Used when disk is low, a few times a month.

## Capabilities and Constraints

- Nothing is deleted without a review screen showing the total and the exact commands.
- Only safe items (regenerable, no data loss, cheap to recreate) are preselected.
- Items with a live process inside them, booted simulators and running emulators are shown locked and cannot be selected.
- Docker volumes and Xcode Archives are never preselected.
- macOS only in v1; no mouse, no config file, no non-interactive mode.

## Brand Commitments

Name: **devsweep**. Visual reference is the author's own Lisa TUI (`@tarcisiopgs/lisa`): yellow accent/focus, gray/dim secondary text, white bold titles, green success, red errors, single borders. UI copy in English.

## Evidence on Hand

Measured on the author's machine on 2026-09-30 (see the design spec, §1). No users, testimonials or benchmarks exist yet; do not invent them.

## Product Principles

1. Never delete what is in use; show it locked instead of hiding it.
2. Show the reason and the size for every item, so the choice is informed.
3. Use each tool's native removal command when one exists.
4. Fast to the first useful screen: rows stream in while sizes are still computing.
5. Safe by default, fast for the expert: preselect only what cannot hurt.
