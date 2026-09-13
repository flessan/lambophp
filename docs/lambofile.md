# `lambo.yml`

The project file. `lambo init` writes it, you own it, and it is meant to be
committed - it is what makes a teammate's environment match yours.

```yaml
# Lambo PHP project configuration
name: my-shop
php: ~8.3
server:
  kind: apache
  document_root: public
database:
  kind: mariadb
  name: my_shop
```

## Reference

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `name` | string | directory name | Slugified; must not be a reserved Windows name (`con`, `aux`, `nul`, …) |
| `php` | version spec | `php.default` from the global config | `stable`, `8.3`, `~8.3`, `8.4.2`, `^8.2` |
| `server.kind` | `apache` \| `php` | inherited | Omit to inherit; `nginx` is rejected, not ignored |
| `server.port` | 1–65535 | `server.port` from the global config | Omit to follow the machine default |
| `server.https_port` | 1–65535 | inherited | Reserved for TLS |
| `server.document_root` | relative path | `.` | Relative to the project; `..` is rejected |
| `database.kind` | `mariadb` \| `mysql` \| `none` | inherited | `none` disables provisioning |
| `database.port` | 1–65535 | inherited | |
| `database.name` | string | slugified `name` | Lowercase letters, digits, underscores; must start with a letter |
| `open_browser` | bool | `browser.open` | Per-project override |
| `extensions` | list | `[]` | PHP extensions this project needs |
| `env` | map | `{}` | Extra `.env` keys to write |

Unknown sections are ignored rather than rejected, so a newer Lambo can write
a file an older one still reads. A `server.http_port` written by king-PHP is
still understood.

## Inheritance

`server.kind` and `database.kind` are optional *on purpose*. Omitting them
means "whatever this machine is configured for", which is what lets the same
project file work on a Windows laptop (Apache + MariaDB) and a Linux CI runner
(PHP's built-in server, no database):

```yaml
name: api
php: ~8.3
server:
  document_root: public
# no server.kind, no database.kind - inherited
```

Writing `kind: none` is different from omitting it: `none` means "this project
has no database", and `lambo up` then skips provisioning entirely.

## Validation

`lambo up` validates the file before starting anything, and `lambo doctor`
reports it as a check. Rejected, with an explanation:

- an absolute or escaping `document_root`;
- a `port` of `0` or above `65535`;
- a project `name` that is a reserved Windows device name;
- a `database.name` that a MySQL identifier cannot be;
- `server.kind: nginx` - Lambo does not manage nginx, and pretending otherwise
  would leave you with a server that never starts;
- an `extensions` entry containing a path separator.

## Migration from `kingphp.yml`

`lambo init` in a directory that has a `kingphp.yml` converts it and writes
`lambo.yml` beside it. The original file is **left in place**, so nothing is
lost if you decide to go back. king's flatter schema (`server: apache`,
top-level `document_root`) is translated into the nested form above.

For a whole installation rather than one project, use
[`lambo migrate`](commands.md#lambo-migrate).

## Examples

**Plain PHP**

```yaml
name: sandbox
php: stable
server:
  document_root: .
database:
  kind: none
```

**WordPress**

```yaml
name: blog
php: ~8.2
server:
  document_root: .
database:
  kind: mariadb
  name: blog
env:
  WP_ENV: development
```

**Symfony, no database, fixed port**

```yaml
name: console-app
php: ^8.3
server:
  document_root: public
  port: 8090
database:
  kind: none
```
