# One runtime-only image. Built by the CI workflow's `image` job on every
# push and pull request from what the runner built (scripts/release-build.sh,
# handed over as a workflow artifact) and, on a push to main, pushed to
# Artifact Registry as its short hash and deployed to the staging Cloud
# Run service; a pushed Release tag names that image by Version and
# deploys it to production, building nothing. A dev box runs the same
# script, then `docker build`.
#
# The base and the runner. The binaries link the glibc of the runner that
# built them and name, per function, the newest version tag that glibc
# offers; at the container's start ld.so checks every tag against this
# base's libc.so.6, and newer glibc runs older binaries, never the
# reverse. Strive for the same release on both sides (this base and the
# shipping jobs' `runs-on: ubuntu-24.04`); the hard rule, for a distroless
# base or one newer than the runners offer, is the inequality: the
# runner's glibc at or below the base's, the base moved first, the runner
# label only once the base's glibc is at or above the runner's. The check,
# on the machine that built:
#   objdump -T target/release/kiewer | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1
# prints at most the base's glibc version.
#
# Layer order: the apt layer first, the binaries last, so a pull after a
# code change fetches the binaries layer alone.

FROM ubuntu:24.04
ARG BUILD_VERSION=dev
# The programs the code runs, and ca-certificates for TLS. This list, the
# CI workflow's apt step and the dev-box setup move together: a program
# one of them gains, all gain in the same commit.
# tini: PID 1 in front of the program, so a child orphaned by a killed
# process is reaped instead of sitting as a zombie for the container's
# life, and the SIGTERM Cloud Run sends at shutdown is forwarded unchanged.
# mesa-vulkan-drivers: Mesa's lavapipe, the software Vulkan driver the
# headless renderer draws thumbnails with (ADR 0004); the Vulkan loader
# comes with it. Cloud Run has no GPU, so this is the only driver the
# loader finds, and no environment variable is needed to select it.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini mesa-vulkan-drivers \
    && rm -rf /var/lib/apt/lists/*
# The user the program runs as: Cloud Run runs the entrypoint as root
# unless the image says otherwise. Ubuntu's image ships a placeholder
# `ubuntu` user at 1000; removing it is hygiene, so a base image without
# it must not break the build.
ARG KIEWER_UID=1500
ARG KIEWER_GID=$KIEWER_UID
RUN if id ubuntu >/dev/null 2>&1; then userdel -r ubuntu; fi \
    && groupadd --gid "$KIEWER_GID" kiewer \
    && useradd --uid "$KIEWER_UID" --gid kiewer --no-create-home --shell /usr/sbin/nologin kiewer
# What the runner built, the executable package alone, not the workspace;
# the wildcard takes every binary that package builds, so a second [[bin]]
# needs no line. --chmod: a workflow artifact comes down without its
# executable bit; what this copies is executable because this line says so.
COPY --chmod=755 target/release/kiewer* /usr/local/bin/
USER kiewer
WORKDIR /app
# Secrets arrive as environment variables from Secret Manager through the
# Cloud Run service definition, never baked into the image. Cloud Run sets
# PORT; the server listens on it.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/kiewer"]
