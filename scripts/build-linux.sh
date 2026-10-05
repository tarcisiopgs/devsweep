#!/bin/sh
# Build the static Linux release binary for this machine's architecture, in
# the same container as scripts/test-linux.sh, and say how it is linked.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)

docker build --quiet --tag devsweep-linux --file "$root/scripts/linux.Dockerfile" "$root/scripts" >/dev/null

exec docker run --rm --init \
    --volume "$root":/src:ro \
    --volume devsweep-linux-target:/target \
    --volume devsweep-linux-cargo:/home/dev/.cargo \
    devsweep-linux sh -c '
        target="$(uname -m)-unknown-linux-musl"
        cargo build --release --locked --target "$target"
        bin="/target/$target/release/devsweep"
        "$bin" --version
        ldd "$bin" 2>&1 || true
    '
