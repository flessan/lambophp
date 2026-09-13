# PHP runtimes

How Lambo installs, finds, validates, selects and runs PHP.

Lambo owns every PHP it installs. Nothing is added to your `PATH`, no service is
registered, no system file is edited, and no administrator privileges are
needed. `lambo php -v` runs the PHP Lambo resolved, with the `php.ini` Lambo
generated, and leaves the rest of the machine exactly as it found it.

---

## The install pipeline

`lambo php install 8.4.2` runs these steps in order, and stops at the first one
that fails:

| Step | What it does | What a failure means |
| --- | --- | --- |
| Resolve the source | Decide where the bytes come from: a local staged artifact, an override URL, a mirror, or the catalogue | No entry for this version and platform |
| Download | HTTPS (or `file://` for a staged artifact) | Network, or the mirror is unreachable |
| Verify the digest | SHA-256 against the digest the catalogue pinned, or the publisher's `.sha256` sidecar | The bytes are not what the catalogue claims |
| Extract | Into a staging directory, refusing entries that escape it | A malformed or hostile archive |
| Write the manifest | Version, platform, source, verified digest, file list | - |
| Promote | Rename staging into place, restoring the previous runtime if the rename fails | - |
| Generate `php.ini` | See [the generated configuration](#the-generated-configuration) | - |
| **Run it** | Start the binary once and read what it reports | The binary will not execute |
| Mark active | Point `.active` at this version | - |

Extraction happens in a staging directory and is only renamed into place at the
end, so a failed install never leaves a directory that looks installed. If the
runtime being replaced was healthy, a failure puts it back.

### Verification happens before execution, and both are required

A matching digest proves the bytes are the ones the catalogue described. It says
nothing about whether those bytes run. A Windows archive unpacked on Linux, a
build linked against a library this machine does not have, and an archive that
verified perfectly and contained the wrong program all pass every check up to
the last one.

So the install starts the binary before reporting success:

```text
error: PHP 8.4.3 failed to start: `<Lambo home>/php/8.4.3/bin/php` exited with code 127:
  error while loading shared libraries: libonig.so.5: cannot open shared object file
  possible causes:
    - the archive verified against its digest and unpacked cleanly
    - the installed binary could not be executed
  next: lambo doctor
```

The first cause is the useful one. The download was fine, so re-downloading it
is not the fix - and saying so stops you spending the evening on the wrong
problem.

Nothing is left behind. A runtime that will not run is removed and reported as
`available` again, not as `installed`.

This check only runs for the host platform. A runtime installed for another
platform is not expected to execute here, and reporting it as broken would be a
false claim about the install.

The same check runs again whenever a project is started. A runtime installed
before the gate existed, or one whose binary broke afterwards, still *resolves*
- the directory is there, the manifest is intact, the executable exists - so
`lambo up` starts the binary before it starts the server. Without that, `lambo
up` would see a port answer and report your project as serving while PHP could
not execute a single request.

---

## Where a runtime lives

```text
<Lambo home>/
├── php/
│   ├── .active              # the selected version, e.g. "8.4.2"
│   ├── 8.4.2/
│   │   ├── .lambo-install.json   # the manifest
│   │   ├── bin/php               # Linux and macOS
│   │   ├── etc/php.ini           # generated (Linux and macOS)
│   │   └── lib/php/extensions/…
│   └── 8.4.3/
│       ├── php.exe               # Windows
│       └── php.ini               # generated (Windows)
```

The layout differs by platform because PHP's own layout does. Windows keeps
`php.ini` beside the binary and its extensions in `ext/`; a Unix build keeps the
configuration under `etc/` and modules in a versioned directory below `lib/`.
Lambo resolves both, and `lambo php ini` prints the path for the platform you
are on.

The `<Lambo home>` location is configurable - see
[configuration.md](configuration.md). Nothing in a project's `lambo.yml` names
an absolute path, so a project file moves between machines and operating
systems.

---

## The generated configuration

`lambo php install` writes `php.ini`. You never edit it, and it is rewritten
whenever the runtime is installed or a project asks for extra extensions.

It sets `extension_dir` from the actual layout found on disk, points `error_log`
into Lambo's log directory, and enables whichever of these are present:

```text
curl  mbstring  openssl  fileinfo  mysqli  pdo_mysql  gd  zip  intl
```

Only extensions whose module file actually exists are enabled. A build without
`php_gd` does not start with a wall of warnings about a missing extension.

### How it reaches PHP

Lambo sets `PHPRC` on every process it starts, pointing at the directory holding
the generated `php.ini`. That is what makes a project's configuration win over a
machine-wide one, without Lambo editing anything outside its own directory.

`lambo doctor` verifies the promise rather than assuming it:

```text
✔ php: PHP 8.4.2 (/home/you/.lambo/php/8.4.2) - PHP 8.4.2 runs, 12 extension(s),
  configuration /home/you/.lambo/php/8.4.2/etc/php.ini
```

If PHP loaded something else - a `PHPRC` already in your environment, or a
system `php.ini` - doctor warns and names the file PHP actually used. Every
setting Lambo "wrote" would otherwise be inert while looking applied.

---

## Selecting a version

Three rules, in order:

1. a `php.version` pinned in the project's `lambo.yml`;
2. otherwise the active runtime (`.active`);
3. otherwise the newest installed one.

```bash
lambo php install 8.4.2   # download, verify, install, activate
lambo php use 8.3         # make an installed version active
lambo php current         # what would run right now
lambo php remove 8.3.10   # remove one
```

`lambo php use` errors when the version is not installed rather than silently
keeping the old one. `lambo php remove` refuses to delete a runtime a live Lambo
service is running from - deleting files under a running `httpd` does not stop
it, and then it cannot be restarted.

---

## Is it actually working?

`lambo php current` starts the runtime and reports what it said:

```text
  version        8.4.2
  path           /home/you/.lambo/php/8.4.2
  executable     /home/you/.lambo/php/8.4.2/bin/php
  reported       8.4.2
  configuration  /home/you/.lambo/php/8.4.2/etc/php.ini
  extensions     12
```

`version` is what Lambo installed. `reported` is what the binary said when Lambo
started it. Printing both is deliberate: a runtime that will not run shows up as
a gap between them instead of looking installed.

The same command, with a runtime that cannot start:

```text
  version        8.4.2
  path           /home/you/.lambo/php/8.4.2
  executable     /home/you/.lambo/php/8.4.2/bin/php
  reported       PHP did not run
  configuration  none loaded
⚠ `…/bin/php` exited with code 127: error while loading shared libraries:
  libonig.so.5: cannot open shared object file
  hint: `lambo doctor` explains what is wrong
```

Both exit non-zero. A runtime whose executable has gone missing entirely reads
`executable  not found` and says so, rather than putting a directory in a column
headed "executable".

`lambo php modules` reports the same way. It used to print "PHP reported no
extensions - it may be broken" for every failure, because an empty module list
was all it had to go on; it now says which of the three happened - would not
start, started and failed, or genuinely has no extensions.

### `corrupt` versus "will not run"

These are different failures and Lambo reports them differently.

`lambo php list` says **`corrupt`** when the directory disagrees with its
manifest: recorded files are missing, the directory was renamed, or the manifest
names another family. That check is about files, and it does not need to start
anything. See [troubleshooting.md](troubleshooting.md).

`lambo php current` and `lambo doctor` additionally **start** the runtime, which
is the only way to catch a runtime whose files are all present and which still
will not execute. A runtime can be `installed` in the list and unusable in
practice; that is why both checks exist.

---

## What the automated tests prove

Two different things, kept deliberately separate:

**The fixture suite** (`crates/lambo-core/tests/runtime_execution.rs`,
`crates/lambo-cli/tests/cli_php.rs`) installs a committed archive whose `php` is
a shell script implementing the command surface Lambo drives - `-v`, `--ini`,
`-m`, `-r`, `-S` and script paths - and nothing else. Unsupported invocations
fail loudly rather than passing. This proves Lambo's discovery, `PHPRC` wiring,
health checking, script execution and install gating are correct.

**The real-artifact test** (`crates/lambo-core/tests/real_artifacts.rs`) runs the
same pipeline against a genuine PHP archive when a developer stages one, and
asserts that real PHP reports the declared version, loads Lambo's generated
`php.ini`, and lists the always-compiled-in modules. With no artifact present it
says so and passes, so normal CI never depends on it.

A green fixture run is **not** verification against a real PHP build, and the
output never implies otherwise. See
`crates/lambo-core/tests/fixtures/real-artifacts/README.md` for how to stage an
artifact.

Windows runtime execution is not covered by CI: the fixture is a shell script.
It is a manual smoke test, written out in [windows.md](windows.md).
