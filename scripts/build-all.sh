#!/usr/bin/env bash
# Builds both versions of HamRadNet:
#
#   dist/go/     the Go programs    (scripts/build-go.sh)
#   dist/rust/   the Rust programs  (scripts/build-rust.sh)
#
# The two speak the same protocol, so any client works with either server.
set -euo pipefail

cd "$(dirname "$0")"
./build-go.sh
echo
./build-rust.sh
