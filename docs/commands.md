# Commands

Every command is listed here with what it actually does. Where a command can
fail, the failure mode is described too - Lambo reports what it observed, not
what it intended.

```
lambo [COMMAND]
```

With no command, `lambo` runs [`status`](#lambo-status).

---

## Project lifecycle

### `lambo init`

Detects what the current directory is and writes `lambo.yml`.

| Flag | Effect |
| --- | --- |
| `--name <name>` | Project name (default: composer package name, else the directory name) |
| `--php <spec>` | PHP requirement, e.g. `8.3`, `~8.3`, `stable` |
| `--port <port>` | HTTP port for this project |
| `--database <engine>` | `mariadb`, `mysql` or `none` |
| `--force` | Overwrite an existing `lambo.yml` |
| `--dry-run` | Report the detection without writing anything |

Detection is evidence-based, and the evidence is printed - including the
probes that came back negative, so you can see *why* it concluded "plain PHP"
rather than "Laravel":

```
── Detected
  framework      Laravel
  · [yes] artisan (Laravel's console entry point)
  · [yes] composer.json (composer package: laravel/framework)
  · [no ] wp-settings.php (WordPress core)
  · [yes] public/ (front controller directory)
```

| Framework | Document root | Database |
| --- | --- | --- |
| Laravel | `public` | provisioned |
| Symfony | `public` | provisioned |
| WordPress | `.` | provisioned |
| CodeIgniter | `.` | provisioned |
| Composer project | `public` if present, else `.` | provisioned |
| Plain PHP | `.` | not provisioned |

A `kingphp.yml` in the directory is converted in place; the original file is
left alone.

### `lambo up`

Starts the project. The order is fixed, and each step is confirmed before the
next one runs:

1. **validate** - the project file is parsed and checked before anything starts
2. **credentials** - a database password is generated if there is none
3. **ports** - every port the project needs is checked *before* anything binds
4. **php** - resolved, installed if missing
5. **database** - installed, initialized, secured, started, health-checked over TCP; the project database is created
6. **env** - `.env` gets the keys it needs, without overwriting existing values
7. **server** - configuration generated, validated, started; then the URL must answer
8. **browser** - opened only once the URL actually responds

| Flag | Effect |
| --- | --- |
| `--no-browser` | Do not open the browser |

A failure stops the sequence and explains itself: what failed, the plausible
causes, the log to read, and the command to run next. Whatever did start is
recorded, so `lambo down` can still clean up.

### `lambo down`

Stops every service Lambo started, in reverse start order. Only processes with
a record in Lambo's state file are touched - a database you started yourself is
left running.

### `lambo restart`

`down` followed by `up`. Deliberately not a signal: in development a restart
means a clean slate - regenerated configuration, re-checked ports, re-validated
`httpd.conf`.

### `lambo status`

The default command. Shows the Lambo home and platform, the project, and every
service with its **observed** state - a service whose process died between
commands is reported as stopped, not as running.

When a port Lambo wants is held by something else, `status` says who holds it.

### `lambo open`

Opens the project URL in your browser. If the URL is not answering it says so
instead of opening a dead page.

| Flag | Effect |
| --- | --- |
| `--db` | Open the database manager instead |

### `lambo doctor`

Runs the diagnostic suite and exits non-zero if anything failed.

Checks: Lambo home · configuration · download transport · catalogue contents ·
runtime readiness · PHP · web server · database · database UI · every port · the
project and its PHP/Apache requirements · recorded services.

*Catalogue contents* counts what is listed; *runtime readiness* reports what can
actually be installed, because an entry can be listed for this platform and still
be uninstallable when nothing can confirm its bytes.

`lambo doctor --release` adds the release gate: the same validation with the rule
that an entry with no pinned SHA-256 is an **error** rather than a fact to work
around. It validates metadata only and downloads nothing, so it is cheap enough
for every commit and for the release pipeline. See
[release.md](release.md#release-gate).

Every warning and failure carries the command that fixes it, and no check ever
suggests a command that does not exist.

Findings are graded:

| Verdict | Meaning | Exit code |
| --- | --- | --- |
| `ok` | As it should be | |
| `--` unsupported | This platform cannot do it; the alternative is named | 0 |
| `!!` warning | Something is missing but Lambo works around it | 0 |
| `xx` failure | A command will not work until this is fixed | 1 |

`unsupported` exists because some gaps are facts rather than faults. The
catalogue publishes Apache for Windows only, and MariaDB and MySQL for Windows
and Linux only - so on macOS those checks report `unsupported` with the
workaround (`server.kind: php`, or install the server yourself and Lambo will
find it) instead of a failure no command can fix.

### `lambo logs [group]`

| Argument / flag | Effect |
| --- | --- |
| `apache` \| `database` \| `php` \| `lambo` | Which log (default: `apache`) |
| `-n, --lines <n>` | Trailing lines to show (default 50) |
| `-f, --follow` | Keep printing as the log grows |
| `--clear` | Empty the log files (truncated, not deleted, so a running service keeps its handle) |

Logs live under `<home>/logs/<group>/`, identically on every platform.

### `lambo migrate`

Moves an existing king-PHP installation to Lambo: global configuration, the
workspace registry, and every project's `kingphp.yml` → `lambo.yml`.

Nothing is deleted, no runtime is copied, and `KING_HOME` is never used to
locate the Lambo home - new installs are Lambo-only. `--dry-run` reports what
would happen.

---

## `lambo php`

| Command | Effect |
| --- | --- |
| `list` | Every version the catalogue and this machine know about, as a table |
| `list-versions` | Just the versions the catalogue offers for this platform |
| `install [version]` | Download, verify, extract, generate `php.ini`, **confirm it runs**, activate |
| `use <version>` | Make an installed version active |
| `current` | Active version, and what PHP itself reported when Lambo started it |
| `remove <version>` | Delete an installed version |
| `ini` | Path of the generated `php.ini` |
| `modules` | Extensions the active PHP actually loaded (`php -m`) |
| `run <args…>` | Run the active PHP; exits with PHP's own status |
| `-v`, `-m`, `script.php`, … | Anything else is passed straight to PHP |

`install` does not report success on the strength of the files it wrote. It
starts the binary once and refuses the install if it will not run - see
[runtime.md](runtime.md#the-install-pipeline).

`current` prints the version Lambo installed *and* the version PHP reported:

```text
  version        8.4.2
  path           /home/you/.lambo/php/8.4.2
  executable     /home/you/.lambo/php/8.4.2/bin/php
  reported       8.4.2
  configuration  /home/you/.lambo/php/8.4.2/etc/php.ini
  extensions     12
```

A runtime that will not run shows `reported  PHP did not run` and exits non-zero
with the reason PHP gave. `modules` behaves the same way; it no longer guesses
"it may be broken" from an empty list. Details in [runtime.md](runtime.md).

`lambo php list` merges the local inventory with the catalogue, so one command
answers "what can I run, and what do I have":

```text
VERSION  STATUS       PATH
8.4.2    active       /home/you/.lambo/php/8.4.2
8.3.16   corrupt      /home/you/.lambo/php/8.3.16
8.2.27   available    not installed
8.1.31   unavailable  only for windows-x64
```

A version published only for another platform is listed rather than hidden -
silently dropping it is what makes someone conclude Lambo is broken - and the
platforms it *does* exist for are named. A runtime placed by hand, which the
catalogue does not mention, is still listed as `installed`.

`corrupt` means the directory is there but Lambo cannot vouch for what is in
it. Every install writes a manifest into the runtime recording the version,
platform, source URL, verified digest and the file list at completion; `list`
checks against it. A runtime is `corrupt` when

- files recorded at install time are missing - the common one is an extension
  directory that has been emptied, which otherwise leaves you debugging a PHP
  that silently has no `openssl`;
- the directory has been renamed to a version Lambo never installed, so the
  name no longer describes the contents;
- the manifest says the directory holds a different family entirely.

Integrity outranks selection: an active-but-broken runtime reports `corrupt`,
not `active`, and corrupt rows sort above the healthy installed ones so they are
not scrolled past. A directory with **no** manifest - one you unpacked yourself,
or one written before the manifest existed - is still `installed`: Lambo cannot
vouch for it, but it has no evidence against it either.

`lambo php remove` refuses to delete a runtime a live Lambo service is running
from, and names `lambo down` as the fix. Deleting the files under a running
`httpd` does not stop it: it keeps serving from unlinked files and then cannot
be restarted.

Lambo never modifies your `PATH`. `lambo php -v` runs the managed PHP with a
generated `php.ini` through `PHPRC`, and nothing else on the machine changes.

Version specs: `stable` (newest non-prerelease), `latest`, `nightly`, `8.4`
(the 8.4.x line), `8.4.2` (exact), `^8.3`, `>=8.1 <8.5`.

---

## `lambo server`

| Command | Effect |
| --- | --- |
| `start` | Start the project's web server (Apache, or PHP's built-in server) |
| `stop` | Stop it |
| `restart` | Stop, then start |
| `status` | Whether it is running, its pid, port and uptime |

`start` checks the port first and confirms the URL answers afterwards, so a
reported success always means something is serving. Apache is installed
automatically when a build exists for the platform; otherwise
`lambo config set server.kind php` switches to PHP's built-in server.

---

## `lambo db`

| Command | Effect |
| --- | --- |
| `install [--engine mariadb\|mysql] [--version v]` | Download and install the server |
| `install-ui` | Install the browser database manager (phpMyAdmin) |
| `start` | Initialize the data directory if needed, start, health-check |
| `stop` / `restart` | Stop / restart the server |
| `status` | Observed state, pid, port, uptime, log path |
| `create [name]` | Create a database (default: the project's) |
| `drop <name> [--force]` | Drop a database; refuses without `--force` |
| `list` | List user databases |
| `shell [name]` | Interactive SQL client |
| `open` | Start the database manager and open it in a browser |
| `credentials [--show-password]` | Print connection details |

The server binds to `127.0.0.1` only. The root password is generated once and
stored in Lambo's configuration - never in a project file, never in a
`my.cnf`, and never on a command line (clients receive it through
`MYSQL_PWD`). `credentials` hides the password unless you ask for it.

---

## `lambo config`

| Command | Effect |
| --- | --- |
| `show` | Effective configuration as YAML |
| `list` | Every key with its current value |
| `get <key>` | One value |
| `set <key> <value>` | Validate, then save |
| `path` | Where the configuration file is |
| `edit` | Open it in `$EDITOR`/`$VISUAL`, then re-validate |
| `hash <file>` | Print a file's SHA-256, for pinning in a catalogue |

`hash` prints and stops. It never records the value: a digest is a claim that
these bytes are the right ones, and that claim needs a human to make it. Check
the output against the publisher's own published checksum before you pin it.

Keys: [docs/configuration.md](configuration.md).

---

## `lambo workspace`

| Command | Effect |
| --- | --- |
| `list` | Workspaces and their projects |
| `add [path] [-w name]` | Add a project (default: current directory, workspace `default`) |
| `remove [path] [-w name]` | Remove a project |

A workspace is a list of directories and nothing more - it holds no
configuration, so a project behaves identically inside and outside one.

---

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success |
| `1` | The command failed; the reason is on stderr with its causes and the next step |
| *n* | `lambo php …` returns PHP's own exit status |

`lambo doctor` exits `1` when any check failed.
