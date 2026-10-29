#!/usr/bin/env bash
# Builds the release `sigit` for a Linux target inside the manylinux2014 image, so the binary
# needs nothing newer than glibc 2.17.
#
#   packaging/linux/build-manylinux.sh x86_64-unknown-linux-gnu
#
# A plain `cargo build` links against the build host's glibc. v1.7.6 was built on a newer
# ubuntu-latest and needed GLIBC_2.43, so it would not start on Ubuntu 24.04 (2.39), the base of
# the Code Assurance runner image (#272). 2.17 is the same floor as the PyPI wheels, which
# release-pypi already builds in quay.io/pypa/manylinux2014_* through maturin, so this image is
# known to handle aws-lc-sys, onig_sys and esaxx-rs.
#
# This is a script and not a job-level `container:` because actions/checkout and the other
# JavaScript actions run Node inside the job container, and that Node needs glibc 2.28, which
# CentOS 7 does not have. cargo-zigbuild (`--target x86_64-unknown-linux-gnu.2.17`) would avoid
# Docker, but it swaps the C toolchain for every native dependency, while this image is the one
# PyPI has been building with since v1.7.4.
#
# The output lands at target/<target>/release/sigit, the same place `cargo build --target` puts
# it, so the packaging steps after it don't change. The host's ~/.cargo/{registry,git} are
# mounted so Swatinem/rust-cache still restores and saves them. Check the result with
# packaging/linux/check-glibc.sh.
set -euo pipefail

target=${1:?usage: build-manylinux.sh <rust-target>}
arch=$(uname -m)
case "${target}" in
  "${arch}"-unknown-linux-gnu) ;;
  *) echo "build-manylinux.sh: ${target} does not match this ${arch} host" >&2; exit 1 ;;
esac

repo=$(cd "$(dirname "$0")/../.." && pwd)
cargo_home=${CARGO_HOME:-$HOME/.cargo}
toolchain=$(sed -n 's/^channel = "\(.*\)"/\1/p' "${repo}/rust-toolchain.toml" | head -n 1)
[ -n "${toolchain}" ] || { echo "build-manylinux.sh: no channel in rust-toolchain.toml" >&2; exit 1; }
image=quay.io/pypa/manylinux2014_${arch}

mkdir -p "${cargo_home}/registry" "${cargo_home}/git"

# The container runs as root. Files it creates under target/ and the cargo cache are handed
# back to the runner user, which the later steps and rust-cache's post step need to write to.
docker run --rm \
  -v "${repo}:/io" \
  -v "${cargo_home}/registry:/cargo/registry" \
  -v "${cargo_home}/git:/cargo/git" \
  -w /io \
  -e CARGO_HOME=/cargo \
  -e RUSTUP_HOME=/rustup \
  -e CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-auto}" \
  -e TOOLCHAIN="${toolchain}" \
  -e TARGET="${target}" \
  -e OWNER="$(id -u):$(id -g)" \
  "${image}" \
  bash -euo pipefail -c '
    trap "chown -R ${OWNER} /io/target /cargo/registry /cargo/git 2>/dev/null || true" EXIT
    # Same before-script-linux as release-pypi: aws-lc-sys needs a full Perl.
    yum install -y -q perl-core
    curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain "${TOOLCHAIN}"
    export PATH=/cargo/bin:$PATH
    ldd --version | head -n 1
    cargo build --locked --release -p sigit --target "${TARGET}"
  '
