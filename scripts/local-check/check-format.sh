#!/usr/bin/env bash
# Runs the pinned `rustfmt` over every `.rs` file in the workspace.
#
# `cargo fmt --all --check` needs a working `cargo` and the crate graph; this
# does not, which is the point. The formatter is the same build CI uses, from
# the version in `rust-toolchain.toml`, so a clean run here means a clean run
# there.
#
# Usage: check-format.sh [--write]
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
destination="${TMPDIR:-/tmp}/lambo-toolchain"
rustfmt="$destination/bin/rustfmt"

if [ ! -x "$rustfmt" ]; then
  echo "no rustfmt at $rustfmt - run fetch-toolchain.sh first" >&2
  exit 1
fi

# The std library distribution carries the driver's shared libraries.
export LD_LIBRARY_PATH="$destination/sysroot/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

mapfile -t files < <(cd "$root" && find crates -name '*.rs' -type f | sort)
echo "checking ${#files[@]} files with $("$rustfmt" --version)"

cd "$root"
if [ "${1:-}" = "--write" ]; then
  "$rustfmt" --edition 2024 -q "${files[@]}"
  echo "formatted"
else
  "$rustfmt" --edition 2024 --check "${files[@]}"
  echo "clean: cargo fmt --all --check has nothing to report"
fi
