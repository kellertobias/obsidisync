FROM rust:1-bookworm AS builder

WORKDIR /app

COPY rust-server/Cargo.toml rust-server/Cargo.lock ./rust-server/
COPY rust-server/src ./rust-server/src

RUN cargo build --manifest-path rust-server/Cargo.toml --release

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git openssh-client \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --uid 10001 --shell /usr/sbin/nologin obsidian-sync \
    && mkdir -p /data \
    && chown -R obsidian-sync:obsidian-sync /data

COPY --from=builder /app/rust-server/target/release/obsidian-git-sync-server /usr/local/bin/obsidian-git-sync-server

USER obsidian-sync
WORKDIR /data

ENV OBSIDIAN_GIT_SYNC_LISTEN=0.0.0.0:8787
ENV OBSIDIAN_GIT_SYNC_DATA_DIR=/data

VOLUME ["/data"]
EXPOSE 8787

ENTRYPOINT ["/usr/local/bin/obsidian-git-sync-server"]
