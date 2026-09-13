# Configuration

Three layers, applied in order. Nothing is hidden: `lambo config show` prints
the effective result, and `lambo status` prints where the home came from.

```
1. global      <home>/config/lambo.yml      machine-wide defaults
2. project     <project>/lambo.yml          this project only
3. state       <home>/data/services.yml     what is running right now
```

A project file wins over the global one; runtime state never changes either -
it only records observations.

## The home directory

| Platform | Default | Override |
| --- | --- | --- |
| Windows | `%USERPROFILE%\Lambo` | `LAMBO_HOME` |
| Linux / macOS | `~/.lambo` | `LAMBO_HOME` |

`LAMBO_HOME` exists for portable installs: point it at a USB stick or a
per-project directory and Lambo takes everything with it.

```
<home>/
  bin/          helper binaries
  cache/        verified runtime archives, safe to delete
  php/<ver>/    one directory per PHP version
  apache/       the web server
  database/     server binaries, data directory, generated my.cnf
  dbui/         the database manager
  projects/     projects Lambo created or adopted
  logs/         apache/ database/ php/ lambo/
  config/       lambo.yml, workspaces.yml, catalogs/
  data/
    services.yml  what Lambo started, with pids and ports
```

### The download cache

`cache/` holds runtime archives that have already been downloaded **and
verified**. The rules are the reason it can be trusted:

- A download lands in `<name>.part` and is hashed there. Only a file that
  matches its digest is renamed into place, so a partial or interrupted
  transfer can never be mistaken for a cache entry.
- A cached file is re-verified against its digest before it is reused. Editing
  or corrupting an entry does not poison an install; the entry is refreshed
  from the source.
- A download that fails verification is deleted. Nothing rejected survives on
  disk.
- Extraction goes to a `<version>.installing` staging directory and is renamed
  into place only once it is complete, so a killed `lambo php install` leaves
  something that is visibly not a runtime - and a retry succeeds over the top
  of it.

Deleting `cache/` is always safe: the next install downloads again. It is
purely a bandwidth saving.

`KING_HOME` is **not** honoured: it is only read by `lambo migrate`, so a
legacy installation can be moved without a new install silently adopting it.

## Global keys

`lambo config list` prints these with their current values.

| Key | Default | Meaning |
| --- | --- | --- |
| `php.default` | `stable` | PHP version used when a project does not say |
| `server.kind` | `apache` (or `php` where no Apache build exists) | `apache` or `php` |
| `server.port` | `80` | HTTP port. Gives `http://localhost`; if it cannot be bound, Lambo moves to the next free port and says so - see [ADR-0007](adr/0007-localhost-first-ports.md) |
| `server.https_port` | `443` | Reserved for TLS |
| `database.kind` | `mariadb` | `mariadb`, `mysql` or `none` |
| `database.port` | `3306` | TCP port; the server binds to `127.0.0.1` only. Fixed by contract - a conflict is reported, not worked around, because `.env` records it |
| `database.username` | `root` | Administrative user |
| `database.password` | *(generated)* | Created on first use; see below |
| `dbui.kind` | `phpmyadmin` | Browser database manager (`phpmyadmin` or `adminer`) |
| `dbui.port` | `8081` | Fallback port, used only when Apache is not serving the project. With Apache the manager is mounted at `/phpmyadmin` on the project's own port |
| `paths.projects` | *(empty)* | Where `lambo`-managed projects live |
| `browser.open` | `true` | Whether `lambo up` opens a browser |
| `sources.mirror` | *(empty)* | Base URL artifacts are fetched from instead of the official hosts; see [Artifact sources](#artifact-sources) |
| `sources.artifacts` | `<home>/cache/artifacts` | Directory searched for pre-downloaded artifacts |

Values are validated when they are set: `lambo config set server.port 99999`
is rejected with `expected a number 1-65535`, and the file is left untouched.

### The database password

Lambo generates a strong password the first time a database is provisioned and
stores it in `<home>/config/lambo.yml`, with the file restricted to the owning
user. It is never:

- written into a project file, so committing `lambo.yml` leaks nothing;
- written into `my.cnf`;
- passed on a command line, where it would be visible in a process listing -
  clients receive it through the `MYSQL_PWD` environment variable.

`lambo db credentials` hides it unless you pass `--show-password`.

## The project file

See [lambofile.md](lambofile.md).

## Catalogue overrides

The download catalogue ships inside the binary and can be overridden per
family by dropping a JSON file into `<home>/config/catalogs/`:

```
catalogs/php.json      catalogs/apache.json      catalogs/mariadb.json
catalogs/mysql.json    catalogs/dbui.json
```

Entries replace the built-in ones when `version` and `platform` match. This is
how a stale URL, a corporate mirror, or a missing checksum is fixed without
waiting for a Lambo release:

```json
{
  "php": [
    {
      "version": "8.4.2",
      "platform": "windows-x64",
      "url": "https://windows.php.net/downloads/releases/archives/php-8.4.2-Win32-vs17-x64.zip",
      "sha256": "d9a4c1a8…64 hex characters…f0"
    }
  ]
}
```

Malformed JSON is reported as an error naming the file - never silently
ignored, because a silently ignored override means an unexpected binary.

### Optional entry fields

Beyond `version`, `platform`, `url`, `sha256` and `checksum_url`, an entry may
describe its artifact precisely. All are optional; every existing catalogue
keeps loading unchanged.

| Field | Values | Purpose |
| --- | --- | --- |
| `archive_format` | `zip`, `tar.gz`, `tar.xz`, `single-file` | How the artifact is packaged. Normally inferred from the URL, so this is for URLs that do not end in a recognisable name. Used by validation: a format Lambo cannot unpack is rejected before anything is downloaded. |
| `size` | bytes | Expected download size. A transfer that stopped halfway is then reported as `expected 43117568 bytes, got 3145728` rather than as a digest mismatch that reads like tampering. A wrong size deletes the file. |
| `executable` | path inside the archive | The primary executable, relative to the runtime root. **Checked at install time**: if the unpacked archive has no such file the install fails, because the entry describes a different artifact. Recorded in the install manifest. |
| `filename` | any file name | The name to cache under, when the URL is a redirect or a query string. |
| `channel` | any label | Informational. Lambo resolves `stable`/`latest`/`nightly` from the version's pre-release tags, not from this field, so setting it does not change what `lambo php install stable` picks. |

`archive_format: "tar.xz"` can be written but is **not installable** - Lambo has
decompressors for gzip and zip and none for xz. Declaring it is how a mirror
entry says honestly what it is; validation then rejects it before anything is
downloaded rather than after a multi-hundred-megabyte transfer completes.

### Validation

`lambo doctor` validates the catalogue before it counts anything. Three PHP
releases where one is dead is not "3 PHP releases".

Blocking problems fail the check:

- two entries claiming the same family, version and platform
- an empty version, or one that is not a version
- a platform key Lambo does not know
- a URL that is not `https://`
- a format that cannot be determined, or one Lambo cannot unpack
- a malformed digest (a `null` digest is *not* a problem - see below)
- a `checksum_url` that cannot be used

Warnings do not fail the check:

- an entry for a platform Lambo does not manage on this machine
- a single file declared for a runtime family, which is normally distributed as
  an archive. The database manager is the one family that *is* a single file, so
  declaring one for it is not a warning.

A failure names where the entry came from, because the two cases need opposite
fixes. An entry from your own file in `config/catalogs/` is edited there; an
entry in the catalogue Lambo ships is fixed by updating Lambo, and no edit on
your machine will remove it.

## Artifact sources

Where the bytes come from is configurable. What they must hash to is not.

That separation is the whole design. Lambo can fetch an artifact from the
official host, from a mirror you run, or from a file already on disk - but the
expected digest always comes from the catalogue (built in or overridden), never
from the source that supplied the bytes. Letting the thing that provides an
artifact also vouch for it is not verification.

| Key | Default | Effect |
| --- | --- | --- |
| `sources.mirror` | *(empty)* | Base URL the official artifact names are fetched from. `https://`, or `file://` for a mounted share. |
| `sources.artifacts` | `<home>/cache/artifacts` | Directory searched for a pre-downloaded artifact. |

```bash
lambo config set sources.mirror https://mirror.internal/lambo
lambo config set sources.artifacts "D:\Lambo Artifacts"
```

### Precedence

For each artifact, in order:

1. **An explicit override** - an entry in `config/catalogs/<family>.json`. You
   named this artifact, so nothing else is consulted.
2. **A local artifact** - a file in the artifacts directory under the name
   Lambo derives from the artifact's identity.
3. **A mirror** - the official artifact fetched from `sources.mirror`.
4. **The official catalogue URL.**

### Mirrors

A mirror is addressed by artifact *name*, not by mirroring the upstream
directory structure, so a mirror is a flat directory of files - which is what an
internal artifact store or a share actually is:

```
sources.mirror = https://mirror.internal/lambo
  → https://mirror.internal/lambo/php-8.4.2-linux-x64.tar.gz
```

The expected digest does not move with it. A mirror that serves different bytes
fails with a checksum mismatch and nothing is installed, which is the correct
outcome: a mirror you cannot distinguish from tampering is not a mirror.

The `.sha256` sidecar address also stays pointed at the **official** URL, so a
mirror cannot supply the digest for the bytes it is serving.

### Local artifacts

Place a verified artifact in the artifacts directory under the name Lambo
derives from its identity - `<family>-<version>-<platform>` plus the format's
extension:

```
<home>/cache/artifacts/php-8.4.2-linux-x64.tar.gz
<home>/cache/artifacts/apache-2.4.62-windows-x64.zip
```

The name is derived, never parsed from whatever the file happens to be called,
so identity is unambiguous and a stray file cannot be mistaken for an artifact.
A name Lambo cannot prove is a single safe path component is not used at all, so
a version or platform containing `..` cannot reach outside the directory.

A local artifact is verified exactly like a downloaded one, against the same
catalogue digest. Being on the local disk is not a reason to skip that - and
there is no flag that would. This is what makes an offline install safe rather
than merely convenient: a school lab or a machine with no internet access gets
the same guarantee as one that downloads.

### Provenance

Every install records where it came from in its manifest -
`catalogue`, `override`, `mirror`, `local`, or `unknown` for a manifest written
before source tracking existed. `lambo php list` shows the same distinction in
its `SOURCE` column, along with whether the artifact can be verified at all.

### A null digest is not a skipped check

`"sha256": null` means *the digest is not known*, not *verification is
optional*. Lambo then looks for a `.sha256` file published beside the artifact
and verifies against that. If neither exists the install is refused with
`cannot verify`, naming the exact command that pins the digest:

```
cannot verify `https://…/php-8.4.2-cli-linux-x86_64.tar.gz`: php 8.4.2 for
linux-x64 has no pinned digest in the catalogue. Lambo never downloads, unpacks
or runs an artifact whose checksum it cannot confirm, so nothing was fetched.
Pin the digest: `config/catalogs/<family>.json` is where a pinned `sha256` goes,
and `lambo config hash <file>` computes the value. Alternatively install PHP
yourself and put it on the machine for Lambo to find.
```

There is no `lambo config set php.<version>.sha256` key, and no
`--skip-checksum` flag. A digest is recorded by editing a catalogue file, and
there is no way to install an artifact whose digest is unknown.

Nothing is fetched in that case - not even to a temporary file.

## Environment variables

| Variable | Effect |
| --- | --- |
| `LAMBO_HOME` | The home directory |
| `NO_COLOR` | Disables coloured output ([no-color.org](https://no-color.org)) |
| `TERM=dumb` | Disables coloured output |
| `VISUAL` / `EDITOR` | Used by `lambo config edit` |

Piped output is never coloured, so `lambo config get server.port | xargs …`
behaves on every platform.

## Portability rules

A project file must not contain anything machine-specific:

- `document_root` is relative to the project and may not contain `..`;
- no absolute paths, so `C:\…` never appears in a committed file;
- omitting `server.kind` or `database.kind` inherits the global value, which
  is what lets one project run on a Windows laptop and a Linux CI runner.
