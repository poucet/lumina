# Lumina (Discord bot + embedded simply-daemon) — server image.
#
# Build context is the WORKSPACE ROOT (see docker-compose.yml: context = ..),
# so paths below are relative to the repo root.
#
# Only the `lumina` binary is built. It pulls in `simply-daemon` via the
# default `embedded` feature, which hosts the daemon in-process and listens on
# 127.0.0.1:9800 — never on a public interface (Caddy proxies the OAuth routes).
# `pocket-tts` adds local text-to-speech on the CPU. Its model downloads from
# Hugging Face into /root/.cache/huggingface on first start (bind-mounted by
# docker-compose.yml so rebuilds keep it); custom voices go in
# /data/models/voices/.

# ---------------------------------------------------------------------------
# Builder
# ---------------------------------------------------------------------------
FROM rust:1-bookworm AS builder

# Native deps: openssl (reqwest), sqlite/rusqlite (bundled needs a C toolchain,
# present in the rust image), plus cmake/clang for any crate using bindgen.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev cmake clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# SKIP_ADMIN_BUILD=1 guarantees the daemon's build.rs never runs the admin-UI
# npm build. (It is already skipped because simply-daemon is a transitive dep
# here, not the primary package — but we make it explicit so no Node is needed.)
ENV SKIP_ADMIN_BUILD=1

# `-p lumina` is required: the workspace's default-members is just simply-daemon,
# so a bare `--bin lumina` from the root won't find the lumina bin.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p lumina --bin lumina --features pocket-tts \
    && cp target/release/lumina /usr/local/bin/lumina \
    && ptts="$(sed -n '/^name = "pocket-tts"$/{n;s/^version = "\(.*\)"$/\1/p;}' Cargo.lock)" \
    && cp -r /usr/local/cargo/registry/src/*/pocket-tts-"$ptts"/config /pocket-tts-config

# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/lumina /usr/local/bin/lumina

# pocket-tts reads its model config (config/<variant>.yaml) from its own
# source dir, which only exists in the builder's cargo registry, or else from
# ./config relative to the working directory. The working directory is /, so
# the config goes in /config.
COPY --from=builder /pocket-tts-config /config

# All config + state (SQLite, vault, blob storage) lives under NOEMA_DATA_DIR,
# which we mount as a volume so it survives container rebuilds.
ENV NOEMA_DATA_DIR=/data
ENV RUST_LOG=info
VOLUME ["/data"]

ENTRYPOINT ["/usr/local/bin/lumina"]
