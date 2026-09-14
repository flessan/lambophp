#!/bin/sh
# Lambo PHP installer (Linux and macOS)
#
#   curl -fsSL https://raw.githubusercontent.com/flessan/lambophp/main/scripts/install.sh | sh
#
# On Windows, download LamboPHP-Setup.exe (or the portable zip) from the
# latest release instead: https://github.com/flessan/lambophp/releases
#
# Environment knobs:
#   LAMBO_VERSION     install a specific tag (e.g. "v0.2.0"); default: latest
#   LAMBO_INSTALL_DIR where the binary lands;            default: ~/.local/bin
#   LAMBO_HOME        where Lambo keeps its data;        default: ~/.lambo
#
# Design: POSIX sh, no sudo, idempotent, checksum-verified, loud failures.

set -eu

REPO="flessan/lambophp"
RELEASES="https://github.com/${REPO}/releases"

say()  { printf '%s\n' "$*"; }
info() { printf '\033[32m✔\033[0m %s\n' "$*"; }
warn() { printf '\033[33m⚠\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31m✖ error:\033[0m %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "required tool '$1' is not installed"
}

fetch() {
    # fetch <url> <output-file>
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$2" "$1"
    else
        die "neither curl nor wget is available"
    fi
}

# --- resolve the release name ----------------------------------------------

os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)

case "$arch" in
    x86_64 | amd64)  name_arch="amd64" ;;
    aarch64 | arm64) name_arch="arm64" ;;
    *) die "unsupported CPU architecture: $arch" ;;
esac

case "$os" in
    linux)  name="${name_arch:+linux-$name_arch}" ;;
    darwin) name="macos-$name_arch" ;;
    *)      die "unsupported OS: $os (on Windows, download LamboPHP-Setup.exe from ${RELEASES}/latest)" ;;
esac

# --- resolve version -------------------------------------------------------

if [ "${LAMBO_VERSION:-}" ]; then
    version="$LAMBO_VERSION"
else
    need grep
    need sed
    # The latest-release redirect URL ends with the tag name - no API rate
    # limits, works everywhere.
    if command -v curl >/dev/null 2>&1; then
        location=$(curl -fsSI -o /dev/null -w '%{url_effective}' "${RELEASES}/latest") \
            || die "could not resolve the latest release"
    else
        location=$(wget --server-response --max-redirect=0 "${RELEASES}/latest" 2>&1 \
            | sed -n 's/.*Location: //p' | tr -d '\r' | head -n1) || true
    fi
    version=$(printf '%s' "${location:-}" | sed -n 's#.*/tag/\(v[^/]*\).*#\1#p')
    [ -n "$version" ] || die "could not determine the latest version; set LAMBO_VERSION explicitly"
fi

short="${version#v}"
archive="LamboPHP-${name}.tar.gz"
url="${RELEASES}/download/${version}/${archive}"
sums_url="${RELEASES}/download/${version}/SHA256SUMS"

# --- download and verify ---------------------------------------------------

tmpdir=$(mktemp -d 2>/dev/null || mktemp -d -t lambo-install)
trap 'rm -rf "$tmpdir"' EXIT

say "Installing Lambo PHP ${version} (${name})"
fetch "$url" "${tmpdir}/${archive}" \
    || die "download failed: $url - does release ${version} provide ${name}?"

# Verify against the release checksums when a checker exists; refuse to
# silently skip when the manifest is downloadable.
if command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1; then
    if fetch "$sums_url" "${tmpdir}/SHA256SUMS" 2>/dev/null; then
        expected=$(grep " ${archive}\$" "${tmpdir}/SHA256SUMS" | cut -d' ' -f1) || true
        [ -n "${expected:-}" ] || die "checksum manifest has no entry for ${archive}"
        if command -v sha256sum >/dev/null 2>&1; then
            actual=$(sha256sum "${tmpdir}/${archive}" | cut -d' ' -f1)
        else
            actual=$(shasum -a 256 "${tmpdir}/${archive}" | cut -d' ' -f1)
        fi
        [ "$actual" = "$expected" ] || die "checksum mismatch for ${archive} (expected $expected, got $actual)"
        info "checksum verified"
    else
        warn "checksum manifest unavailable; skipping verification"
    fi
fi

# --- install ---------------------------------------------------------------

need tar

install_dir="${LAMBO_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$install_dir"

tar -xzf "${tmpdir}/${archive}" -C "$tmpdir"
extracted=$(find "$tmpdir" -type f -name lambo -perm +111 | head -n1)
[ -n "$extracted" ] || die "the archive did not contain a lambo executable"
mv "$extracted" "${install_dir}/lambo"
chmod +x "${install_dir}/lambo"
info "installed ${install_dir}/lambo"

# --- PATH guidance ---------------------------------------------------------

case ":${PATH}:" in
    *":${install_dir}:"*) ;;
    *)
        warn "$install_dir is not on your PATH"
        say "  add this to your shell profile (~/.bashrc, ~/.zshrc, ...):"
        say "    export PATH=\"${install_dir}:\$PATH\""
        ;;
esac

# --- done ------------------------------------------------------------------

info "Lambo PHP ${version} installed"
say ""
say "Next steps:"
say "  cd your-project"
say "  lambo init       # detect the project, write lambo.yml"
say "  lambo up         # start PHP, the web server and the database"
say "  lambo doctor     # explain anything that is missing"
