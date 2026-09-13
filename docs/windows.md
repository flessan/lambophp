# Windows

> **This is a Release Candidate (`0.13.0-rc.1`).**
>
> **Physical Windows validation is still required.** Nothing on this page has
> been executed on a real Windows machine: the agent that produced this build
> had no Windows host, no MSVC linker and no installer toolchain, so the
> binaries were compiled for Windows but never run there. Every check in the
> acceptance procedure below is marked `MANUAL WINDOWS TEST` and is yours to
> run. Do not read any green test suite as evidence that the Windows build
> works.

Windows 10 and 11 on x86_64 are the primary target. This page covers how
Lambo behaves there and, at the end, the manual checks that CI cannot perform.

## What is different by design

Nothing in the engine asks "am I on Windows?" and then forks into a different
implementation. Every platform decision goes through one `Os` model, which is
an *argument*, not a compile-time switch - so Windows behaviour is exercised by
the test suite on Linux and macOS too, and a Linux-only bug in a Windows code
path is a test failure rather than a user's surprise.

| Concern | Windows | Unix |
| --- | --- | --- |
| Paths | Written into generated configs with forward slashes, which both Apache and MariaDB accept | as-is |
| Executables | `.exe` resolved by `Os::executable_name` | extension-less |
| Process groups | `CREATE_NEW_PROCESS_GROUP`, so Ctrl+C in a console does not kill a service | setsid-free: signals go to the recorded pid only |
| Stopping a tree | `taskkill /T` - the OS scopes the tree, so nothing outside it can be hit | signal the bare pid, then its children as recorded |
| Graceful shutdown | `httpd -k stop`, `mysqladmin shutdown` | same |
| Opening a browser | `rundll32 url.dll,FileProtocolHandler`, then `explorer.exe` | `xdg-open`, then `wslview` |
| Home directory | `%USERPROFILE%\Lambo` | `~/.lambo` |
| Editor | `notepad` | `vi` |

There is no `cmd /c`, no `/bin/sh`, no `kill -9`, no `/etc/hosts` and no
service registration anywhere in the codebase. Lambo never asks for elevation.

## Ports

| Service | Port |
| --- | --- |
| HTTP | `80` |
| HTTPS | `443` |
| MariaDB / MySQL | `3306` |
| Database manager | `8081` (only when served standalone) |

Port 80 is the default because the workflow is `http://localhost`, and on
Windows binding it needs no administrator rights - there is no privileged-port
rule below 1024. On Linux and macOS the same request falls back to `8080`
automatically and says so, because there 80 *would* need root. See
[ADR-0007](adr/0007-localhost-first-ports.md).

The database manager is normally reached at `http://localhost/phpmyadmin`
through Apache's alias, which is why its own port only matters when the project
is not running.

Before anything binds, `lambo up` checks every port it needs. A port held by
something else is reported as a conflict - naming what holds it where that can
be determined - and never "resolved" by killing the other process.

Windows services that commonly hold these ports: IIS (`http.sys`, port 80 and
443), SQL Server (1433), MySQL/MariaDB installed separately (3306), Docker
Desktop's port proxies, Skype (has historically taken 80). Change the port
instead:

```powershell
lambo config set server.port 8088
```

## Offline and air-gapped machines

A Windows PC with no internet access can still install runtimes, provided a
verified artifact is available locally. Two ways:

**A local artifacts directory.** Place the archive under the name Lambo derives
from its identity, and it is found automatically:

```
%USERPROFILE%\Lambo\cache\artifacts\php-8.4.2-windows-x64.zip
```

The name is derived from family, version and platform - never parsed from
whatever the file happens to be called. Paths with spaces need no quoting,
because nothing is passed through a shell.

**A mirror on a share.**

```
lambo config set sources.mirror file://\\fileserver\LamboArtifacts
```

Either way the artifact is verified against the catalogue's digest before it is
unpacked, exactly as a download would be. Being on the local disk is not a
reason to skip that, and there is no flag that would.

The catalogue currently ships with no pinned digests, so until they are added an
override entry in `%USERPROFILE%\Lambo\config\catalogs\php.json` has to carry
the `sha256`. See [configuration.md](configuration.md#artifact-sources).

## Antivirus and SmartScreen

`LamboPHP-Setup.exe` is not code-signed, so SmartScreen shows "Windows
protected your PC". Click **More info → Run anyway**, or verify the hash
against `SHA256SUMS` first. Defender may also scan extracted PHP binaries on
first run, which makes the first `lambo php install` slower than the rest.

## Ctrl+C

`lambo up` runs services detached, so Ctrl+C in your console stops `lambo`,
not the services. Use `lambo down`. That is deliberate: a developer who
interrupts a foreground command should not lose a database mid-write.

## Logs

```powershell
lambo logs apache -n 100
lambo logs database --follow
lambo logs --clear
```

Everything lives under `%USERPROFILE%\Lambo\logs\`.

---

## Final release-candidate test procedure

The twenty checks below are the release-candidate acceptance list. **Every one
of them is a MANUAL WINDOWS TEST** - none has been executed by the agent,
because none of them can be: they need a real Windows machine, a real browser,
a real Apache and MariaDB, and a real installer.

Run them in order on a clean Windows 10 or 11 machine, signed in as a standard
(non-administrator) user. Record pass or fail against each number.

| # | Test | What to do | Pass means | Result |
| --- | --- | --- | --- | --- |
| 1 | Install | Run `LamboPHP-Setup.exe` | Installs with no UAC prompt; Start Menu entry appears | MANUAL WINDOWS TEST |
| 2 | Launch | Start Menu → Lambo PHP | `lambo-gui.exe` opens on the Dashboard; no console window behind it | MANUAL WINDOWS TEST |
| 3 | First run | Read the Dashboard on a fresh install | Says no PHP/Apache/MariaDB yet, and how to get each - not a blank screen and not a crash | MANUAL WINDOWS TEST |
| 4 | Add project | `cd` into a project, `lambo init`; then Projects screen | The project is registered, listed, and selectable by clicking its row | MANUAL WINDOWS TEST |
| 5 | Select PHP | `lambo php install 8.4`; then PHP screen | Downloads, verifies the digest, and the PHP screen shows it **active** with the version PHP itself reported | MANUAL WINDOWS TEST |
| 6 | Start | Click **Start** | State shows **Starting…** while it works, then **Running** once the site answers. Not "Running" before it answers | MANUAL WINDOWS TEST |
| 7 | localhost | Open `http://localhost` | The project renders. Port 80, no `:8080` | MANUAL WINDOWS TEST |
| 8 | phpMyAdmin | Click **Open phpMyAdmin** | Opens `http://localhost/phpmyadmin` through Apache, prefilled, no password in the URL | MANUAL WINDOWS TEST |
| 9 | PHP script | Put `<?php phpinfo();` in the document root and reload | `phpinfo()` renders, showing Lambo's generated `php.ini` as the loaded file | MANUAL WINDOWS TEST |
| 10 | Database | `lambo db create demo`, then Database screen | The database is created; the screen shows the server, the manager and its URL | MANUAL WINDOWS TEST |
| 11 | Stop | Click **Stop** | **Stopping…**, then everything stops. `tasklist` shows no Lambo-owned processes left | MANUAL WINDOWS TEST |
| 12 | Restart | Start, stop, start again | Comes back cleanly; no stale PID complaint, no orphaned process | MANUAL WINDOWS TEST |
| 13 | Port conflict | Occupy port 80, then **Start** | State reads **Failed**, the notice names the service and the remedy. No Rust backtrace | MANUAL WINDOWS TEST |
| 14 | Reboot | Reboot Windows, then `lambo status` and launch the GUI | Nothing claims to be running; starting works normally | MANUAL WINDOWS TEST |
| 15 | Tray | - | **NOT IMPLEMENTED.** Lambo has no system tray icon and no start-with-Windows option. Do not test this as a feature; if you want it, it is a future request | N/A |
| 16 | Browser opening | Let Lambo open the browser for you (`lambo up`, **Open localhost**) | Your default browser opens the exact URL the Dashboard shows | MANUAL WINDOWS TEST |
| 17 | Upgrade | Install a newer `LamboPHP-Setup.exe` over the top | No UAC prompt, no "uninstall first"; projects, PHP installs, databases, config and logs all survive | MANUAL WINDOWS TEST |
| 18 | Uninstall | Run the uninstaller, then check `%USERPROFILE%\Lambo` | Application and Start Menu entries gone; **your data still present** | MANUAL WINDOWS TEST |
| 19 | Portable ZIP | Extract `LamboPHP-<version>-windows-amd64.zip` somewhere and run `lambo.exe` from it | Works with no install; contains `lambo.exe`, `lambo-gui.exe`, both licences and `README.txt` - and no test fixtures | MANUAL WINDOWS TEST |
| 20 | CLI | `lambo --version`, `lambo doctor`, `lambo status` in a terminal | Version matches the GUI's About screen; doctor and status agree with the Dashboard | MANUAL WINDOWS TEST |

Two notes on what this build can and cannot do:

- **Runtime downloads are fail-closed.** The shipped catalogue pins no
  `sha256` for PHP, Apache, MariaDB or phpMyAdmin, so `lambo php install` and
  `lambo db install-ui` will refuse until a digest is pinned in
  `<home>/config/catalogs/`. That is deliberate: Lambo never extracts or runs
  a download it cannot verify. Steps 5, 8 and 10 therefore need a pinned
  digest, or a runtime placed by hand. See
  [installation.md](installation.md).
- **No checksum was invented to make this easier.** Pinning one requires the
  real upstream artifact, which is a release step, not a guess.

If a step fails, open an issue with `lambo doctor` output and
`lambo logs apache -n 200`.

## Manual smoke test

CI runs the unit and integration suite on `windows-latest`, including the
platform, process, port, path, runtime-discovery and service-lifecycle tests,
and the full `up → HTTP 200 → status → down` lifecycle against a real child
process over a real TCP connection.

What CI *cannot* do is download real runtimes, start real Apache or MariaDB, or
open a real browser - and the Windows identity probe (`tasklist` parsing, used
to refuse to signal a reused PID) is only exercised against a real `tasklist`
here, never in CI. Run through this list once per release on a clean Windows 10
or 11 machine.

Nothing in this phase has been executed on physical Windows. The Windows code
paths are covered by compile-time checks (`cargo check --target
x86_64-pc-windows-msvc --all-targets`) and by tests that model Windows paths
and semantics on Linux; steps 6a, 6b and 22a below exist precisely because
those code paths have not yet run on the platform they are written for.

**Setup**

1. Sign in as a standard user - *not* an administrator. Every step below must
   work without elevation; if one does not, that is a bug.
2. Install from `LamboPHP-Setup.exe`. Confirm no UAC prompt appears.
3. Open a new `cmd` window and run `lambo --version`.

**Install and start**

| # | Command | Expect |
| --- | --- | --- |
| 4 | `lambo doctor` | Runs; reports missing PHP/Apache/MariaDB with the command that installs each |
| 5 | `lambo php install 8.4` | Downloads, verifies the checksum, extracts, activates |
| 6 | `lambo php -v` | Prints the version Lambo just installed |
| 6a | `lambo php -m`, `lambo php --ini` | Both reach PHP: the module list, and the generated `php.ini` path |
| 6b | `lambo php list` | A `VERSION / STATUS / SOURCE / PATH` table; the installed one reads `active` |
| 6c | `lambo php current` | `reported` equals `version`, `configuration` is the generated `php.ini`, `extensions` is non-zero - this is the check that PHP really ran on Windows |
| 7 | `lambo php ini` | Prints a path under `%USERPROFILE%\Lambo\php\…` |
| 8 | `lambo db install` | Downloads and installs MariaDB |
| 9 | `mkdir C:\code\demo && cd C:\code\demo` then `echo ^<?php phpinfo(); > index.php` | |
| 10 | `lambo init` | Writes `lambo.yml`; detection says plain PHP, `document_root: .` |
| 11 | `lambo up` | Every step ticks; ends with the URL |
| 12 | open `http://localhost` | `phpinfo()` renders |
| 13 | `lambo status` | Apache and the database show **running** with pids and uptime |

**Database and UI**

| # | Command | Expect |
| --- | --- | --- |
| 14 | `lambo db create demo` | Succeeds |
| 15 | `lambo db list` | Shows `demo` |
| 16 | `lambo db credentials` | Prints host/port/user; password hidden |
| 17 | `lambo db install-ui` then `lambo db open` | phpMyAdmin opens in the browser, prefilled, no password in the URL |
| 17a | open `http://localhost/phpmyadmin` while `lambo up` is running | The same manager, served through Apache on the project's own port - no second port to remember |
| 17b | `lambo down`, then `lambo db open` | Falls back to the standalone manager on loopback, and still opens |
| 18 | `lambo db shell demo` | An interactive prompt; `SELECT 1;` works; `exit` returns you |

**Shutdown**

| # | Command | Expect |
| --- | --- | --- |
| 19 | `lambo down` | Every service reports *stopped cleanly* |
| 20 | `lambo status` | Everything **not started** |
| 21 | `tasklist \| findstr /i "httpd mysqld mariadbd php"` | **No Lambo-owned processes remain** - this is the orphan check |
| 22 | Start a long-running unrelated process, run `lambo up` then `lambo down` | The unrelated process survives |
| 22a | `lambo up`, note a pid from `lambo status`, kill that service in Task Manager, start any unrelated program until it reuses the pid, then `lambo down` | Lambo reports the record as **stale** and signals nothing - this is the PID-reuse check |

**Failure paths** - these matter as much as the happy path.

| # | Scenario | Expect |
| --- | --- | --- |
| 23 | Occupy port 80 (`python -m http.server 80`), then `lambo up` | A port-conflict error naming port 80 and what to change; nothing left half-started |
| 24 | Corrupt `lambo.yml`, then `lambo up` | A parse error naming the line; no service started |
| 25 | Kill `httpd` in Task Manager, then `lambo status` | Reports **stopped**, not running |
| 26 | Pin a wrong checksum, then `lambo php install` | Refuses before extracting; the message says the checksum did not match |
| 26a | Rename `php.exe` inside an installed runtime, then `lambo php current` | Reports the runtime as unusable and names the missing executable - it does not claim a version it could not start |
| 26b | Move the Visual C++ redistributable out of reach (or use a PHP build needing one that is absent), then `lambo php current` | `reported  PHP did not run`, plus the loader's own message, and a non-zero exit |
| 27 | Disconnect the network, then `lambo php install 8.3` | A download error, not a hang and not a partial install |
| 28 | `lambo logs apache` after a failure | The log explains the failure |

**The GUI** - `lambo-gui.exe` is the same engine with a window on it, so every
assertion below is about whether the window agrees with the CLI, not about
whether it draws.

| # | Step | Expect |
| --- | --- | --- |
| 29 | Start Menu → Lambo PHP, or run `lambo-gui.exe` | The window opens with the Dashboard; no console window behind it |
| 30 | Read the Dashboard against `lambo status` in a terminal | Same project, same per-service states, same URL. Any disagreement is a bug |
| 31 | **About** | Shows the same version as `lambo --version`, plus the real home, data, config and log paths - all of which must exist on disk |
| 32 | **Open localhost** | The browser opens the URL the Dashboard shows, not a hard-coded one |
| 33 | **Open phpMyAdmin** | Opens `http://localhost/phpmyadmin`; enabled only when a manager is installed |
| 34 | Click **Stop**, then **Start** | Buttons enable and disable with the actual state; a second Start while running does nothing harmful |
| 35 | Kill `httpd` in Task Manager, wait for the next refresh | The row stops claiming it is running; the overall state reflects it |
| 36 | Make a start fail (occupy port 80 first), then **Start** | The state reads **Failed**, the notice names the service that failed, and the message is in English - **no Rust backtrace** |
| 37 | Every other screen | Projects, PHP, Services, Database, Logs and Settings each show real values, not placeholders |
| 38 | Close the window | Lambo's services keep running; the GUI is a view, not a supervisor |

**Upgrade** - an upgrade replaces the application and must not touch anything
the user created.

| # | Step | Expect |
| --- | --- | --- |
| 39 | Note your projects, PHP versions, databases, config and logs | Record `lambo php list`, `lambo db list`, `lambo config list` |
| 40 | Install a newer `LamboPHP-Setup.exe` over the existing install | No UAC prompt; no "uninstall first" |
| 41 | Re-run the three commands above | Identical output - projects, PHP installs, databases, config and logs all survived |
| 42 | `lambo up` in an existing project | Starts normally; no re-init, no lost `lambo.yml` |

**Hostile paths** - Windows users put spaces and non-ASCII in paths constantly.

| # | Step | Expect |
| --- | --- | --- |
| 43 | `mkdir "C:\My Projects\Ünïcödé demo"`, put an `index.php` there, `lambo init` then `lambo up` | Works; the document root is quoted correctly in the generated Apache config |
| 44 | `set LAMBO_HOME=C:\My Data\Lambo Home` then `lambo doctor` | Everything resolves under the spaced path; no truncated paths in errors |
| 45 | Run `lambo up` from a project on a network or substituted drive (`Z:\`) | Works, or fails with a message that names the real problem |

**Uninstall**

| # | Step | Expect |
| --- | --- | --- |
| 46 | Run the uninstaller | Binary, Start Menu entries and your `PATH` entry are gone |
| 47 | Check `%USERPROFILE%\Lambo` | Still present - data is never deleted by an uninstall |
| 48 | Confirm what the uninstaller offered | It distinguishes application files from your data, and does not offer to delete your data silently |

If any step fails, please open an issue with the output of `lambo doctor` and
`lambo logs apache -n 200`.
