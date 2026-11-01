# syntax=docker/dockerfile:1.7
#
# An E2B template for Sandstorm: a rootfs with E2B's own envd in it.
#
# envd is built from e2b-dev/infra at a pinned tag rather than forked: the
# SDKs switch features on the version envd reports, so it has to be the real
# thing, and moving to a newer one is a change to ENVD_REF and a rebuild.
# envd is Apache-2.0; see NOTICE.
#
# BASE is any image to put envd into. ubuntu:22.04 gives E2B's `base`;
# e2bdev/code-interpreter gives the code interpreter.

ARG ENVD_REF=2026.30
ARG GO_VERSION=1.26
ARG BASE=ubuntu:22.04

FROM golang:${GO_VERSION}-bookworm AS envd
ARG ENVD_REF
ARG TARGETARCH
RUN git clone --depth 1 --branch "${ENVD_REF}" https://github.com/e2b-dev/infra.git /src
WORKDIR /src/packages/envd
RUN CGO_ENABLED=0 GOOS=linux GOARCH=${TARGETARCH} \
      go build -ldflags "-X=main.commitSHA=$(git -C /src rev-parse --short HEAD) -s -w" -o /out/envd . \
 && /out/envd -version

FROM ${BASE}
ARG DEBIAN_FRONTEND=noninteractive
# tini as PID 1: envd starts processes and a command's orphans are
# reparented to PID 1, which has to reap them.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl sudo tini python3 \
 && rm -rf /var/lib/apt/lists/*
# E2B runs commands as `user`, uid 1000, with passwordless sudo, and the
# SDKs default to it. See the README on what sudo needs from the bundle.
RUN id -u user >/dev/null 2>&1 || useradd --create-home --uid 1000 --shell /bin/bash user; \
    echo 'user ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/user && chmod 0440 /etc/sudoers.d/user
COPY --from=envd /out/envd /usr/bin/envd
COPY entrypoint.sh /usr/local/bin/sandstorm-e2b-entrypoint
RUN chmod 0755 /usr/bin/envd /usr/local/bin/sandstorm-e2b-entrypoint
