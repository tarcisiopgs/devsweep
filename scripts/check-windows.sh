#!/bin/sh
# Type-check and lint the crate for Windows, in the same container as
# scripts/test-linux.sh. No Windows test runs here: a Mac cannot run a
# Windows container, so the suite itself only runs in CI (windows-latest).
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)

docker build --quiet --tag devsweep-linux --file "$root/scripts/linux.Dockerfile" "$root/scripts" >/dev/null

exec docker run --rm --init \
    --volume "$root":/src:ro \
    --volume devsweep-linux-target:/target \
    --volume devsweep-linux-cargo:/home/dev/.cargo \
    devsweep-linux cargo clippy --locked --all-targets --target x86_64-pc-windows-gnu "$@" -- -D warnings
