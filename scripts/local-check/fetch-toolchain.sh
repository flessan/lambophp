#!/usr/bin/env bash
# Fetches a Rust toolchain where the Rust hosts are unreachable.
#
# Some crates on the npm registry are the official Rust distributions: the
# vendor splits `rustc`, `rust-std`, `cargo` and `rustfmt` into separate
# packages, published from rust-lang's own release pipeline. That makes the
# npm registry usable as a transport in a sandbox where `static.rust-lang.org`
# and every crates.io mirror are blocked.
#
# Usage: fetch-toolchain.sh [version] [target] [destination]
#   version      e.g. 1.88.0   (default: read from rust-toolchain.toml)
#   target       e.g. x86_64-unknown-linux-gnu (default: this machine)
#   destination  where the sysroot is assembled (default: $TMPDIR/lambo-toolchain)
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
version="${1:-}"
target="${2:-}"
destination="${3:-${TMPDIR:-/tmp}/lambo-toolchain}"

if [ -z "$version" ]; then
  # CI pins an exact version; the package names are per version, so the pin is
  # what has to be fetched. `rust-toolchain.toml`'s channel is the fallback for
  # a checkout that has not pinned one.
  version="$(sed -n 's/.*dtolnay\/rust-toolchain@\([0-9][0-9.]*\).*/\1/p' \
    "$root/.github/workflows/ci.yml" | head -1)"
fi
if [ -z "$version" ]; then
  version="$(sed -n 's/^channel = "\([0-9][0-9.]*\)"/\1/p' "$root/rust-toolchain.toml" | head -1)"
fi
if [ -z "$version" ]; then
  echo "cannot tell which toolchain to fetch; pass one as the first argument" >&2
  exit 1
fi
if [ -z "$target" ]; then
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
    Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
    Darwin-x86_64) target=x86_64-apple-darwin ;;
    Darwin-arm64) target=aarch64-apple-darwin ;;
    MINGW* | MSYS* | CYGWIN*) target=x86_64-pc-windows-msvc ;;
    *) echo "cannot guess a target; pass one as the second argument" >&2; exit 1 ;;
  esac
fi

echo "toolchain $version for $target (pass a version as \$1 to override)"

work="${TMPDIR:-/tmp}/lambo-toolchain-npm"
mkdir -p "$work"
cd "$work"

for component in rustc rust-std rustfmt cargo; do
  package="@rustbin/${component}-${version}-${target}"
  echo "fetching $package"
  npm install --no-audit --no-fund --silent "$package"
done

sysroot="$destination/sysroot"
rm -rf "$sysroot"
mkdir -p "$sysroot"

# `rustc` ships the driver and its libraries; the std distribution adds the
# precompiled core/alloc/std rlibs that a `--sysroot` build needs.
rustc_dir="$work/node_modules/@rustbin/rustc-${version}-${target}/rustc"
std_dir="$work/node_modules/@rustbin/rust-std-${version}-${target}/rust-std-${target}"
test -d "$rustc_dir" || { echo "rustc package has an unexpected layout" >&2; exit 1; }

cp -r "$rustc_dir/bin" "$sysroot/"
cp -r "$rustc_dir/lib" "$sysroot/"
if [ -d "$std_dir/lib" ]; then
  cp -r "$std_dir/lib/." "$sysroot/lib/"
fi

rustfmt_dir="$work/node_modules/@rustbin/rustfmt-${version}-${target}/rustfmt-preview"
mkdir -p "$destination/bin"
if [ -x "$rustfmt_dir/bin/rustfmt" ] || [ -f "$rustfmt_dir/bin/rustfmt" ]; then
  cp "$rustfmt_dir/bin/rustfmt" "$destination/bin/rustfmt"
  chmod +x "$destination/bin/rustfmt"
fi
if [ -d "$work/node_modules/@rustbin/cargo-${version}-${target}" ]; then
  found="$(find "$work/node_modules/@rustbin/cargo-${version}-${target}" -type f -name cargo -perm -u+x | head -1 || true)"
  if [ -n "$found" ]; then
    cp "$found" "$destination/bin/cargo"
    chmod +x "$destination/bin/cargo"
  fi
fi

cat <<EOF

sysroot:   $sysroot
rustc:     $sysroot/bin/rustc   (needs LD_LIBRARY_PATH=$sysroot/lib)
rustfmt:   $destination/bin/rustfmt  (needs the same LD_LIBRARY_PATH)
cargo:     $destination/bin/cargo    (still needs the registry: it cannot build
                                      this workspace offline)

Next:
  export LD_LIBRARY_PATH=$sysroot/lib
  $sysroot/bin/rustc --edition 2024 --sysroot $sysroot --crate-type lib --emit=metadata -o /dev/null <file>
  $(dirname "${BASH_SOURCE[0]}")/check-format.sh
EOF
