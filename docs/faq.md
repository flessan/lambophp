# FAQ

## How is Lambo different from XAMPP / WAMP / Laragon?

XAMPP installs one fixed stack into a fixed location and you edit its
configuration files. Lambo installs *per-version* runtimes under one directory
it owns, generates `httpd.conf` and `php.ini` for you, and records what it
started so it can stop exactly that.

| | XAMPP / WAMP | Lambo |
| --- | --- | --- |
| PHP versions | one | as many as you install, one active |
| `httpd.conf` / `php.ini` | you edit them | generated; never edit them |
| Per-project configuration | none | `lambo.yml`, committed with the project |
| Ports | 80/443 (needs admin, collides with IIS) | 8080/8443 by default |
| Stopping | Control Panel | `lambo down`, which stops only what Lambo started |
| Diagnostics | logs, mostly | `lambo doctor`, every finding with its fix |

Laragon is the closest in spirit; Lambo is open source, cross-platform, and
driven from a committed project file.

## Why not Docker?

Docker virtualises; Lambo manages your OS natively. No containers, no VM, no
volume-performance cliff, no image to rebuild - the debugger, profiler and
filesystem behave exactly as they would in production-on-bare-metal, and your
project directory is just your project directory.

If your production is containers, keep using them. Lambo is for the local
loop.

## Does it need administrator rights?

No. The default HTTP port is 8080, the database binds to `127.0.0.1:3306`,
the Windows installer is per-user, and everything lives under
`%USERPROFILE%\Lambo` or `~/.lambo`. Lambo never registers a Windows service,
never edits `/etc/hosts`, and never asks for elevation.

## Where does everything live?

Under one directory - see
[configuration.md](configuration.md#the-home-directory). Set `LAMBO_HOME` to
move it, which is also how you make a portable install.

## Can I use my own Apache or MariaDB?

Yes. If `httpd` or `mysqld`/`mariadbd` is on `PATH` (or in the usual system
location), Lambo finds it and uses it instead of downloading one. That is the
recommended path on Linux and macOS, where the catalogue has no server builds.

## Can I use my own PHP?

Lambo manages the PHP it installs. A system PHP is detected by
[`lambo init`](commands.md#lambo-init) as evidence about the project, but
`lambo up` runs the version from `lambo.yml`, so the environment is
reproducible rather than "whatever this machine had".

## What is phpMyAdmin, and why is it the database UI?

The browser database manager most people arriving from XAMPP already know, and
the reason `http://localhost/phpmyadmin` is a URL that means something. Lambo
serves it with the PHP runtime it already manages - no Electron, no second
runtime, no separate web server.

When the project is running, Apache serves it through an alias at
`/phpmyadmin` on the project's own port. When it is not, `lambo db open` starts
it on loopback directly. Either way the connection is prefilled and the
password is never in the URL; `lambo db credentials --show-password` prints it.

Set `dbui.kind: adminer` if you would rather have the single-file manager.

## Does Lambo touch my `.env`?

Only to add the database keys your app needs (`DB_HOST`, `DB_PORT`, and
`DB_CONNECTION`/`DB_DATABASE`/`DB_USERNAME`/`DB_PASSWORD` for Laravel). A key
that already has a value is left alone, comments and ordering are preserved,
and the write is atomic. It seeds from `.env.example` when `.env` does not
exist.

## Does Lambo edit my `PATH`?

Only the Windows installer does, and only your user `PATH`, to add its own
directory. Lambo itself never modifies `PATH` - that is why `lambo php -v`
runs the managed PHP while `php -v` still runs whatever you had before.

## Can I run several projects at once?

Not on the same port. Each project's `lambo.yml` can pin its own
`server.port`, and `lambo workspace` groups projects for bookkeeping. Running
a whole workspace with one command is on the [roadmap](roadmap.md).

## What happened to `kingphp.yml`?

The project was renamed to Lambo PHP. `lambo init` converts a `kingphp.yml` it
finds, leaving the original in place, and `lambo migrate` moves a whole
`~/.king` installation - configuration, workspaces and every project file -
into the Lambo home. Nothing is deleted, and `KING_HOME` is never used to
locate the Lambo home.

## Is there a GUI?

Not yet. The engine is a library with no UI dependency, so a GUI would be a
rendering layer over the same API the CLI uses - which is exactly why the CLI
is forbidden from holding logic. See [architecture.md](architecture.md).

## Does Lambo phone home?

No. No telemetry, no update check, no analytics. The only network traffic is
the downloads you ask for, over HTTPS, verified against a checksum before
anything is extracted.

## How do I build from source?

[development.md](development.md). Rust 1.85+, no C dependencies.
