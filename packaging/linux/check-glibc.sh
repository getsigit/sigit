#!/usr/bin/env bash
# Fails if a Linux binary needs a newer glibc than the release floor.
#
#   packaging/linux/check-glibc.sh target/x86_64-unknown-linux-gnu/release/sigit [2.17]
#
# The Linux releases are built in manylinux2014 (packaging/linux/build-manylinux.sh) so they run
# on glibc 2.17 and later. If a build slips back onto the runner's own toolchain, the binary still
# builds and passes tests there, then fails to start on an older distro with "GLIBC_2.xx not
# found". This check catches it before release, the way check-static-crt.ps1 does for Windows.
set -euo pipefail

binary=${1:?usage: check-glibc.sh <binary> [max-glibc]}
max=${2:-2.17}

top=$(objdump -T "${binary}" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -n 1)
if [ -z "${top}" ]; then
  echo "::error::objdump found no GLIBC_ symbols in ${binary}"
  exit 1
fi
if [ "$(printf '%s\n%s\n' "${top}" "${max}" | sort -V | tail -n 1)" != "${max}" ]; then
  echo "::error::${binary} needs GLIBC_${top}, above the ${max} floor"
  exit 1
fi
echo "${binary} needs at most GLIBC_${top} (floor ${max})."
