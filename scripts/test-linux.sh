#!/bin/sh
# Run the test suite on Linux, in a container, as a regular user. The
# repository is mounted read-only; arguments go to `cargo test`.
#
#   scripts/test-linux.sh            # the whole suite
#   scripts/test-linux.sh inuse::    # one module
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)

docker build --quiet --tag devsweep-linux --file "$root/scripts/linux.Dockerfile" "$root/scripts" >/dev/null

exec docker run --rm --init \
    --volume "$root":/src:ro \
    --volume devsweep-linux-target:/target \
    --volume devsweep-linux-cargo:/home/dev/.cargo \
    devsweep-linux cargo test --locked "$@"
