# ADR-0007: Localhost-first ports (80/443) with automatic fallback

- **Status:** Accepted (supersedes ADR-0005)
- **Date:** 2026-09-13

## Context

ADR-0005 chose 8080/8443 so the default workflow never needed root. That
reasoning was sound for a CLI toolkit, and it is still true on Linux and macOS.

But the product is now a Windows-first desktop application whose promise is:

```text
Install Lambo → Open a project → Press Start → http://localhost
```

`http://localhost:8080` is not that promise. It asks the user to remember a port
that exists only because of an implementation constraint they did not choose, and
it makes every shared URL, every screenshot and every "just open localhost"
conversation carry a suffix.

Two platform facts decide how this resolves:

- **Windows has no privileged-port rule.** A standard user can bind 80. So on
  the primary target, port 80 costs nothing and needs no elevation.
- **Linux and macOS restrict ports below 1024** to root or
  `CAP_NET_BIND_SERVICE`. Demanding that would break the first run on a
  locked-down machine, which is exactly what ADR-0005 protected against.

## Decision

Default to **HTTP 80 / HTTPS 443**, and treat an unavailable web port as an
expected condition rather than a failure:

1. Attempt to bind the configured port.
2. If the OS refuses it (privileged port on Unix, excluded range on Windows) or
   another process holds it, move to the next free port - 80 → 8080, 443 → 8443.
3. State the change and the resulting URL. Never absorb it silently.

Only the **web** port may move. The database port is a contract: `.env` records
it and applications connect to it, so a conflict there remains a hard failure
with advice.

`port::resolve_listen_port` observes the real `bind` error rather than guessing
from the platform. "Port 80 must be a permissions problem on Unix" would be
wrong on Windows, and wrong on Unix too whenever something is genuinely
listening there.

## Consequences

- `http://localhost` works out of the box on Windows with no elevation.
- On Linux and macOS the first run still works with no elevation; the URL is
  `http://localhost:8080` and Lambo says why.
- Every command that displays or opens the project URL goes through
  `session::effective_url`, which prefers the port a running server actually
  bound. Reporting the configured port after a fallback would point the user at
  an address nothing is listening on - and `lambo open` would open it, making
  the failure look like the browser's fault.
- ADR-0005 is superseded, not repealed: its security reasoning still rules out
  a setuid helper or `setcap` on the managed `httpd`, and nothing here
  introduces one. The change is that a fallback is now automatic and visible
  rather than a configuration exercise.
