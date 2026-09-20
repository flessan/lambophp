# Troubleshooting

Start with `lambo doctor`. Every finding names the command that fixes it, and
the checks run in the same order `lambo up` does, so the first failure is
usually the real one.

```bash
lambo doctor
lambo logs apache -n 200
lambo status
```

---

## Downloads fail with "cannot be verified"

```
error: cannot verify `https://dl.static-php.dev/static-php-cli/common/php-8.4.2-cli-linux-x86_64.tar.gz`:
php 8.4.2 for linux-x64 has no pinned digest in the catalogue. Lambo never
downloads, unpacks or runs an artifact whose checksum it cannot confirm, so
nothing was fetched. Pin the digest: `config/catalogs/<family>.json` is where a
pinned `sha256` goes, and `lambo config hash <file>` computes the value.
Alternatively install PHP yourself and put it on the machine for Lambo to find.
```

There is no `lambo config set php.<version>.sha256` key and no `--skip-checksum`
flag - see the correction below if you found an older Lambo telling you
otherwise.

The command exits 1 and **nothing is downloaded** - not even to a temporary
file.

This is the fail-closed behaviour working as intended, not a network problem.
The catalogue ships with `sha256: null` for every entry, and Lambo will not
extract or execute a binary it cannot check.

`sha256: null` means *not known*, not *verification optional*. Before refusing,
Lambo looks for a `.sha256` file published beside the artifact and verifies
against that. The message above appears only when there is no digest anywhere.
The message names the artifact, the family, the version, the platform, and the
exact command that fixes it.

Pin the release you want:

```bash
# 1. download it yourself and compute the digest
lambo config hash php-8.4.2-cli-linux-x86_64.tar.gz
# (or sha256sum - the output format is the same)

# 2. write <home>/config/catalogs/php.json
```

Before you trust that digest, check it against the value the publisher
publishes. A digest you computed from the file you are about to install only
proves the file equals itself; the point is to confirm it equals what upstream
says it is.

```json
{ "php": [ { "version": "8.4.2", "platform": "linux-x64",
  "url": "https://dl.static-php.dev/static-php-cli/common/php-8.4.2-cli-linux-x86_64.tar.gz",
  "sha256": "…64 hex characters…" } ] }
```

Full instructions: [installation.md#pinning-checksums](installation.md#pinning-checksums).

**A wrong digest** produces `checksum mismatch` and leaves nothing extracted.
That is also correct: recompute it from a copy you trust.

## A port is already in use

```
error: port 8080 failed to start: … is in use
  possible causes:
    - another application may be using it (Docker, IIS, …)
  next: lambo doctor
```

Lambo never kills another application's process. Change the port instead:

```bash
lambo config set server.port 8088
```

`lambo doctor` and `lambo status` report which of Lambo's ports are occupied
and, where the platform allows it, what is holding them. On Windows the usual
suspects are IIS/`http.sys`, SQL Server, a separately installed MySQL, and
Docker Desktop's port proxies.

## "no Apache release for linux-x64 in the catalogue"

Lambo ships Apache builds for Windows only. On Linux and macOS either install
Apache with your package manager (`apt install apache2`, `brew install httpd`)
- Lambo finds it on `PATH` - or use PHP's built-in server:

```bash
lambo config set server.kind php
```

The same applies to MariaDB on macOS.

## The server started but nothing answers

```
error: Apache failed to start: the server started but never answered an HTTP
request
  possible causes:
    - the error log may say why: <home>/logs/apache/httpd.log
    - a PHP fatal error during start-up can also stop the first response
  next: lambo logs apache
```

The process is alive and the port was free, so the cause is almost always in
the log. The two common ones:

- **A PHP fatal error at start-up** - check `lambo logs php` too.
- **A `document_root` that does not exist** - `lambo init` records what it
  detected; if the project moved, edit `lambo.yml`.

## `lambo status` says a service is stopped, but it was running

That is the observed answer, and it is correct: the process is gone. Lambo
records a pid, a start time and a command line, and re-checks all three, so a
service killed from Task Manager or `kill` is reported as stopped rather than
as a ghost.

Restart it with `lambo server start` / `lambo db start`, or `lambo up` for the
whole project.

## `lambo db open` says the database manager is not installed

```bash
lambo db install-ui
```

phpMyAdmin - the default manager - is pinned in the shipped catalogue and
installs out of the box. Adminer carries no digest its publisher exposes, so
it fails closed like any other unverifiable download: pin one in
`<home>/config/catalogs/dbui.json`, or install the manager yourself so that
`<home>/dbui/index.php` exists - Lambo serves whatever is at that path, whether
through Apache's `/phpmyadmin` alias or with `php -S` on loopback.

## The database will not start

```bash
lambo logs database -n 200
lambo db status
```

In order of likelihood:

1. **Something else owns port 3306** - a system MySQL, or a Docker container.
   `lambo doctor` reports it.
2. **An incomplete data directory** - a previous start was interrupted.
   Remove `<home>/database/data` and run `lambo db start` to re-initialize.
   This deletes the databases in it.
3. **The password was changed outside Lambo** - the generated password lives
   in `<home>/config/lambo.yml`. If you set a different one with
   `mysqladmin`, update it with `lambo config set database.password …`.

## `lambo init` says the wrong framework

Detection reports its evidence, including the probes that came back negative:

```bash
lambo init --dry-run
```

If a probe is missing something, the fix is usually one file: Laravel needs
`artisan`, Symfony needs `bin/console`, WordPress needs `wp-settings.php`.
Whatever it writes, `lambo.yml` is yours to edit - set
`server.document_root` and `database.kind` directly and `lambo up` will honour
them.

## `.env` was not written

`lambo up` writes database keys only, and never overwrites a key that already
has a value:

```
✔ env: .env: wrote DB_HOST, DB_PORT, DB_DATABASE
· env: .env already had what it needs (…)
```

An empty value counts as unset and *is* written. Comments, ordering and CRLF
line endings are preserved, and the write is atomic.

## PHP is installed but will not run

```text
  version        8.4.2
  path           /home/you/.lambo/php/8.4.2
  executable     /home/you/.lambo/php/8.4.2/bin/php
  reported       PHP did not run
  configuration  none loaded
⚠ `…/bin/php` exited with code 127: error while loading shared libraries:
  libonig.so.5: cannot open shared object file
```

This is a different failure from `corrupt` below. `corrupt` means the directory
disagrees with its manifest - a check about files. This means the files are all
there and the binary still will not execute, which only starting it can find.

`lambo php install` runs this check itself, so a fresh install that fails this
way never gets recorded as installed. Reaching this state means something
changed afterwards.

| What PHP printed | Cause | Fix |
| --- | --- | --- |
| `error while loading shared libraries: libX.so` | A library the build needs is missing. Slim Linux images often lack `libonig`, `libzip`, `libicu` or `libxml2` | Install the library, or install a PHP build that does not need it |
| `cannot execute binary file: Exec format error` | An archive for the wrong architecture or OS was unpacked here | `lambo php remove <version>`, then reinstall for this platform |
| `Permission denied` | The executable bit was lost - an archive extracted with a tool that drops modes, or a volume mounted `noexec` | Restore the bit, or move the Lambo home off the `noexec` mount |
| Exit code with no output at all | Usually a crash in the loader before PHP could print | Run the executable directly to see the loader's own message |

In every case the archive itself was fine - Lambo verified its digest before
unpacking, and says so in the install error. Re-downloading is not the fix.

```bash
lambo php remove 8.4.2
lambo php install 8.4.2
```

See [runtime.md](runtime.md#is-it-actually-working) for what each field of
`lambo php current` means.

## `lambo php list` says a version is `corrupt`

Every install writes a manifest into the runtime directory recording the
version, platform, source URL, the digest it was verified against, and the file
list at completion. `lambo php list` checks the directory against that record,
and `corrupt` means the two disagree:

```text
VERSION  STATUS    PATH
8.3.16   corrupt   /home/you/.lambo/php/8.3.16
```

| Cause | What happened | Fix |
| --- | --- | --- |
| Recorded files are missing | Something deleted part of the runtime - most often an emptied `ext/` directory, which leaves a PHP with silently no `openssl` | `lambo php remove 8.3.16` then `lambo php install 8.3.16` |
| The directory was renamed | The name no longer describes the contents, so `lambo php use` would select a version Lambo never installed | Rename it back, or remove and reinstall |
| The manifest names another family | The directory holds something other than what it is filed under | Remove and reinstall |

Reinstalling over a corrupt runtime is safe: the previous directory is moved
aside rather than deleted, and only discarded once the new one is in place. If
the install fails part-way the old runtime is put back, so you are never left
with neither.

A runtime with **no** manifest - one you unpacked by hand, or one installed
before the manifest existed - is listed as `installed`, not `corrupt`. Lambo
cannot vouch for it but has no evidence against it.

## `lambo php remove` refuses to remove a version

```
error: PHP 8.4.2 is in use by the running `apache` service; run `lambo down` first
```

The runtime is being used by a service Lambo started. Removing the files would
not stop that service - it would keep running from files that no longer have a
name, and then could not be restarted, leaving an orphan that Lambo's own state
still reports as running. Stop the services first:

```bash
lambo down
lambo php remove 8.4.2
```

A *stale* record - one whose process has already exited - does not block the
removal; liveness is re-checked rather than trusted.

## Windows-specific

| Symptom | Cause and fix |
| --- | --- |
| SmartScreen blocks the installer | Unsigned binary. **More info → Run anyway**, or verify `SHA256SUMS` first |
| First `lambo php install` is slow | Defender scanning extracted binaries; later runs are fast |
| Ctrl+C in the console does not stop services | Intended - they are detached. Use `lambo down` |
| Processes survive `lambo down` | Should not happen; report it with `tasklist` output. Lambo stops only what it recorded |
| `lambo config edit` opens the wrong editor | Set `VISUAL` or `EDITOR`; the default is `notepad` |

See the full [Windows guide](windows.md).

## Nothing above matches

Collect this and open an issue:

```bash
lambo --version
lambo doctor
lambo status
lambo logs apache -n 200
lambo logs lambo -n 200
```

Please include the platform (`lambo status` prints it) and, if a download is
involved, the URL from the error message.
