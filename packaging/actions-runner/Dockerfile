# Guest image for GitHub Actions jobs that run in a microVM. It holds the pinned Actions runner
# and the tools most workflows call. The runner starts from a just-in-time configuration that
# arrives with the job, takes one job and exits, so the image holds no credentials and no
# registration state.
#
# Two targets share one base:
#
#   actions-runner     runner, git, node, pnpm, jq, gh   (the default)
#   actions-runner-qa  the above plus browsers, a virtual display, ffmpeg, tauri-driver and xcrs
#
#   docker build --target actions-runner    -t actions-runner    packaging/actions-runner
#   docker build --target actions-runner-qa -t actions-runner-qa packaging/actions-runner
#
# The image has to boot systemd: the microVM's init and its guest agent are units. See README.md
# for turning the image into a root filesystem.

ARG BASE=ubuntu:24.04

# tauri-driver is cargo-only; build it here and copy the binary so the guest has no Rust.
FROM rust:1-bookworm AS tauri-driver
RUN cargo install tauri-driver --locked --root /out

# xcrs is the MCP server the QA agent drives the app through. It is built from a pinned commit of
# smbcloud-cli, in the same way, so the guest has no Rust. The commit is the one that adds the
# desktop target (WebDriver); move it when xcrs changes.
FROM rust:1-bookworm AS xcrs
ARG XCRS_REV=571acdba8702f8cb620de4a807326ae0e3e5dd24
RUN cargo install --git https://github.com/smbcloudXYZ/smbcloud-cli --rev "${XCRS_REV}" xcrs --locked --root /out

FROM ${BASE} AS actions-runner

LABEL org.opencontainers.image.source=https://github.com/getsigit/sigit \
      org.opencontainers.image.description="Guest image for GitHub Actions jobs in a microVM"

ARG RUNNER_VERSION=2.338.2
# sha256 of actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz, from the release notes.
ARG RUNNER_SHA256=baebf204307bac6193480852732f0aba2ae97b6c5597c244f4d5558390147c40
ARG NODE_VERSION=24.12.0
# sha256 of node-v${NODE_VERSION}-linux-x64.tar.xz, from SHASUMS256.txt in the same dist directory.
ARG NODE_SHA256=bdebee276e58d0ef5448f3d5ac12c67daa963dd5e0a9bb621a53d1cefbc852fd
ARG PNPM_VERSION=10.34.9

ENV DEBIAN_FRONTEND=noninteractive

# Init, the packages the guest agent and the host tooling need (python3, git, CA certificates,
# zlib), and the usual build tools.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        systemd systemd-sysv dbus iproute2 \
        ca-certificates zlib1g python3 git curl jq unzip xz-utils zstd \
        build-essential pkg-config libssl-dev openssh-client sudo \
    && rm -rf /var/lib/apt/lists/*

# The GitHub CLI, from its apt repository.
RUN install -d -m 0755 /etc/apt/keyrings \
    && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
        -o /etc/apt/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=amd64 signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
        > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*

# Node from nodejs.org, checked against its published digest, then pnpm at an exact version.
RUN set -eu; \
    curl -fsSL --retry 2 -o /tmp/node.tar.xz \
        "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz"; \
    echo "${NODE_SHA256}  /tmp/node.tar.xz" | sha256sum -c -; \
    tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1; \
    rm /tmp/node.tar.xz; \
    npm install -g "pnpm@${PNPM_VERSION}"

# The Actions runner, checked against the published digest, with its system dependencies. The
# runner refuses to start as root unless RUNNER_ALLOW_RUNASROOT is set; the host sets it, since
# the guest has a single user and the VM lives for one job.
RUN set -eu; \
    mkdir -p /opt/actions-runner; \
    curl -fsSL --retry 2 -o /tmp/runner.tar.gz \
        "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz"; \
    echo "${RUNNER_SHA256}  /tmp/runner.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/runner.tar.gz -C /opt/actions-runner; \
    rm /tmp/runner.tar.gz; \
    /opt/actions-runner/bin/installdependencies.sh; \
    rm -rf /var/lib/apt/lists/*; \
    test -x /opt/actions-runner/run.sh

WORKDIR /workspace

FROM actions-runner AS actions-runner-qa

LABEL org.opencontainers.image.description="Guest image for GitHub Actions jobs in a microVM, with browsers"

# Must match the consumer's @playwright/test. A job's `playwright install` is a no-op when it
# does and a browser download when it doesn't, and visual baselines are only stable against the
# browsers they were recorded with.
ARG PLAYWRIGHT_VERSION=1.64.1

ENV PLAYWRIGHT_BROWSERS_PATH=/ms-playwright

# Tauri runtime on Linux (AppImage under tauri-driver), a virtual display, and ffmpeg, which
# records that display during a siGit Code Assurance operate run (sigit-app ci/assurance/record.sh).
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        xvfb libwebkit2gtk-4.1-0 webkit2gtk-driver bubblewrap ffmpeg \
    && rm -rf /var/lib/apt/lists/*

# Browsers and their system libraries and fonts, at the pinned Playwright version.
RUN npx -y "playwright@${PLAYWRIGHT_VERSION}" install --with-deps chromium webkit \
    && chmod -R a+rX /ms-playwright \
    && rm -rf /var/lib/apt/lists/*

COPY --from=tauri-driver /out/bin/tauri-driver /usr/local/bin/tauri-driver
COPY --from=xcrs /out/bin/xcrs /usr/local/bin/xcrs
