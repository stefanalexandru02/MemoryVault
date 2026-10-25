# syntax=docker/dockerfile:1
#
# MemoryVault server image. One binary (`memvault`) serves every role:
#   serve (API + /mcp + /health + /metrics), worker, planner, migrate, admin, evals.
#
# Build from the repository root:
#   docker build -f deploy/docker/server.Dockerfile -t memvault:0.1.0 .
#
# The runtime is Debian slim rather than distroless/cc so that `kubectl exec` has a shell for
# incident work. Switching to gcr.io/distroless/cc-debian12:nonroot is a one-line change if the
# image should carry nothing but the binary; the binary links against glibc only (rustls, no OpenSSL).

FROM rust:1.97-bookworm AS builder
WORKDIR /src
COPY . .
# The cache mounts keep the cargo registry and the build directory between builds. The binary is
# copied out of the cache mount in the same step, because the mount is not part of the image.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p memvault \
 && cp /src/target/release/memvault /usr/local/bin/memvault

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 memvault \
 && useradd --system --uid 10001 --gid memvault --home-dir /app --shell /usr/sbin/nologin memvault

WORKDIR /app
COPY --from=builder /usr/local/bin/memvault /usr/local/bin/memvault
# Migrations are embedded in the binary at compile time (sqlx::migrate!). The copy here is for
# operators who inspect the image; the runtime does not read it.
COPY migrations/ /app/migrations/
# Read at start-up by the prompt registry and the schema validator. Kubernetes mounts ConfigMaps
# over these paths, so a prompt change is a ConfigMap update plus a rollout.
COPY prompts/ /app/prompts/
COPY schemas/ /app/schemas/

ENV PROMPTS_DIR=/app/prompts \
    SCHEMAS_DIR=/app/schemas \
    HOST=0.0.0.0 \
    PORT=8000 \
    LOG_FORMAT=json

USER 10001:10001
EXPOSE 8000

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/memvault"]
CMD ["serve"]
