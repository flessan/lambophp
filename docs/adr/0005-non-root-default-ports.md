# ADR-0005: Unprivileged default ports (8080/8443)

- **Status:** Superseded by [ADR-0007](0007-localhost-first-ports.md)
- **Date:** 2026-07-30

> **Superseded.** The default is now HTTP 80 / HTTPS 443 so the URL is
> `http://localhost`, with an automatic, explained fallback to 8080/8443 when
> the port cannot be bound. The security reasoning below still holds and still
> rules out a setuid helper or `setcap` on the managed `httpd`; what changed is
> that a fallback is now automatic and visible rather than a configuration
> exercise. Windows has no privileged-port rule, so port 80 there needs no
> elevation at all.

## Context

Ports 80/443 are what users mentally expect ("`my-shop.test`, done"), but
on Linux binding below 1024 requires root or `CAP_NET_BIND_SERVICE`. A core
principle reads: *users should rarely need sudo after installation.*

## Decision

Default to **HTTP 8080 / HTTPS 8443**, with `server.http_port` /
`server.https_port` configurable (validated 1–65535). The M2 service
manager will document the optional, explicit, auditable paths to port 80
(authbind, setcap on the managed httpd, or a tiny setuid helper - chosen in
the M2 design RFC, never automatic).

## Rationale

- **The default workflow is 100% user-space.** First `lambo up` works on a
  locked-down corporate laptop.
- **No setuid binary by default** in something that also runs `composer
  install` on user projects - a dramatically better security posture
  (see SECURITY.md's threat model).
- **Tradeoff is a port in the URL** (`https://my-shop.test:8443`). We
  accept it for M2; if a *safe*, cross-distro mechanism proves itself, a
  future ADR can revisit - changing a default later is cheap, un-burning a
  security incident is not.

## Consequences

- Doctor checks treat `EACCES` on binding as a *warning with instructions*,
  not a failure.
- Documentation must always print URLs including the port when non-standard.
