# Guest image for GitHub Actions jobs that run in a microVM. It holds the pinned Actions runner
# and the tools most workflows call. The runner starts from a just-in-time configuration that
# arrives with the job, takes one job and exits, so the image holds no credentials and no
# registration state.
#
# Two targets share one base:
#
#   actions-runner     runner, git, node, pnpm, jq, gh   (the default)
#   actions-runner-qa  the above plus browsers, a virtual display and tauri-driver
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

FROM ${BASE} AS actions-runner

LABEL org.opencontainers.image.source=https://github.com/getsigit/sigit \
      org.opencontainers.image.description="Guest image for GitHub Actions jobs in a microVM"

ARG RUNNER_VERSION=2.338.2
# sha256 of actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz, from the release notes.
ARG RUNNER_SHA256=baebf204307bac6193480852732f0aba2ae97b6c5597c244f4d5558390147c40
ARG NODE_VERSION=24.12.0
ARG PNPM_VERSION=10

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

# Node from nodejs.org, then pnpm.
RUN curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
        | tar -xJ -C /usr/local --strip-components=1 \
    && npm install -g "pnpm@${PNPM_VERSION}"

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

# Tauri runtime on Linux (AppImage under tauri-driver) and a virtual display.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        xvfb libwebkit2gtk-4.1-0 webkit2gtk-driver bubblewrap \
    && rm -rf /var/lib/apt/lists/*

# Browsers and their system libraries and fonts, at the pinned Playwright version.
RUN npx -y "playwright@${PLAYWRIGHT_VERSION}" install --with-deps chromium webkit \
    && chmod -R a+rX /ms-playwright \
    && rm -rf /var/lib/apt/lists/*

COPY --from=tauri-driver /out/bin/tauri-driver /usr/local/bin/tauri-driver
