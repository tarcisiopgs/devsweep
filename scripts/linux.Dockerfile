# The Linux environment behind scripts/test-linux.sh and
# scripts/check-windows.sh: a regular user, so permission tests mean what
# they mean on a real machine, and the Windows target for type checks.
FROM rust:1-bookworm

RUN rustup component add clippy rustfmt \
    && rustup target add x86_64-pc-windows-gnu \
    && useradd --create-home --uid 1000 dev \
    && mkdir -p /target /home/dev/.cargo \
    && chown -R dev:dev /target /home/dev/.cargo

ENV CARGO_HOME=/home/dev/.cargo \
    CARGO_TARGET_DIR=/target

USER dev
WORKDIR /src
