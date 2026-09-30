# Contributing

Thanks for helping devsweep find more of what developers can safely delete.

## Adding a cache rule

Developer tool caches live in `catalog/*.toml`, one file per ecosystem. A rule is data, not code:

```toml
[[rule]]
id = "npm-npx"                 # unique, kebab-case
group = "JavaScript"           # shown next to the item
label = "npx cache"            # what the user sees
paths = ["~/.npm/_npx"]        # `~` = home, `{darwin_cache}` = getconf DARWIN_USER_CACHE_DIR, one `*` allowed
safe = true                    # preselected? see below
mode = "clear"                 # remove | clear | children
busy_when = ["npm", "npx"]     # a running process with this exact name locks the item
# command = ["npm", "cache", "verify"]   # native command, preferred when the tool offers one
# requires_bin = "npm"                   # the command is used only when this binary is on PATH
# path_cmd = ["pnpm", "store", "path"]   # for tools that decide the path at runtime
source = "docs: https://docs.npmjs.com/cli/commands/npx"
```

**When is a rule `safe`?** Only when the data regenerates by itself, nothing is lost, and recreating it costs nothing relevant. Download caches (packages, modules, browsers, SDKs) are never `safe`: they regenerate, but the next build downloads everything again. Build caches, logs and temporary files can be.

**Every rule needs a `source`.** Either `docs: <url>` pointing to the tool's own documentation, or `observed on disk` when you confirmed the folder on a real machine. `cargo test` fails when a rule has no source.

## Clean-room policy

devsweep is MIT licensed. Some cleaners in this space are GPL licensed, Mole among them. Do not read, copy or translate their source code when contributing here, not even to "check how they did it". Facts are fine: where a tool keeps its cache, what its documentation says, what you see on disk, and bug reports that describe a pitfall. Write the rule and the code yourself.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

TUI changes come with `insta` snapshots: run `cargo insta review` and check every snapshot against the design before accepting it.

## Releasing

1. Bump `version` in `Cargo.toml` (and `Cargo.lock` with `cargo check`) in a pull request, and merge it.
2. Publish a GitHub Release whose tag is that version with a `v` prefix:

   ```sh
   gh release create v0.2.0 --generate-notes
   ```

The `Release` workflow checks that the tag matches `Cargo.toml`, attaches the macOS archives to the release and publishes `@tarcisiopgs/devsweep`, `devsweep-darwin-arm64` and `devsweep-darwin-x64` to npm through trusted publishing. No npm token is involved.
