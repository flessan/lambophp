#!/usr/bin/env bash
# Packages a release build into the artifacts a release publishes.
#
#   LAMBO_TARGET   rust target triple      (required)
#   LAMBO_NAME     artifact suffix         (required, e.g. windows-amd64)
#   LAMBO_ARCHIVE  zip | tar.gz            (required)
#   LAMBO_VERSION  version tag             (optional; read from Cargo.toml)
#
# Output in ./dist:
#   lambo-<name>[.exe]        the bare binary, for people who want one file
#   LamboPHP-<name>.<ext>     the archive, for everyone else
#
# The same script runs locally and in CI, so a hand-made release and a
# pipeline release produce the same layout.
set -euo pipefail

: "${LAMBO_TARGET:?LAMBO_TARGET is required}"
: "${LAMBO_NAME:?LAMBO_NAME is required}"
: "${LAMBO_ARCHIVE:?LAMBO_ARCHIVE is required (zip or tar.gz)}"

cd "$(dirname "$0")/.."
root="$(pwd)"

version="${LAMBO_VERSION:-}"
if [ -z "$version" ]; then
    version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
fi
version="${version#v}"

case "$LAMBO_TARGET" in
    *-windows-*) exe=".exe" ;;
    *) exe="" ;;
esac

binary="target/${LAMBO_TARGET}/release/lambo${exe}"
[ -f "$binary" ] || { echo "missing $binary - run 'cargo build --release --target ${LAMBO_TARGET}' first" >&2; exit 1; }

# The GUI is a Windows application, so it is expected on Windows targets and
# absent elsewhere. Optional rather than assumed: a Linux or macOS package is
# CLI-only, and failing the build there over a GUI that does not exist would be
# wrong.
gui_binary="target/${LAMBO_TARGET}/release/lambo-gui${exe}"
have_gui=no
case "$LAMBO_TARGET" in
    *-windows-*)
        if [ -f "$gui_binary" ]; then
            have_gui=yes
        else
            echo "warning: $gui_binary is missing; the package will be CLI-only" >&2
        fi
        ;;
esac

staging="dist/staging/LamboPHP-${version}-${LAMBO_NAME}"
rm -rf dist/staging
mkdir -p "$staging" dist

# The archive carries the licence and the shortest possible getting-started
# note: someone who downloads a zip should not have to find the repository to
# learn the first command.
cp "$binary" "$staging/lambo${exe}"
if [ "$have_gui" = yes ]; then
    cp "$gui_binary" "$staging/lambo-gui${exe}"
fi
# The licences are not optional. Shipping a binary without the terms it is
# distributed under is a packaging failure, so a missing licence file stops the
# build rather than quietly producing an incomplete archive.
for licence in LICENSE-APACHE LICENSE-MIT; do
    [ -f "$licence" ] || { echo "missing $licence; the package cannot ship without its licences" >&2; exit 1; }
    cp "$licence" "$staging/"
done
cat > "$staging/README.txt" <<EOF
Lambo PHP ${version} (${LAMBO_NAME})

Install
-------
Put lambo${exe} somewhere on your PATH, or run it from this directory.
Nothing else is required: Lambo downloads PHP, Apache and MariaDB itself,
into its own directory (%USERPROFILE%\\Lambo on Windows, ~/.lambo elsewhere).

Getting started
---------------
  cd your-project
  lambo init      detect the project and write lambo.yml
  lambo up        start the services
  open http://localhost
  lambo down      stop everything Lambo started

If port 80 is taken, Lambo serves on the next free port and tells you
which one - the URL it prints is the one to open.

Everything Lambo writes lives under LAMBO_HOME; nothing is installed
system-wide and no administrator rights are needed.

Documentation: https://github.com/flessan/lambophp/tree/main/docs
EOF

echo "dist/lambo-${LAMBO_NAME}${exe}"
cp "$staging/lambo${exe}" "dist/lambo-${LAMBO_NAME}${exe}"

archive="dist/LamboPHP-${LAMBO_NAME}.${LAMBO_ARCHIVE}"
echo "$archive"
case "$LAMBO_ARCHIVE" in
    zip)
        # `-j` would flatten the directory; the archive keeps the folder so
        # extracting it cannot scatter files into the current directory.
        (cd dist/staging && zip -qr "../$(basename "$archive")" "LamboPHP-${version}-${LAMBO_NAME}")
        ;;
    tar.gz)
        (cd dist/staging && tar czf "../$(basename "$archive")" "LamboPHP-${version}-${LAMBO_NAME}")
        ;;
    *)
        echo "LAMBO_ARCHIVE must be zip or tar.gz" >&2
        exit 1
        ;;
esac

rm -rf dist/staging
ls -l dist
