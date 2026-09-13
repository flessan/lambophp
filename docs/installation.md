# Installation

## Windows 10 / 11 (x86_64)

Windows is the primary target. Two options, neither needing administrator
rights.

### Installer

1. Download `LamboPHP-Setup.exe` from the
   [latest release](https://github.com/flessan/kink-php-dev/releases).
2. Verify it against `SHA256SUMS` in the same release:

   ```powershell
   Get-FileHash -Algorithm SHA256 .\LamboPHP-Setup.exe
   ```

3. Run it. The install is per-user, into
   `%LOCALAPPDATA%\Programs\LamboPHP`, and adds that directory to *your*
   `PATH` - not the system one.
4. On the last page, tick **Run lambo doctor now** to see what is missing.

The installer writes nothing outside its own directory, your `PATH` and the
Start Menu. Uninstalling removes those three things and leaves your PHP
versions, databases and projects alone.

### Portable

Download `LamboPHP-windows-amd64.zip`, extract it anywhere, and run
`lambo.exe` from there. Nothing is registered and no `PATH` entry is needed:

```powershell
.\lambo.exe --version
```

For a fully self-contained install - binary, runtimes and data on one drive:

```powershell
$env:LAMBO_HOME = "D:\Lambo"
.\lambo.exe doctor
```

### After installing

```powershell
cd C:\code\my-project
lambo init
lambo up
start http://localhost
```

Details and the manual checks CI cannot perform:
[windows.md](windows.md).

## Linux

```bash
curl -fsSL https://raw.githubusercontent.com/flessan/kink-php-dev/main/scripts/install.sh | sh
```

The script resolves `linux-amd64` or `linux-arm64`, verifies the download
against the release `SHA256SUMS`, and puts `lambo` in `~/.local/bin`. It never
uses `sudo`.

| Variable | Default | Purpose |
| --- | --- | --- |
| `LAMBO_VERSION` | latest release | Install a specific tag |
| `LAMBO_INSTALL_DIR` | `~/.local/bin` | Where the binary lands |
| `LAMBO_HOME` | `~/.lambo` | Where Lambo keeps its data |

## macOS

The same script installs `macos-amd64` (Intel) or `macos-arm64` (Apple
silicon). Gatekeeper may quarantine the binary on first run:

```bash
xattr -d com.apple.quarantine ~/.local/bin/lambo
```

## From source

```bash
git clone https://github.com/flessan/kink-php-dev.git
cd kink-php-dev
cargo build --release
./target/release/lambo --version
```

Rust 1.85 or newer (edition 2024). The build has no C dependencies and needs
no network access once the crates are vendored. See
[development.md](development.md).

## Verifying an install

```bash
lambo --version     # prints lambo 0.9.0
lambo doctor        # every check, each failure with the command that fixes it
```

On a fresh machine `lambo doctor` will report that PHP, the web server and the
database are missing. That is the expected starting state, not a broken
install.

## Pinning checksums

Lambo downloads PHP, Apache, MariaDB and phpMyAdmin - and refuses to extract or
execute anything it cannot verify. The catalogue that ships in the binary has
`sha256: null` for every entry, which means "look for an upstream
`<url>.sha256` sidecar". Most of these upstreams do not publish one, so **on a
fresh install every download fails closed** with a message naming the
catalogue directory.

Pin the releases you want by writing a JSON file into
`<home>/config/catalogs/` (`php.json`, `apache.json`, `mariadb.json`,
`mysql.json`, `dbui.json`). Entries replace the built-in ones when `version`
and `platform` match:

```json
{
  "php": [
    {
      "version": "8.4.2",
      "platform": "windows-x64",
      "url": "https://windows.php.net/downloads/releases/archives/php-8.4.2-Win32-vs17-x64.zip",
      "sha256": "…64 hex characters…"
    }
  ]
}
```

Compute the digest from a copy you trust:

```powershell
# Windows
Get-FileHash -Algorithm SHA256 .\php-8.4.2-Win32-vs17-x64.zip
```

```bash
# Linux / macOS
sha256sum php-8.4.2-cli-linux-x86_64.tar.gz
```

Then `lambo php install 8.4` verifies against it before extracting.

Platform keys are `<os>-<arch>`: `windows-x64`, `linux-x64`, `linux-arm64`,
`macos-x64`, `macos-arm64`. `lambo php list-versions` shows what the catalogue
offers for the machine you are on.

> [!NOTE]
> Checksums are the reason this step exists at all: a development tool that
> downloads and executes binaries should not guess about their integrity, and
> a wrong digest is a bug you want to hear about loudly.

## What is not covered yet

- **macOS Apache and MariaDB**: no catalogue entries. Use Homebrew's `httpd`
  and `mariadb`, or `lambo config set server.kind php` for PHP's built-in
  server.
- **Linux Apache**: same - use your distribution's package, or
  `server.kind: php`.
- **phpMyAdmin**: no checksum is pinned, so `lambo db install-ui` fails closed.
  Pin one in `catalogs/dbui.json`, or install the manager so that
  `<home>/dbui/index.php` exists.

These are tracked in [roadmap.md](roadmap.md).

## Uninstalling

1. `lambo down` - stop anything still running.
2. Run the uninstaller (Windows) or delete `~/.local/bin/lambo`.
3. Delete the home directory if you want the runtimes and data gone too:
   `%USERPROFILE%\Lambo` or `~/.lambo`.

Removing the binary alone leaves your data intact, so a reinstall picks up
exactly where you left off.
