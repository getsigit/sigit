# ghcr.io/getsigit/sigit-code-assurance-runner: the guest image of the `sigit-code-assurance`
# Fireactions pool, where siGit Code Assurance tests a release candidate in a fresh microVM.
# The package is public and holds no secrets: everything a run needs arrives with the job.
#
# It carries no app dependencies, so the same image serves any repository the gate runs on;
# jobs run their own `pnpm install`. What it does carry is this release's `sigit`, so the
# assurance agent doesn't download it on every run. release-assurance-image.yml rebuilds it
# for each sigit release.
#
# Fireactions needs an image that boots systemd and runs `fireactions agent`, so this starts from
# the official fireactions-images base (runner, docker, gh, jq, agent) and adds what the gate
# needs on top. It does not start from mcr.microsoft.com/playwright: that image has no init.
#
#   docker build --build-arg SIGIT_VERSION=v1.7.6 -t sigit-code-assurance-runner packaging/assurance-runner

ARG BASE=ghcr.io/hostinger/fireactions-images/ubuntu24.04:latest

# tauri-driver is cargo-only; build it here and copy the binary so the guest has no Rust.
FROM rust:1-bookworm AS tauri-driver
RUN cargo install tauri-driver --locked --root /out

FROM ${BASE}

LABEL org.opencontainers.image.source=https://github.com/getsigit/sigit \
      org.opencontainers.image.description="Guest image for siGit Code Assurance runs"

ARG NODE_VERSION=24.12.0
ARG PNPM_VERSION=10
# Must match the consumer's @playwright/test. A job's `playwright install` is a no-op when it
# does and a browser download when it doesn't, and the visual baselines are only stable against
# the browsers they were recorded with.
ARG PLAYWRIGHT_VERSION=1.64.1
# A sigit release tag (v1.7.6), or `latest` for a pull-request build.
ARG SIGIT_VERSION=latest

# Build-time only inside the microVM: Fireactions boots this as a rootfs, so jobs must set
# PLAYWRIGHT_BROWSERS_PATH themselves (see sigit-app fireactions-smoke.yml).
ENV DEBIAN_FRONTEND=noninteractive \
    PLAYWRIGHT_BROWSERS_PATH=/ms-playwright

# Tauri runtime on Linux (AppImage under tauri-driver), a virtual display, and bubblewrap for
# the assurance agent's sandbox.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        xvfb libwebkit2gtk-4.1-0 webkit2gtk-driver bubblewrap \
    && rm -rf /var/lib/apt/lists/*

# Node from nodejs.org, then pnpm.
RUN curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
        | tar -xJ -C /usr/local --strip-components=1 \
    && npm install -g "pnpm@${PNPM_VERSION}"

# Browsers and their system libraries and fonts, at the pinned Playwright version.
RUN npx -y "playwright@${PLAYWRIGHT_VERSION}" install --with-deps chromium webkit \
    && chmod -R a+rX /ms-playwright \
    && rm -rf /var/lib/apt/lists/*

COPY --from=tauri-driver /out/bin/tauri-driver /usr/local/bin/tauri-driver

# The released sigit. The .sha256 sidecar is a bare digest, so it is compared rather than fed
# to `sha256sum -c`. It comes from the same origin as the binary: it catches a truncated or
# corrupted download, not a tampered release.
RUN set -eu; \
    if [ "${SIGIT_VERSION}" = latest ]; then \
        url=https://github.com/getsigit/sigit/releases/latest/download/sigit-linux-amd64; \
    else \
        url="https://github.com/getsigit/sigit/releases/download/${SIGIT_VERSION}/sigit-linux-amd64"; \
    fi; \
    expected="$(curl -fsSL --retry 2 "${url}.sha256" | tr -d '[:space:]')"; \
    curl -fsSL --retry 2 "${url}" -o /usr/local/bin/sigit; \
    actual="$(sha256sum /usr/local/bin/sigit | cut -d' ' -f1)"; \
    [ "${actual}" = "${expected}" ] || { echo "sigit checksum mismatch: got ${actual}, expected ${expected}" >&2; exit 1; }; \
    chmod 755 /usr/local/bin/sigit; \
    /usr/local/bin/sigit --version
