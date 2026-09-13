#!/usr/bin/env bash
# Rebuilds the two fixture PHP archives from the sources in this directory.
#
# The archives are committed and their digests are pinned in catalogue.json and
# asserted by tests/runtime_distribution.rs, so this must be reproducible:
# fixed mtimes, fixed ownership, sorted entries, no extended headers. Run it
# after editing the fixture, then copy the two new digests into both places.
#
# Usage: ./build.sh
set -euo pipefail

cd "$(dirname "$0")"

VERSION=8.4.2
STAMP="2024-12-17T18:22:53Z"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# Reproducibility flags shared by both archives.
TAR_REPRO=(--owner=0 --group=0 --numeric-owner --mtime="$STAMP" --sort=name --format=ustar)

# ---------------------------------------------------------------------------
# linux-x64: a conventional Unix PHP layout.
# ---------------------------------------------------------------------------
linux="$STAGE/php-$VERSION"
mkdir -p "$linux/bin" "$linux/lib/php/extensions/no-debug-non-zts-20240924" \
         "$linux/ext" "$linux/etc"
cp src/php "$linux/bin/php"
cp src/php-config "$linux/bin/php-config"
chmod 755 "$linux/bin/php" "$linux/bin/php-config"
cp src/php.ini-shipped "$linux/lib/php.ini"
cp src/NEWS "$linux/NEWS"

# Module files. Only the ones present here can be enabled by write_php_ini, so
# this list decides which extensions a fixture install ends up with.
extdir="$linux/lib/php/extensions/no-debug-non-zts-20240924"
for module in Core curl mbstring openssl fileinfo mysqli pdo_mysql gd zip intl; do
    printf 'ELF fixture module %s\n' "$module" > "$extdir/$module.so"
done

tar czf php-$VERSION-linux-x64.tar.gz "${TAR_REPRO[@]}" -C "$STAGE" "php-$VERSION"

# ---------------------------------------------------------------------------
# linux-x64, broken on purpose: an archive that verifies perfectly and will not
# run. This is the case post-install verification exists for - a matching
# digest proves nothing about whether the binary executes.
# ---------------------------------------------------------------------------
broken="$STAGE/php-8.4.3"
mkdir -p "$broken/bin" "$broken/lib/php/extensions/no-debug-non-zts-20240924" "$broken/etc"
cp src/php "$broken/bin/php"
cp src/php-config "$broken/bin/php-config"
chmod 755 "$broken/bin/php" "$broken/bin/php-config"
cp src/NEWS "$broken/NEWS"
# The control file the fixture reads to refuse to start, baked into the archive.
echo 127 > "$broken/bin/.fixture-fail"
for module in Core openssl; do
    printf 'ELF fixture module %s\n' "$module" > "$broken/lib/php/extensions/no-debug-non-zts-20240924/$module.so"
done

tar czf php-8.4.3-linux-x64.tar.gz "${TAR_REPRO[@]}" -C "$STAGE" "php-8.4.3"

# ---------------------------------------------------------------------------
# windows-x64: the Win32 layout, where php.ini and the DLLs sit beside php.exe.
# ---------------------------------------------------------------------------
win="$STAGE/php-$VERSION-Win32-vs17-x64"
mkdir -p "$win/ext"
cp src/php.exe.txt "$win/php.exe"
cp src/php.ini-development "$win/php.ini-development"
cp src/NEWS "$win/NEWS"
for module in curl mbstring openssl fileinfo mysqli pdo_mysql gd zip intl; do
    printf 'MZ fixture module %s\n' "$module" > "$win/ext/php_$module.dll"
done

# Deterministic zip: fixed timestamps, no extras. Python's zipfile writes a
# stable central directory for a fixed input order.
python3 - "$STAGE" "php-$VERSION-Win32-vs17-x64" "php-$VERSION-windows-x64.zip" <<'PYEOF'
import sys
import zipfile
from pathlib import Path

stage, top, out = sys.argv[1], sys.argv[2], sys.argv[3]
root = Path(stage) / top
entries = sorted(p for p in root.rglob("*") if not p.name.startswith("."))
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zf:
    for path in entries:
        info = zipfile.ZipInfo(str(path.relative_to(root.parent)))
        info.date_time = (2024, 12, 17, 18, 22, 53)
        info.compress_type = zipfile.ZIP_DEFLATED
        if path.is_dir():
            info.external_attr = 0o40755 << 16
            zf.writestr(info, b"")
        else:
            # php.exe must be executable so the archive models a real build.
            info.external_attr = 0o100755 << 16
            zf.writestr(info, path.read_bytes())
PYEOF

# ---------------------------------------------------------------------------
# Sidecars, and a summary the maintainer copies into catalogue.json.
# ---------------------------------------------------------------------------
for archive in php-$VERSION-linux-x64.tar.gz php-8.4.3-linux-x64.tar.gz php-$VERSION-windows-x64.zip; do
    sha256sum "$archive" | sed "s|  |  |" > "$archive.sha256"
    echo "$archive  $(sha256sum "$archive" | cut -d' ' -f1)"
done
