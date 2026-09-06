#!/bin/sh
# Test the isolated vendored nusb request-construction patch without USB hardware.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/sdrfox-nusb.XXXXXX")
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
cp -R "$root/vendor/nusb/." "$scratch/"
cargo test --manifest-path "$scratch/Cargo.toml" --locked --lib control_out_tests
