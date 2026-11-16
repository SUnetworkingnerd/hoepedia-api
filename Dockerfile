# syntax=docker/dockerfile:1

# ---- chef: cache dependency builds across code changes ----
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# SQLite and Lua 5.4 are compiled in (rusqlite "bundled", mlua "vendored"): nothing extra at runtime.
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked --bin hoepedia-api
# Data dir, created here because the runtime image has no shell.
RUN mkdir /data

# ---- runtime: slim Debian with the preview tools, non-root ----
# ffmpeg (video/audio) and Poppler (PDF) make the media previews; without them the server
# still works and simply serves those files without previews.
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg poppler-utils ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --gid 65532 nonroot \
 && useradd --uid 65532 --gid 65532 --no-create-home --shell /usr/sbin/nologin nonroot
COPY --from=builder /app/target/release/hoepedia-api /usr/local/bin/hoepedia-api
# 65532 = the "nonroot" user. A named volume mounted on /data inherits this ownership.
COPY --from=builder --chown=65532:65532 /data /data
USER 65532:65532

ENV PORT=3000 \
    BIND_ADDR=0.0.0.0 \
    RUST_LOG=info \
    DATABASE_PATH=/data/wiki.db \
    BACKUP_DIR=/data/backups
VOLUME ["/data"]
EXPOSE 3000

# The binary has a built-in --healthcheck flag (no curl needed).
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/hoepedia-api", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/hoepedia-api"]
