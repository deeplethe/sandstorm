# Multi-stage build for the sandstormd daemon.
#
# The controller is the long-running daemon; this image runs it as PID 1.
# It does NOT contain gVisor (`runsc`) — that must be installed on the
# host. The container is expected to run with
# `--privileged --network=host --pid=host` (or equivalent CAP_NET_ADMIN +
# CAP_SYS_ADMIN) so the daemon can manage netns and cgroups.
#
# Build:   docker build -t sandstormd .
# Run:     docker run -d --name sandstorm --privileged --network=host \
#              -v /var/lib/sandstorm:/var/lib/sandstorm \
#              -v /var/log/sandstorm:/var/log/sandstorm \
#              -v $HOME/.local/share/sandstorm/snapshots:/var/lib/sandstorm/snapshots:ro \
#              sandstormd

# ---- console ----
# The web console (web/console, TypeScript + React), built to one HTML file
# that build.rs embeds in sandstormd.
FROM node:22-bookworm-slim AS console
WORKDIR /build/web/console
COPY web/console/package.json web/console/package-lock.json ./
RUN npm ci
COPY web/console/ ./
RUN npm run build

# ---- builder ----
FROM rust:1.83-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
# Compiled in: what an image template runs as its init and agent.
COPY rootfs-init ./rootfs-init
COPY --from=console /build/web/console/dist ./web/console/dist
COPY packaging/install.sh ./packaging/install.sh
RUN cargo build --release --locked --bin sandstormd

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates iproute2 \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/sandstormd /usr/local/bin/sandstormd

# Non-root by default (the operator can override via --user 0 when the
# daemon needs CAP_NET_ADMIN for netns / cgroup work).
RUN groupadd --system sandstorm \
 && useradd  --system --gid sandstorm --home-dir /var/lib/sandstorm --shell /usr/sbin/nologin sandstorm \
 && mkdir -p /var/lib/sandstorm /var/log/sandstorm \
 && chown -R sandstorm:sandstorm /var/lib/sandstorm /var/log/sandstorm

USER sandstorm
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/sandstormd", "serve"]
# Add --tls-san with the name clients use: nothing outside the container can
# verify this master by its loopback names.
CMD ["--bind", "0.0.0.0:8080"]
