//! What has to happen after an archive is unpacked.
//!
//! Installers are only half of an install. Apache ships with a configuration
//! file that points at the distribution's own directories and comments out the
//! modules it needs, PHP ships without a `php.ini` at all, and both are then
//! run from an installation root they have never seen. This module is the other
//! half: it rewrites what the archive shipped into what this installation
//! needs, and puts the website, the virtual-host includes and the runtime DLLs
//! where the services expect to find them.
//!
//! Ported from the post-install hooks of the original `catalog.go`. The
//! transformations are preserved in order and in effect - including the ones
//! that look redundant, such as uncommenting modules the shipped file already
//! has commented out under a slightly different name, because those are what
//! make the patch idempotent on a file that has already been patched.
//!
//! # Why both `<Directory>` and `<Location>` grants
//!
//! When Apache 2.4 services a `.php` request, the `Action` directive triggers
//! an internal sub-request to `/__lambo-php-bin__/php-cgi.exe`. That
//! sub-request is evaluated against URL space first (`<Location>`), *then*
//! filesystem space (`<Directory>`). The configuration Apache Lounge ships has
//! a global `<Directory />` with `Require all denied`, so without an explicit
//! `<Location>` grant the URL-space walk inherits that deny and the request
//! fails with `client denied by server configuration` before the
//! `<Directory ".../php">` grant is ever consulted. Both grants are therefore
//! required, and the comment is kept in the generated file so the next person
//! to read it does not delete one of them.
//!
//! # Brand migration
//!
//! The markers and the internal URL prefix are Lambo's own (`# >>> Lambo PHP
//! handler BEGIN` and `/__lambo-php-bin__/`). A configuration written by the
//! previous implementation contains the old ones; when a complete legacy block
//! is found it is removed and replaced by the current one, so an upgraded
//! installation ends up with exactly one handler block and one vhost include
//! rather than two competing aliases.
//!
//! # Differences that are deliberate
//!
//! * **The configuration is written atomically** ([`crate::fsx`]). The previous
//!   implementation truncated `httpd.conf` in place, so an interrupted install
//!   left Apache unable to start with a half-written configuration and no copy
//!   of the original.
//! * **Line-anchored matching is done line by line**, not with a regular
//!   expression engine (the dependency set is frozen). Every pattern here is
//!   anchored to a line start and cannot match across a line ending in any
//!   realistic configuration file; the one theoretical difference - the
//!   original's `\s+` could in principle consume a line ending - is noted where
//!   it applies.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fsx;
use crate::logs::LogFn;
use crate::process::combined_output;

/// Marks the start of the generated PHP handler block.
pub const PHP_HANDLER_BEGIN: &str =
    "# >>> Lambo PHP handler BEGIN — do not edit between these markers <<<";

/// Marks the end of the generated PHP handler block.
pub const PHP_HANDLER_END: &str = "# <<< Lambo PHP handler END >>>";

/// Marks the start of the generated virtual-host include.
pub const VHOST_INCLUDE_BEGIN: &str = "# >>> Lambo vhost include BEGIN <<<";

/// Marks the end of the generated virtual-host include.
pub const VHOST_INCLUDE_END: &str = "# <<< Lambo vhost include END >>>";

/// The URL prefix the PHP CGI binary is aliased under.
pub const PHP_HANDLER_URL_PREFIX: &str = "/__lambo-php-bin__/";

/// The marker the previous implementation wrote in front of its handler block.
pub const LEGACY_PHP_HANDLER_BEGIN: &str =
    "# >>> GoAMPP PHP handler BEGIN — do not edit between these markers <<<";

/// The marker the previous implementation wrote at the end of its block.
pub const LEGACY_PHP_HANDLER_END: &str = "# <<< GoAMPP PHP handler END >>>";

/// The marker the previous implementation wrote in front of its vhost include.
pub const LEGACY_VHOST_INCLUDE_BEGIN: &str = "# >>> GoAMPP vhost include BEGIN <<<";

/// The marker the previous implementation wrote at the end of its include.
pub const LEGACY_VHOST_INCLUDE_END: &str = "# <<< GoAMPP vhost include END >>>";

/// The generated PHP handler block, with `{php_bin}` and `{prefix}` to fill in.
const PHP_HANDLER_TEMPLATE: &str = "

# >>> Lambo PHP handler BEGIN — do not edit between these markers <<<
# Classic CGI handler: Apache runs php-cgi.exe per request, which is what the
# architecture of this stack has always been.
#
# Why both <Directory> AND <Location> grants:
# When Apache 2.4 services a .php request, the Action directive
# triggers an internal sub-request to {prefix}php-cgi.exe.
# That sub-request is evaluated against URL-space first (<Location>),
# THEN filesystem-space (<Directory>). The shipped httpd.conf has a
# global <Directory /> with \"Require all denied\" — without an explicit
# <Location> grant, the URL-space walk inherits that deny and the
# request 403s with: \"client denied by server configuration\"
# before our <Directory \"{php_bin}\"> grant is even consulted.
# Adding <Location \"{prefix}\"> with
# Require all granted bypasses the filesystem deny entirely for this
# specific URL prefix.
ScriptAlias \"{prefix}\" \"{php_bin}/\"
<Directory \"{php_bin}\">
    AllowOverride None
    Options +ExecCGI
    <Files \"php-cgi.exe\">
        Require all granted
    </Files>
    Require all granted
</Directory>
<Location \"{prefix}\">
    Require all granted
</Location>
AddHandler application/x-httpd-php .php
Action application/x-httpd-php \"{prefix}php-cgi.exe\"
# <<< Lambo PHP handler END >>>
";

/// The PHP handler block for an installation whose PHP binaries live in
/// `php_bin`.
pub fn php_handler_block(php_bin: &str) -> String {
    PHP_HANDLER_TEMPLATE
        .replace("{php_bin}", php_bin)
        .replace("{prefix}", PHP_HANDLER_URL_PREFIX)
}

/// The virtual-host include block for `vhosts_file`.
pub fn vhost_include_block(vhosts_file: &str) -> String {
    format!("\r\n{VHOST_INCLUDE_BEGIN}\r\nInclude \"{vhosts_file}\"\r\n{VHOST_INCLUDE_END}\r\n")
}

/// The installation root an install directory belongs to.
///
/// `bin/apache` and `bin/php` both sit directly under it, which is what lets
/// the generated paths point at the website and at PHP without being told
/// anything else.
pub fn base_dir_of(install_dir: &Path) -> PathBuf {
    let up = install_dir.parent().unwrap_or_else(|| Path::new("."));
    let up = up.parent().unwrap_or_else(|| Path::new("."));
    if up.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        up.to_path_buf()
    }
}

/// Rewrites a shipped `httpd.conf` into one that runs from this installation.
///
/// Returns the patched text, which is the input unchanged when there was
/// nothing to do - a second run over an already-patched file changes nothing
/// and appends nothing.
pub fn patch_httpd_conf(contents: &str, install_dir: &Path, base_dir: &Path) -> String {
    let srvroot = slashes(install_dir);
    let docroot = slashes(&base_dir.join("www"));
    let php_bin = slashes(&base_dir.join("bin").join("php"));

    let mut patched = contents.to_owned();

    patched = replace_srvroot(&patched, &srvroot);
    patched = map_lines(&patched, replace_server_name);
    patched = map_lines(&patched, |line| replace_document_root(line, &docroot));
    patched = map_lines(&patched, |line| replace_htdocs_directory(line, &docroot));
    patched = map_lines(&patched, replace_directory_index);
    patched = map_lines(&patched, |line| {
        uncomment_load_module(line, "cgi_module", "modules/mod_cgi.so")
    });
    patched = map_lines(&patched, |line| {
        uncomment_load_module(line, "actions_module", "modules/mod_actions.so")
    });
    patched = map_lines(&patched, |line| {
        uncomment_load_module(line, "proxy_module", "modules/mod_proxy.so")
    });
    patched = map_lines(&patched, |line| {
        uncomment_load_module(line, "proxy_http_module", "modules/mod_proxy_http.so")
    });

    // Only the first one, exactly as before: the shipped file has several
    // `AllowOverride None` lines, but the one that matters is the first - the
    // document root's - and rewriting them all would loosen the others.
    //
    // First in the *file*, not first per block: the original ran an unanchored
    // `strings.Replace(…, 1)` over the whole text, so running the patch a second
    // time on its own output rewrites the next `None` it finds. That is only
    // reachable by patching twice, and it is what the tests below pin.
    if let Some(at) = patched.find("AllowOverride None") {
        patched.replace_range(at..at + "AllowOverride None".len(), "AllowOverride All");
    }

    // A complete block from the previous implementation is replaced rather than
    // kept next to the new one: two ScriptAlias lines for the same prefix make
    // Apache refuse to start.
    patched = strip_block(&patched, LEGACY_PHP_HANDLER_BEGIN, LEGACY_PHP_HANDLER_END);

    if !patched.contains(PHP_HANDLER_BEGIN) && !patched.contains(LEGACY_PHP_HANDLER_BEGIN) {
        patched.push_str(&php_handler_block(&php_bin));
    }

    let vhosts_file = slashes(&base_dir.join("conf").join("apache").join("vhosts.conf"));
    patched = strip_block(
        &patched,
        LEGACY_VHOST_INCLUDE_BEGIN,
        LEGACY_VHOST_INCLUDE_END,
    );
    if !patched.contains(VHOST_INCLUDE_BEGIN) && !patched.contains(LEGACY_VHOST_INCLUDE_BEGIN) {
        patched.push_str(&vhost_include_block(&vhosts_file));
    }

    patched
}

/// Patches an installed Apache in place.
///
/// A configuration that cannot be read is not an error: a missing `httpd.conf`
/// means this is not an Apache installation, and the install has already
/// succeeded by the time this runs.
pub fn apply_httpd_conf(install_dir: &Path, log: &LogFn) -> Result<()> {
    let conf = install_dir.join("conf").join("httpd.conf");
    if let Ok(original) = fs::read_to_string(&conf) {
        let base_dir = base_dir_of(install_dir);
        let patched = patch_httpd_conf(&original, install_dir, &base_dir);
        if patched != original {
            log("  patched httpd.conf (SRVROOT, ServerName, DocumentRoot, PHP handler)");
            fsx::write_atomic(&conf, &patched)?;
        }
    }
    Ok(())
}

/// Prepares the document root, before anything is served from it.
///
/// Part of the Apache hook rather than of [`apply_httpd_conf`], because the
/// original created it even when `httpd.conf` was not there at all: an
/// installation whose configuration is missing still needs a `www/` for the
/// welcome page, and `ensureApacheRuntimeFiles` writes into it. The result is
/// ignored there, and here: a document root that cannot be created is reported
/// by Apache when it tries to start, not by an install that otherwise finished.
pub fn ensure_document_root(base_dir: &Path) {
    let _ = fsx::ensure_dir(&base_dir.join("www"));
}

/// A path with forward slashes, the only form Apache's generated configuration
/// can carry on every platform.
fn slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Applies `patch` to each line's content, keeping the line endings.
///
/// `patch` receives the content without its `\n` (a `\r` before it is part of
/// the content, because the line-anchored patterns that consume a whole line
/// consume the carriage return with it) and returns `None` to leave the line
/// alone.
fn map_lines(text: &str, mut patch: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut start = 0;
    while start < text.len() {
        let end = text[start..]
            .find('\n')
            .map_or(text.len(), |offset| start + offset);
        let content = &text[start..end];
        match patch(content) {
            Some(replacement) => out.push_str(&replacement),
            None => out.push_str(content),
        }
        if end < text.len() {
            out.push('\n');
        }
        start = end + 1;
    }
    out
}

/// Replaces every `Define SRVROOT "…"`, case-insensitively.
fn replace_srvroot(text: &str, srvroot: &str) -> String {
    const PREFIX: &str = "Define SRVROOT \"";
    let mut out = String::with_capacity(text.len());
    let mut index = 0;

    while index < text.len() {
        let matched = text
            .get(index..index + PREFIX.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(PREFIX));
        if matched {
            let after = index + PREFIX.len();
            // No closing quote means no match at all: the original's `"[^"]*"`
            // needs one, so this is not a line to rewrite.
            if let Some(offset) = text[after..].find('"') {
                out.push_str(&format!("Define SRVROOT \"{srvroot}\""));
                index = after + offset + 1;
                continue;
            }
        }
        let character = text[index..].chars().next().unwrap_or('\0');
        out.push(character);
        index += character.len_utf8();
    }

    out
}

/// Replaces a whole `#?ServerName …` line with `ServerName localhost:80`.
fn replace_server_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix('#').unwrap_or(line);
    let rest = rest.strip_prefix("ServerName")?;
    if !rest.starts_with(|character: char| character.is_whitespace()) {
        return None;
    }
    // `\s+[^\n]*` runs to the end of the line, carriage return included.
    Some("ServerName localhost:80".to_owned())
}

/// Replaces a `DocumentRoot "…"` prefix, keeping whatever follows it.
fn replace_document_root(line: &str, docroot: &str) -> Option<String> {
    let rest = line.strip_prefix("DocumentRoot")?;
    let rest = strip_whitespace(rest)?;
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(format!("DocumentRoot \"{docroot}\"{}", &rest[end + 1..]))
}

/// Rewrites `<Directory "…/htdocs">` to point at the new document root.
fn replace_htdocs_directory(line: &str, docroot: &str) -> Option<String> {
    let rest = line.strip_prefix("<Directory")?;
    let rest = strip_whitespace(rest)?;
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let directory = &rest[..end];
    let rest = &rest[end + 1..];
    let rest = strip_whitespace_zero(rest);
    let rest = rest.strip_prefix('>')?;
    if !directory.ends_with("/htdocs") {
        return None;
    }
    Some(format!("<Directory \"{docroot}\">{rest}"))
}

/// Rewrites an indented `DirectoryIndex …` line to serve `index.php` first.
fn replace_directory_index(line: &str) -> Option<String> {
    let indentation = &line[..line.len() - line.trim_start().len()];
    let rest = line[indentation.len()..].strip_prefix("DirectoryIndex")?;
    strip_whitespace(rest)?;
    // `[^\n]*` consumes the rest of the line, carriage return included.
    Some(format!("{indentation}DirectoryIndex index.php index.html"))
}

/// Uncomments a `#LoadModule` line for one module, keeping any trailing text.
fn uncomment_load_module(line: &str, module: &str, file: &str) -> Option<String> {
    let rest = line.strip_prefix('#')?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("LoadModule")?;
    let rest = strip_whitespace(rest)?;
    let rest = rest.strip_prefix(module)?;
    let rest = strip_whitespace(rest)?;
    let rest = rest.strip_prefix(file)?;
    Some(format!("LoadModule {module} {file}{rest}"))
}

/// Splits off a required run of leading whitespace.
///
/// The newline is not considered: these patterns are matched per line, so
/// whitespace here is always inside one line. (The original's `\s` could, in
/// principle, consume a line ending - a configuration file where a directive's
/// argument starts on the next line - which no shipped `httpd.conf` does.)
fn strip_whitespace(text: &str) -> Option<&str> {
    let rest = text.trim_start();
    if rest.len() == text.len() {
        None
    } else {
        Some(rest)
    }
}

/// Splits off an optional run of leading whitespace.
fn strip_whitespace_zero(text: &str) -> &str {
    text.trim_start()
}

/// Removes a complete marker-delimited block, markers and all.
///
/// Nothing is removed unless both markers are present: a half-written block is
/// not something to guess about, and removing the wrong region of a
/// configuration file is worse than leaving a stale one behind.
fn strip_block(text: &str, begin: &str, end: &str) -> String {
    let Some(from) = text.find(begin) else {
        return text.to_owned();
    };
    let Some(end_at) = text[from..].find(end) else {
        return text.to_owned();
    };
    let end_at = from + end_at + end.len();

    // Take the markers' whole lines with them, so no blank line is left where
    // the block used to be.
    let line_start = text[..from].rfind('\n').map_or(0, |index| index + 1);
    let line_end = text[end_at..]
        .find('\n')
        .map_or(text.len(), |offset| end_at + offset + 1);

    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..line_start]);
    out.push_str(&text[line_end..]);
    out
}

// ---------------------------------------------------------------------------
// The welcome page and the runtime files
// ---------------------------------------------------------------------------

/// The marker the welcome page carries so a later launch knows it may replace
/// it with a refreshed copy.
pub const WELCOME_MARKER: &str = "@lambo-welcome v6";

/// The marker the previous implementation wrote.
///
/// A page carrying it - and not the current marker - is refreshed, so an
/// installation upgraded in place ends up with the rebranded dashboard instead
/// of a stale link to a project that no longer exists. A page carrying neither
/// marker is the user's own work and is never touched.
pub const LEGACY_WELCOME_MARKER: &str = "@goampp-welcome";

/// The welcome page, as shipped.
const WELCOME_INDEX: &str = include_str!("../assets/welcome/index.php");

/// The `phpinfo()` shortcut, as shipped.
const WELCOME_PHPINFO: &str = include_str!("../assets/welcome/phpinfo.php");

/// The Visual C++ runtime DLLs the stack's binaries link against.
pub const RUNTIME_DLLS: [&str; 3] = ["vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll"];

/// The brand asset mirrored into `www/assets/`.
const WELCOME_BRAND_ASSET: &str = "lambo.png";

/// Puts the website where Apache serves it from, and the runtime DLLs beside
/// the PHP binaries that need them.
///
/// Every step is best effort and reports rather than fails: this runs from an
/// Apache post-install hook, and a welcome page that could not be written is
/// not a reason to report an otherwise complete install as failed.
///
/// The virtual-host include is seeded by [`crate::vhost`] - see
/// [`ensure_apache_runtime_files`].
pub fn ensure_welcome_page(base_dir: &Path, log: &LogFn) -> Result<()> {
    let www = base_dir.join("www");
    fsx::ensure_dir(&www)?;
    write_welcome_page(&www.join("index.php"), log);

    let phpinfo = www.join("phpinfo.php");
    if !phpinfo.exists() {
        match fsx::write_atomic(&phpinfo, WELCOME_PHPINFO) {
            Ok(()) => log(&format!(
                "  self-heal: wrote phpinfo shortcut → {}",
                phpinfo.display()
            )),
            Err(error) => log(&format!(
                "  self-heal: failed to write phpinfo.php: {error}"
            )),
        }
    }

    Ok(())
}

/// Writes the welcome page when it is missing or is a page this product owns.
fn write_welcome_page(path: &Path, log: &LogFn) {
    let existing = fs::read_to_string(path).ok();
    let write = match &existing {
        None => true,
        Some(text) => {
            // A page written by the previous implementation, and not already
            // refreshed. A page carrying the current marker is left alone
            // (even an old one), and so is a page with neither - the user's
            // own work is never overwritten.
            text.contains(LEGACY_WELCOME_MARKER) && !text.contains(WELCOME_MARKER)
        }
    };
    if !write {
        return;
    }

    match fsx::write_atomic(path, WELCOME_INDEX) {
        Ok(()) => log(&format!(
            "  self-heal: wrote welcome page → {}",
            path.display()
        )),
        Err(error) => log(&format!("  self-heal: failed to write index.php: {error}")),
    }
}

/// Mirrors the shipped brand asset and the service icons into the website.
pub fn ensure_welcome_assets(base_dir: &Path, log: &LogFn) {
    let assets = base_dir.join("www").join("assets");
    let icons = assets.join("icons");
    if fsx::ensure_dir(&icons).is_err() {
        return;
    }

    let logo = base_dir.join("logo.png");
    if logo.is_file() {
        let _ = fsx::copy_file_if_changed(&logo, &assets.join(WELCOME_BRAND_ASSET));
    }

    let Ok(entries) = fs::read_dir(base_dir.join("assets").join("icons")) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let _ = fsx::copy_file_if_changed(&entry.path(), &icons.join(entry.file_name()));
    }

    // Failure to mirror an icon is not worth a log line: the page falls back to
    // its `onerror` handler, and this runs on every launch. The original took a
    // logger here and never used it either.
    let _ = log;
}

/// Mirrors the Visual C++ runtime DLLs into every installed PHP.
///
/// PHP's Windows builds link against the MSVC runtime, and the PHP directory
/// is where the loader looks first, so the three DLLs are copied next to
/// `php-cgi.exe`. An installation without a `runtime/` directory of its own
/// (an older one, or a build that never carried them) is left alone.
pub fn ensure_php_runtime_dlls(base_dir: &Path, log: &LogFn) {
    let runtime = base_dir.join("runtime");
    if !runtime.is_dir() {
        return;
    }

    let bin = base_dir.join("bin");
    let Ok(entries) = fs::read_dir(&bin) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // Only PHP installations, and never a "-legacy" copy.
        if name != "php" && !name.starts_with("php-") {
            continue;
        }
        if name.ends_with("-legacy") {
            continue;
        }
        let directory = entry.path();
        if !directory.join("php-cgi.exe").exists() {
            continue;
        }
        for dll in RUNTIME_DLLS {
            if let Err(error) = fsx::copy_file_if_changed(&runtime.join(dll), &directory.join(dll))
            {
                log(&format!(
                    "  self-heal: failed to mirror {dll} into {name}: {error}"
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The hook dispatcher
// ---------------------------------------------------------------------------

/// What a post-install hook may need beyond its own install directory.
pub struct HookContext<'a> {
    /// The installation root (`bin/`, `www/`, `conf/`, `data/` live under it).
    pub base_dir: &'a Path,
    /// Where the hook narrates what it is doing.
    pub log: &'a LogFn,
    /// The transport for the one hook that downloads outside the cache.
    pub downloader: &'a dyn crate::download::Downloader,
    /// The download cache, for the hook that installs a prerequisite.
    pub cache: &'a mut crate::download_cache::DownloadCache,
}

/// Runs a component's post-install hook.
///
/// The install directory passed here is always the **canonical** one, even for
/// a versioned install, which is what the original did: `bin/php-8.3` is where
/// the files are, but `php.ini`, the data directories and the runtime mirroring
/// all belong to `bin/php`.
pub fn run(
    hook: crate::catalog_panel::Hook,
    install_dir: &Path,
    ctx: &mut HookContext<'_>,
) -> Result<()> {
    use crate::catalog_panel::Hook;

    match hook {
        Hook::ApacheHttpdConf => {
            apply_httpd_conf(install_dir, ctx.log)?;
            ensure_document_root(ctx.base_dir);
            ensure_apache_runtime_files(ctx.base_dir, ctx.log)
        }
        Hook::PhpIni => apply_php_ini(install_dir, ctx.log),
        Hook::MariaDbDataDir => initialise_mariadb(install_dir, ctx.log),
        Hook::PostgresCluster => initialise_postgres(install_dir, ctx.log),
        Hook::PhpMyAdminConfig => configure_phpmyadmin(install_dir, ctx.log),
        Hook::AdminerBlankPassword => neutralise_adminer_password_guard(install_dir, ctx.log),
        Hook::ComposerBat => write_composer_wrapper(install_dir, ctx.log),
        Hook::PythonPip => prepare_python(install_dir, ctx.log, ctx.downloader),
        Hook::RustToolchain => install_rust_toolchain(install_dir, ctx.log),
        Hook::RabbitMqErlang => prepare_rabbitmq(install_dir, ctx.log, ctx.cache),
        Hook::MinioDataDir => prepare_minio(install_dir, ctx.log),
    }
}

/// Seeds the virtual-host include and the website, and mirrors the runtime
/// DLLs.
///
/// The include is written by [`crate::vhost::write_apache_vhosts`], which owns
/// the file's contents.
pub fn ensure_apache_runtime_files(base_dir: &Path, log: &LogFn) -> Result<()> {
    let vhosts = base_dir.join("conf").join("apache").join("vhosts.conf");
    if !vhosts.exists() && fsx::ensure_dir(&base_dir.join("conf").join("apache")).is_ok() {
        match crate::vhost::write_apache_vhosts(&vhosts, base_dir, &[]) {
            Ok(()) => log(&format!("  self-heal: seeded {}", vhosts.display())),
            Err(error) => log(&format!(
                "  self-heal: failed to write vhosts.conf: {error}"
            )),
        }
    }

    ensure_welcome_page(base_dir, log)?;
    ensure_php_runtime_dlls(base_dir, log);
    ensure_welcome_assets(base_dir, log);
    Ok(())
}

// ---------------------------------------------------------------------------
// php.ini
// ---------------------------------------------------------------------------

/// The extensions the previous implementation enabled.
///
/// Order is preserved because the patch is applied in this order; the log line
/// still says "8 extensions", which was true of an earlier version of the list
/// and is preserved verbatim rather than quietly corrected.
pub const PHP_EXTENSIONS: [&str; 22] = [
    "bz2",
    "curl",
    "exif",
    "fileinfo",
    "ftp",
    "gd",
    "gettext",
    "gmp",
    "mbstring",
    "openssl",
    "zip",
    "intl",
    "mysqli",
    "pdo_mysql",
    "pdo_pgsql",
    "pgsql",
    "pdo_sqlite",
    "sqlite3",
    "soap",
    "sockets",
    "xsl",
    "sodium",
];

/// Creates and patches `php.ini`, then mirrors the runtime DLLs.
///
/// A PHP archive ships `php.ini-development` and no `php.ini`; the copy is what
/// makes the runtime usable, and the patch points PHP's upload, session and
/// extension directories at this installation. An installation with neither
/// file is not a PHP installation and is left alone.
pub fn apply_php_ini(install_dir: &Path, log: &LogFn) -> Result<()> {
    let ini = install_dir.join("php.ini");
    let contents = match fs::read_to_string(&ini) {
        Ok(existing) => existing,
        Err(_) => {
            let development = install_dir.join("php.ini-development");
            let Ok(template) = fs::read_to_string(&development) else {
                return Ok(());
            };
            log("  created php.ini from php.ini-development");
            template
        }
    };

    let base_dir = base_dir_of(install_dir);
    let tmp_dir = slashes(&base_dir.join("tmp"));
    fsx::ensure_dir(&base_dir.join("tmp"))?;

    let patched = patch_php_ini(&contents, &tmp_dir);
    fsx::write_atomic(&ini, &patched)?;
    log("  patched php.ini (extension_dir, session.save_path, 8 extensions)");

    // The DLLs go in next to the binary the hook was run for, which for a
    // versioned PHP is the version's own directory.
    ensure_php_runtime_dlls(&base_dir, log);
    Ok(())
}

/// Applies the `php.ini` patch. Pure, so it can be tested against a real
/// `php.ini-development`.
///
/// The order matters: the first rule comments out `extension_dir = "./"`, the
/// second uncomments `extension_dir = "ext"`. The shipped `php.ini-development`
/// carries the second, commented out, so the pair is what turns it on; a
/// configuration that points at `"./"` is normalised to the commented form
/// first, which is what leaves the second rule as the effective one.
pub fn patch_php_ini(contents: &str, tmp_dir: &str) -> String {
    let mut patched = contents.to_owned();

    patched = map_lines(&patched, |line| {
        let rest = directive(line, "extension_dir")?;
        // The original's pattern is `^;?\s*extension_dir\s*=\s*"\./"`: the dot
        // is escaped, so this is the literal `"./"` and not "any one
        // character". Only the match is replaced, so anything the line carries
        // after it survives.
        let tail = rest.strip_prefix("\"./\"")?;
        Some(format!(";extension_dir = \"./\"{tail}"))
    });

    patched = map_lines(&patched, |line| {
        let rest = directive(line, "extension_dir")?;
        let tail = rest.strip_prefix("\"ext\"")?;
        Some(format!("extension_dir = \"ext\"{tail}"))
    });

    for (name, replacement) in [
        ("upload_tmp_dir", format!("upload_tmp_dir = \"{tmp_dir}\"")),
        (
            "session.save_path",
            format!("session.save_path = \"{tmp_dir}\""),
        ),
    ] {
        patched = map_lines(&patched, |line| {
            directive(line, name)?;
            Some(replacement.clone())
        });
    }

    for (name, replacement) in [
        ("cgi.force_redirect", "cgi.force_redirect = 0"),
        // The missing spaces around `=` are the original's, and are kept.
        ("cgi.fix_pathinfo", "cgi.fix_pathinfo=1"),
        ("display_errors", "display_errors = Off"),
        ("display_startup_errors", "display_startup_errors = Off"),
    ] {
        patched = map_lines(&patched, |line| {
            directive(line, name)?;
            Some(replacement.to_owned())
        });
    }

    for extension in PHP_EXTENSIONS {
        patched = map_lines(&patched, |line| {
            commented_extension(line, extension).then(|| format!("extension={extension}"))
        });
    }

    patched
}

/// Splits a directive line, returning what follows its `=`.
///
/// `<?` matches the original's `^;?\s*name\s*=\s*`: an optional leading
/// semicolon, then the name, then `=`. Whitespace is taken as horizontal -
/// these are line-anchored patterns, so there is no line ending left to
/// consume.
fn directive<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(';').unwrap_or(line);
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(name)?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?;
    Some(rest.trim_start())
}

/// Whether a line is a commented-out `extension=<name>` directive.
///
/// The original's `\b` is why `zip` does not match `zip2`: the character after
/// the name must not be a word character.
fn commented_extension(line: &str, name: &str) -> bool {
    let Some(rest) = line.strip_prefix(";extension") else {
        return false;
    };
    let rest = rest.trim_start();
    let Some(rest) = rest.strip_prefix('=') else {
        return false;
    };
    let rest = rest.trim_start();
    let Some(rest) = rest.strip_prefix(name) else {
        return false;
    };
    rest.chars()
        .next()
        .is_none_or(|next| !(next.is_ascii_alphanumeric() || next == '_'))
}

// ---------------------------------------------------------------------------
// MariaDB and PostgreSQL
// ---------------------------------------------------------------------------

/// Initialises MariaDB's data directory.
///
/// A directory that was left behind by a failed attempt is wiped first: MariaDB
/// refuses to initialise into a non-empty one, and a half-written `data/` is
/// exactly the state the earlier implementation used to fail on forever. The
/// wipe only happens when `data/mysql` is absent, so an initialised cluster is
/// never touched.
pub fn initialise_mariadb(install_dir: &Path, log: &LogFn) -> Result<()> {
    let data_dir = install_dir.join("data");
    if data_dir.join("mysql").exists() {
        log("  data dir already initialized, skipping");
        return Ok(());
    }

    let populated = fs::read_dir(&data_dir)
        .map(|entries| entries.count() > 0)
        .unwrap_or(false);
    if populated {
        log("  wiping stale data/ from earlier broken install");
        fs::remove_dir_all(&data_dir).map_err(|source| Error::io(&data_dir, source))?;
        fsx::ensure_dir(&data_dir)?;
    }

    // Both spellings, because MariaDB renamed the tool.
    let candidates = [
        install_dir.join("bin").join("mariadb-install-db.exe"),
        install_dir.join("bin").join("mysql_install_db.exe"),
    ];
    let Some(init) = candidates.iter().find(|path| path.is_file()) else {
        return Err(Error::InvalidInput(
            "mariadb-install-db.exe not found".to_owned(),
        ));
    };

    log("  initializing MariaDB data directory (this can take ~30s)...");
    let spec = crate::process::ProcessSpec::new(init, "mariadb-install-db")
        .arg(format!("--datadir={}", data_dir.display()))
        .cwd(install_dir.join("bin"));
    let output = crate::process::run(&spec, crate::platform::Os::host())?;
    if !output.status.success() {
        log(&format!(
            "  install-db output: {}",
            combined_output(&output)
        ));
        return Err(helper_failed("mariadb-install-db", &output));
    }
    Ok(())
}

/// Initialises a PostgreSQL cluster.
///
/// `trust` authentication, the `C` locale and the `postgres` password are the
/// original's choices and are what every connection string on the welcome page
/// assumes. The password is passed through a temporary file because `initdb`
/// has no environment-variable form for it.
pub fn initialise_postgres(install_dir: &Path, log: &LogFn) -> Result<()> {
    let data_dir = install_dir.join("data");
    if data_dir.join("PG_VERSION").exists() {
        log("  cluster already initialized, skipping");
        return Ok(());
    }

    let init = install_dir.join("bin").join("initdb.exe");
    if !init.is_file() {
        return Err(Error::InvalidInput("initdb.exe not found".to_owned()));
    }
    let bin = install_dir.join("bin");

    // The Windows builds link against the MSVC runtime, which the archive does
    // not carry; this installation ships it in `runtime/` for exactly this.
    let runtime = base_dir_of(install_dir).join("runtime");
    if runtime.is_dir() {
        for dll in RUNTIME_DLLS {
            let _ = fsx::copy_file(&runtime.join(dll), &bin.join(dll));
        }
        log("  seeded VC++ 14.44 runtime into pgsql/bin/");
    }

    let password_file = std::env::temp_dir().join("lambo-pg-pw.txt");
    fs::write(&password_file, "postgres").map_err(|source| Error::io(&password_file, source))?;
    fsx::restrict_to_owner(&password_file);

    log(
        "  initializing PostgreSQL cluster (user=postgres, password=postgres, locale=C, auth=trust)...",
    );
    let spec = crate::process::ProcessSpec::new(&init, "initdb")
        .arg("-D")
        .arg(data_dir.display().to_string())
        .arg("-U")
        .arg("postgres")
        .arg(format!("--pwfile={}", password_file.display()))
        .arg("-E")
        .arg("SQL_ASCII")
        .arg("-A")
        .arg("trust")
        .arg("--locale=C")
        .arg("--no-instructions");
    let output = crate::process::run(&spec, crate::platform::Os::host());
    let _ = fs::remove_file(&password_file);

    let output = output?;
    if !output.status.success() {
        log(&format!("  initdb output: {}", combined_output(&output)));
        return Err(helper_failed("initdb", &output));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// phpMyAdmin, Adminer and Composer
// ---------------------------------------------------------------------------

/// Creates `config.inc.php` and patches it for a local development stack.
///
/// Two changes: a generated blowfish secret, because phpMyAdmin refuses to
/// start without one and the shipped sample's is empty; and `AllowNoPassword`,
/// because the local database's root account has no password. An installation
/// with neither file - a phpMyAdmin that shipped preconfigured - is left alone.
pub fn configure_phpmyadmin(install_dir: &Path, log: &LogFn) -> Result<()> {
    let target = install_dir.join("config.inc.php");
    let contents = match fs::read_to_string(&target) {
        Ok(existing) => existing,
        Err(_) => {
            let sample = install_dir.join("config.sample.inc.php");
            let Ok(template) = fs::read_to_string(&sample) else {
                return Ok(());
            };
            log("  created config.inc.php from sample");
            template
        }
    };

    let secret = crate::secret::token(32);
    let mut patched = set_blowfish_secret(&contents, &secret);
    patched = set_allow_no_password(&patched);

    log("  patched config.inc.php (blowfish_secret, AllowNoPassword)");
    fsx::write_atomic(&target, &patched)
}

/// Replaces the `blowfish_secret` assignment.
///
/// Only a line that already assigns it is rewritten: a configuration without
/// one gets no new line, exactly as before.
pub fn set_blowfish_secret(contents: &str, secret: &str) -> String {
    map_lines(contents, |line| {
        let rest = line.strip_prefix("$cfg['blowfish_secret']")?;
        let rest = rest.trim_start();
        let rest = rest.strip_prefix('=')?;
        let rest = rest.trim_start();
        let rest = rest.strip_prefix('\'')?;
        // `'[^']*'` - an empty secret matches too, which is the case that
        // matters: the shipped sample has none.
        let end = rest.find('\'')?;
        let rest = &rest[end + 1..];
        // The original's pattern ended `;.*$`, so it consumed the semicolon and
        // everything after it: any trailing comment on the line is dropped
        // along with a trailing carriage return.
        rest.strip_prefix(';')?;
        Some(format!(
            "$cfg['blowfish_secret'] = '{secret}'; /* auto-generated by Lambo PHP */"
        ))
    })
}

/// Turns `AllowNoPassword` on.
///
/// Matches either spelling of the value, because the sample has it commented
/// out with `false` and a user may have set it to either.
pub fn set_allow_no_password(contents: &str) -> String {
    map_lines(contents, |line| {
        let rest = line.strip_prefix("$cfg['Servers'][$i]['AllowNoPassword']")?;
        let rest = rest.trim_start();
        let rest = rest.strip_prefix('=')?;
        let rest = rest.trim_start();
        // `(?:true|false);.*$` - the semicolon is required and the rest of the
        // line goes with it.
        rest.strip_prefix("true;")
            .or_else(|| rest.strip_prefix("false;"))?;
        Some(
            "$cfg['Servers'][$i]['AllowNoPassword'] = true; /* local dev — empty root password is fine */"
                .to_owned(),
        )
    })
}

/// Neutralises Adminer's empty-password guard.
///
/// Adminer refuses to log in with an empty password unless the guard is
/// disabled, which is what the single-file build normally needs an edit for.
/// When the pattern is not found the file is left exactly as it is and the
/// reason is logged: a newer Adminer may have reworded it, and patching a file
/// that does not match would be guesswork.
pub fn neutralise_adminer_password_guard(install_dir: &Path, log: &LogFn) -> Result<()> {
    let target = install_dir.join("index.php");
    let contents = fs::read_to_string(&target)
        .map_err(|source| Error::InvalidInput(format!("read adminer: {source}")))?;

    let Some(patched) = disable_password_guard(&contents) else {
        log("  Adminer: empty-password guard pattern not found (already patched?)");
        return Ok(());
    };
    if patched == contents {
        log("  Adminer: empty-password guard pattern not found (already patched?)");
        return Ok(());
    }

    fsx::write_atomic(&target, &patched)?;
    log("  patched Adminer to allow blank-password local dev logins");
    Ok(())
}

/// Rewrites Adminer's `if($x=="")return sprintf('Adminer does not support …`
/// condition to a constant `false`.
///
/// The original matched this with a regular expression that could span a line
/// ending; the scan below does the same, so an Adminer that wraps the
/// condition across lines is still patched - and one that does not match at all
/// is left untouched.
pub fn disable_password_guard(contents: &str) -> Option<String> {
    const CONDITION: &str = "if($";
    const BETWEEN: &str = "==\"\")return";
    const TAIL: &str = "sprintf('Adminer does not support";
    const REPLACEMENT: &str = "if(false)return ";

    // The original replaced *every* occurrence, so a file carrying the guard
    // more than once comes out with all of them neutralised. Each failed
    // candidate start is copied through untouched.
    let mut patched = String::with_capacity(contents.len());
    let mut from = 0;
    let mut found = false;

    while let Some(offset) = contents[from..].find(CONDITION) {
        let start = from + offset;
        patched.push_str(&contents[from..start]);
        let after_condition = start + CONDITION.len();

        // `\w+`, which must not be empty: `if($==""` is not the guard.
        let words = contents[after_condition..]
            .bytes()
            .take_while(|byte| is_word_byte(*byte))
            .count();
        if words == 0 {
            patched.push_str(CONDITION);
            from = after_condition;
            continue;
        }

        let rest = &contents[after_condition + words..];
        let Some(rest) = rest.strip_prefix(BETWEEN) else {
            patched.push_str(&contents[start..after_condition + words]);
            from = after_condition + words;
            continue;
        };

        // `\s+`, which the original allowed to run past a line ending.
        let whitespace = rest.len() - rest.trim_start().len();
        if whitespace == 0 {
            patched.push_str(&contents[start..after_condition + words + BETWEEN.len()]);
            from = after_condition + words + BETWEEN.len();
            continue;
        }

        let rest = &rest[whitespace..];
        if !rest.starts_with(TAIL) {
            let consumed = after_condition + words + BETWEEN.len() + whitespace;
            patched.push_str(&contents[start..consumed]);
            from = consumed;
            continue;
        }

        // The whole match - condition, `return` and the whitespace between -
        // is replaced, which is what turns `return` into part of a statement
        // that is never reached. Everything after it is left as it was.
        patched.push_str(REPLACEMENT);
        patched.push_str(TAIL);
        from = contents.len() - (rest.len() - TAIL.len());
        found = true;
    }

    patched.push_str(&contents[from..]);
    found.then_some(patched)
}

/// Whether a byte is part of a word, as the original's `\w` meant it.
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Writes the `composer.bat` wrapper next to `composer.phar`.
///
/// The phar is not executable on Windows, so the wrapper - which resolves its
/// own directory and forwards every argument - is what makes `composer` work
/// from a terminal. An existing wrapper is never overwritten: a user may have
/// edited it.
pub fn write_composer_wrapper(install_dir: &Path, log: &LogFn) -> Result<()> {
    let wrapper = install_dir.join("composer.bat");
    if wrapper.exists() {
        return Ok(());
    }
    fsx::write_atomic(&wrapper, "@echo off\r\nphp \"%~dp0composer.phar\" %*\r\n")?;
    log("  created composer.bat wrapper");
    Ok(())
}

// ---------------------------------------------------------------------------
// Python and Rust
// ---------------------------------------------------------------------------

/// Where pip's bootstrap script is fetched from.
pub const GET_PIP_URL: &str = "https://bootstrap.pypa.io/get-pip.py";

/// Makes an embeddable Python usable: `import site`, then pip.
///
/// The embeddable build ships a `python<version>._pth` file that disables site
/// imports, which is what stops `pip` from working at all; uncommenting it is
/// the documented fix. pip itself is bootstrapped from a downloaded script, and
/// a failure to download it is **not** fatal - Python is installed and usable
/// either way, and the reason is logged.
pub fn prepare_python(
    install_dir: &Path,
    log: &LogFn,
    downloader: &dyn crate::download::Downloader,
) -> Result<()> {
    for path in pth_files(install_dir) {
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        if !contents.contains("#import site") {
            continue;
        }
        let patched = contents.replace("#import site", "import site");
        log(&format!(
            "  uncommented import site in {}",
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        let _ = fsx::write_atomic(&path, &patched);
    }

    if install_dir.join("Scripts").join("pip.exe").exists() {
        log("  pip already installed");
        return Ok(());
    }

    let script = install_dir.join("get-pip.py");
    log("  downloading get-pip.py from bootstrap.pypa.io");
    // Through the caller's transport, not a free function: the installer hands
    // every hook the same downloader, and a test can hand it one that refuses.
    if let Err(error) = downloader.fetch_with_progress(GET_PIP_URL, &script, &|_, _| {}) {
        log(&format!("  get-pip download failed (non-fatal): {error}"));
        return Ok(());
    }

    log("  bootstrapping pip via python.exe get-pip.py ...");
    // The script comes first and the options follow it, as the original had
    // them.
    let spec = crate::process::ProcessSpec::new(install_dir.join("python.exe"), "get-pip")
        .arg(script.display().to_string())
        .args(["--no-warn-script-location", "--quiet"])
        .cwd(install_dir);
    let output = crate::process::run(&spec, crate::platform::Os::host())?;
    if !output.status.success() {
        log(&format!(
            "  pip bootstrap output: {}",
            combined_output(&output)
        ));
        return Err(helper_failed("get-pip.py", &output));
    }
    log("  pip ready");
    Ok(())
}

/// The `python*._pth` files in an installation.
///
/// The original globbed `python*._pth`; a directory listing is used here
/// because a glob implementation would be a new dependency, and the pattern is
/// a prefix and a suffix.
fn pth_files(install_dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = fs::read_dir(install_dir) else {
        return Vec::new();
    };
    let mut found: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .is_some_and(|name| name.starts_with("python") && name.ends_with("._pth"))
        })
        .collect();
    // A deterministic order, so a log line reads the same way every time.
    found.sort();
    found
}

/// Installs the Rust toolchain into the component's own directories.
///
/// `rustup-init.exe` is a single file that bootstraps everything else. It is
/// pointed at `.rustup` and `.cargo` inside the component rather than the
/// user's profile - with `--no-modify-path`, so an installation never rewrites
/// a PATH the user did not ask it to.
pub fn install_rust_toolchain(install_dir: &Path, log: &LogFn) -> Result<()> {
    let init = install_dir.join("rustup-init.exe");
    if !init.is_file() {
        return Err(Error::InvalidInput(format!(
            "rustup-init.exe not found in {}",
            install_dir.display()
        )));
    }

    log("  installing Rust stable via rustup (3–5 min first time)...");
    let spec = crate::process::ProcessSpec::new(&init, "rustup-init")
        .args([
            "-y",
            "--no-modify-path",
            "--default-toolchain",
            "stable",
            "--profile",
            "default",
        ])
        .cwd(install_dir)
        .env(
            "RUSTUP_HOME",
            install_dir.join(".rustup").display().to_string(),
        )
        .env(
            "CARGO_HOME",
            install_dir.join(".cargo").display().to_string(),
        );
    let output = crate::process::run(&spec, crate::platform::Os::host())?;
    if !output.status.success() {
        log(&format!("  rustup output: {}", combined_output(&output)));
        return Err(helper_failed("rustup-init", &output));
    }

    log("  Rust stable ready — cargo at bin/rust/.cargo/bin/cargo.exe");
    Ok(())
}

// ---------------------------------------------------------------------------
// RabbitMQ, Erlang and MinIO
// ---------------------------------------------------------------------------

/// The Erlang installer RabbitMQ needs, and its version.
pub const ERLANG_VERSION: &str = "27.3.4";

/// The Erlang installer download.
pub const ERLANG_URL: &str =
    "https://github.com/erlang/otp/releases/download/OTP-27.3.4/otp_win64_27.3.4.exe";

/// The Erlang installer's file name in the download cache.
pub const ERLANG_FILE_NAME: &str = "otp_win64_27.3.4.exe";

/// Prepares RabbitMQ: Erlang, a data directory, and the management plugin.
///
/// RabbitMQ is a Windows service-shaped application: its Erlang dependency is a
/// silent installer, its state lives in `data/rabbitmq`, and its management UI
/// is a plugin that has to be enabled once. Failing to enable the plugin is not
/// fatal - the broker itself works without it, and the reason is logged.
pub fn prepare_rabbitmq(
    install_dir: &Path,
    log: &LogFn,
    cache: &mut crate::download_cache::DownloadCache,
) -> Result<()> {
    let base_dir = base_dir_of(install_dir);
    let erlang_dir = base_dir.join("bin").join("erlang");

    if !erlang_dir.join("bin").join("erl.exe").is_file() {
        log("  Erlang not found — downloading OTP 27.3.4...");
        let installer = cache
            .fetch(
                ERLANG_FILE_NAME,
                ERLANG_URL,
                &crate::download::nop_progress(),
            )
            .map_err(|error| Error::InvalidInput(format!("erlang download: {error}")))?;

        log("  installing Erlang OTP silently...");
        fsx::ensure_dir(&erlang_dir)
            .map_err(|error| Error::InvalidInput(format!("erlang dir: {error}")))?;
        let target = absolute(&erlang_dir);
        let spec = crate::process::ProcessSpec::new(&installer, "erlang-installer")
            .arg("/S")
            .arg(format!("/D={}", target.display()));
        let output = crate::process::run(&spec, crate::platform::Os::host())?;
        if !output.status.success() {
            log(&format!("  erlang installer: {}", combined_output(&output)));
            return Err(helper_failed("erlang installer", &output));
        }
        log("  Erlang OTP installed");
    }

    let data_dir = base_dir.join("data").join("rabbitmq");
    fsx::ensure_dir(&data_dir)?;
    log("  created data/rabbitmq/ — RabbitMQ stores state here");

    let plugins = install_dir.join("sbin").join("rabbitmq-plugins.bat");
    if !plugins.is_file() {
        return Ok(());
    }

    // The batch file needs `cmd.exe` to run, which is the one place this
    // product starts a command interpreter - the arguments are fixed strings
    // and the paths come from the configuration, never from input.
    let spec = crate::process::ProcessSpec::new("cmd.exe", "rabbitmq-plugins")
        .arg("/c")
        .arg(plugins.display().to_string())
        .arg("enable")
        .arg("rabbitmq_management")
        .env("ERLANG_HOME", absolute(&erlang_dir).display().to_string())
        .env("RABBITMQ_BASE", absolute(&data_dir).display().to_string());
    let output = crate::process::run(&spec, crate::platform::Os::host())?;
    if output.status.success() {
        log("  enabled rabbitmq_management plugin");
    } else {
        log(&format!("  rabbitmq-plugins: {}", combined_output(&output)));
    }
    Ok(())
}

/// Creates the directory MinIO keeps its objects in.
pub fn prepare_minio(install_dir: &Path, log: &LogFn) -> Result<()> {
    let data_dir = base_dir_of(install_dir).join("data").join("minio");
    fsx::ensure_dir(&data_dir)?;
    log("  created data/minio/ — MinIO will store objects here");
    Ok(())
}

// ---------------------------------------------------------------------------
// Running a helper
// ---------------------------------------------------------------------------

/// The error reported when a hook's helper exits with a failure.
fn helper_failed(what: &str, output: &std::process::Output) -> Error {
    Error::ProcessExited {
        name: what.to_owned(),
        code: output.status.code(),
    }
}

/// An absolute form of a path, falling back to the path itself.
///
/// Both silent installers are given an absolute directory (`/D=<dir>`), because
/// they run with their own working directory expectations and a relative path
/// would install somewhere unpredictable.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// The shape Apache Lounge ships: CRLF endings, indented blocks, and the
    /// modules PHP needs commented out.
    ///
    /// `concat!` rather than `\`-continued lines, because a backslash at the end
    /// of a string line *strips the next line's leading whitespace*: the
    /// indentation this fixture is about would not survive.
    const SHIPPED: &str = concat!(
        "Define SRVROOT \"c:/Apache24\"\r\n",
        "ServerRoot \"${SRVROOT}\"\r\n",
        "Listen 80\r\n",
        "#ServerName www.example.com:80\r\n",
        "DocumentRoot \"c:/Apache24/htdocs\"\r\n",
        "<Directory \"c:/Apache24/htdocs\">\r\n",
        "    AllowOverride None\r\n",
        "    Require all granted\r\n",
        "</Directory>\r\n",
        "<Directory \"c:/Apache24/cgi-bin\">\r\n",
        "    AllowOverride None\r\n",
        "    Options None\r\n",
        "</Directory>\r\n",
        "#LoadModule cgi_module modules/mod_cgi.so\r\n",
        "# LoadModule actions_module modules/mod_actions.so\r\n",
        "#LoadModule proxy_module modules/mod_proxy.so\r\n",
        "#LoadModule proxy_http_module modules/mod_proxy_http.so\r\n",
        "    DirectoryIndex index.html\r\n",
    );

    fn patch(contents: &str) -> String {
        patch_httpd_conf(
            contents,
            Path::new("/stack/bin/apache"),
            Path::new("/stack"),
        )
    }

    #[test]
    fn the_shipped_configuration_is_rewritten_for_this_installation() {
        let patched = patch(SHIPPED);

        // The installation root, in the forward-slash form Apache needs.
        assert!(patched.contains("Define SRVROOT \"/stack/bin/apache\"\r\n"));
        // The document root points at the stack's own website.
        assert!(patched.contains("DocumentRoot \"/stack/www\"\r\n"));
        assert!(patched.contains("<Directory \"/stack/www\">\r\n"));
        // The second directory block is untouched: only the htdocs one moves.
        assert!(patched.contains("<Directory \"c:/Apache24/cgi-bin\">\r\n"));
        // PHP is served before the static index, and the first AllowOverride is
        // the one that opens up. `    AllowOverride None` becomes `All` and is
        // never looked at again, so the patch stays idempotent.
        assert!(patched.contains("    DirectoryIndex index.php index.html\n"));
        assert!(patched.contains("AllowOverride All\r\n    Require all granted"));
        assert!(patched.contains("    AllowOverride None\r\n    Options None"));
        assert_eq!(patched.matches("AllowOverride All").count(), 1);
        // ServerName is written even though the shipped line is commented out.
        assert!(patched.contains("ServerName localhost:80\n"));
        // The four modules Apache needs for PHP are uncommented, with the
        // leading whitespace the file happened to use preserved.
        for module in [
            "LoadModule cgi_module modules/mod_cgi.so",
            "LoadModule actions_module modules/mod_actions.so",
            "LoadModule proxy_module modules/mod_proxy.so",
            "LoadModule proxy_http_module modules/mod_proxy_http.so",
        ] {
            assert!(
                patched.contains(module),
                "`{module}` must be present: {patched}"
            );
            assert!(!patched.contains(&format!("# {module}")));
            assert!(!patched.contains(&format!("#{module}")));
        }
    }

    #[test]
    fn the_handler_block_and_the_vhost_include_are_appended_once() {
        let patched = patch(SHIPPED);

        assert!(patched.contains(PHP_HANDLER_BEGIN));
        assert!(patched.contains(PHP_HANDLER_END));
        assert!(patched.contains("ScriptAlias \"/__lambo-php-bin__/\" \"/stack/bin/php/\""));
        assert!(patched.contains("<Directory \"/stack/bin/php\">"));
        assert!(patched.contains("<Location \"/__lambo-php-bin__/\">"));
        assert!(patched.contains("AddHandler application/x-httpd-php .php"));
        assert!(
            patched.contains("Action application/x-httpd-php \"/__lambo-php-bin__/php-cgi.exe\"")
        );
        assert!(patched.contains("Include \"/stack/conf/apache/vhosts.conf\""));
        assert_eq!(patched.matches("ScriptAlias").count(), 1);
        assert_eq!(patched.matches(PHP_HANDLER_BEGIN).count(), 1);
        assert_eq!(patched.matches(VHOST_INCLUDE_BEGIN).count(), 1);
    }

    #[test]
    fn patching_again_moves_the_next_allow_override_and_then_settles() {
        // A second run over the patch's own output finds the first `None` gone
        // and rewrites the next one - the cgi-bin block's. That is what an
        // unanchored replace-one does, and it is only reachable by patching
        // twice; a third run has no `None` left and changes nothing.
        let once = patch(SHIPPED);
        assert_eq!(once.matches("AllowOverride All").count(), 1);
        // The cgi-bin block's, which was not the one rewritten, and the one
        // inside the handler block that has just been appended.
        assert_eq!(once.matches("AllowOverride None").count(), 2);

        let twice = patch(&once);
        assert_eq!(twice.matches("AllowOverride All").count(), 2);
        assert_eq!(twice.matches("AllowOverride None").count(), 1);

        // The third run rewrites the appended block's own line - the last
        // `None` there is - and after that the file stops changing.
        let thrice = patch(&twice);
        assert_eq!(thrice.matches("AllowOverride None").count(), 0);
        assert_eq!(patch(&thrice), thrice, "the fixed point is stable");
    }

    #[test]
    fn a_line_the_patch_does_not_touch_keeps_its_carriage_return() {
        let patched = patch(SHIPPED);

        // Everything the patch leaves alone keeps the shipped CRLF endings, and
        // the blocks it appends keep their own: the handler block is LF like the
        // previous implementation's template, and the include that follows it is
        // CRLF.
        assert!(patched.contains("    Require all granted\r\n"));
        assert!(patched.contains("# <<< Lambo PHP handler END >>>\n"));
        assert!(patched.ends_with(&format!("{VHOST_INCLUDE_END}\r\n")));
    }

    #[test]
    fn the_previous_implementations_block_is_replaced_not_duplicated() {
        let legacy = SHIPPED.to_owned()
            + "\n# >>> GoAMPP PHP handler BEGIN — do not edit between these markers <<<\n\
               # Legacy body.\n\
               ScriptAlias \"/__goampp-php-bin__/\" \"c:/goampp/bin/php/\"\n\
               Action application/x-httpd-php \"/__goampp-php-bin__/php-cgi.exe\"\n\
               # <<< GoAMPP PHP handler END >>>\n\
               \r\n# >>> GoAMPP vhost include BEGIN <<<\r\n\
               Include \"c:/goampp/conf/apache/vhosts.conf\"\r\n\
               # <<< GoAMPP vhost include END >>>\r\n";

        let patched = patch(&legacy);

        assert!(
            !patched.contains("GoAMPP"),
            "no trace of the legacy block may remain: {patched}"
        );
        assert!(!patched.contains("/__goampp-php-bin__/"));
        assert_eq!(patched.matches("ScriptAlias").count(), 1);
        assert_eq!(patched.matches("Include \"").count(), 1);
        assert_eq!(patched.matches(PHP_HANDLER_BEGIN).count(), 1);
        assert_eq!(patched.matches(VHOST_INCLUDE_BEGIN).count(), 1);
    }

    #[test]
    fn a_truncated_legacy_block_is_left_alone_and_not_doubled() {
        // A BEGIN with no END is not something to guess about: the block stays
        // where it is, and no second handler is appended on top of it.
        let truncated = SHIPPED.to_owned()
            + "\n# >>> GoAMPP PHP handler BEGIN — do not edit between these markers <<<\n\
               ScriptAlias \"/__goampp-php-bin__/\" \"c:/goampp/bin/php/\"\n";

        let patched = patch(&truncated);
        assert!(patched.contains(LEGACY_PHP_HANDLER_BEGIN));
        assert!(!patched.contains(PHP_HANDLER_BEGIN));
        assert_eq!(patched.matches("ScriptAlias").count(), 1);
        // The vhost include is independent and is still added.
        assert_eq!(patched.matches(VHOST_INCLUDE_BEGIN).count(), 1);
    }

    #[test]
    fn srvroot_is_replaced_case_insensitively_everywhere() {
        let text = "define srvroot \"c:/one\"\r\nDEFINE SRVROOT \"c:/two\"\r\n";
        let patched = replace_srvroot(text, "/stack/bin/apache");
        assert_eq!(
            patched,
            "Define SRVROOT \"/stack/bin/apache\"\r\nDefine SRVROOT \"/stack/bin/apache\"\r\n"
        );
    }

    #[test]
    fn a_srvroot_line_without_a_closing_quote_is_not_a_match() {
        let text = "Define SRVROOT \"c:/unterminated\n";
        assert_eq!(replace_srvroot(text, "/stack"), text);
    }

    #[test]
    fn text_after_a_replaced_directive_is_preserved() {
        let patched = patch(
            "DocumentRoot \"c:/Apache24/htdocs\" # keep me\r\n\
             <Directory \"c:/Apache24/htdocs\"> # and me\r\n",
        );
        assert!(patched.contains("DocumentRoot \"/stack/www\" # keep me\r\n"));
        assert!(patched.contains("<Directory \"/stack/www\"> # and me\r\n"));
    }

    #[test]
    fn directives_that_do_not_match_are_left_alone() {
        let untouched = "DocumentRoot c:/Apache24/htdocs\r\n\
                         DocumentRootUnset \"x\"\r\n\
                         #ServerNames www.example.com\r\n\
                         <Directory \"c:/Apache24/icons\">\r\n\
                         DirectoryIndexing on\r\n\
                         #LoadModule cgi_module modules/mod_cgi.SO\r\n";
        let patched = patch(untouched);
        assert!(
            patched.starts_with(untouched),
            "every line must be left exactly as it was: {patched}"
        );
    }

    #[test]
    fn base_dir_is_two_levels_up_with_a_fallback() {
        assert_eq!(
            base_dir_of(Path::new("/stack/bin/apache")),
            PathBuf::from("/stack")
        );
        assert_eq!(base_dir_of(Path::new("apache")), PathBuf::from("."));
        assert_eq!(
            base_dir_of(Path::new("C:/stack/bin/apache")),
            PathBuf::from("C:/stack")
        );
    }

    #[test]
    fn windows_style_paths_are_written_with_forward_slashes() {
        let patched = patch_httpd_conf(
            "Define SRVROOT \"c:/Apache24\"\r\n",
            Path::new(r"C:\stack\bin\apache"),
            Path::new(r"C:\stack"),
        );
        assert!(patched.contains("Define SRVROOT \"C:/stack/bin/apache\""));
        assert!(patched.contains("ScriptAlias \"/__lambo-php-bin__/\" \"C:/stack/bin/php/\""));
    }

    #[test]
    fn apply_httpd_conf_writes_the_patch() {
        let temp = TempDir::new();
        let install_dir = temp.join("bin").join("apache");
        let conf_dir = install_dir.join("conf");
        fs::create_dir_all(&conf_dir).expect("failed to create the configuration directory");
        fs::write(conf_dir.join("httpd.conf"), SHIPPED).expect("failed to write the fixture");

        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&lines);
        let log: LogFn = std::sync::Arc::new(move |line: &str| {
            sink.lock().expect("log lock").push(line.to_owned());
        });

        apply_httpd_conf(&install_dir, &log).expect("the patch must succeed");

        let written =
            fs::read_to_string(conf_dir.join("httpd.conf")).expect("failed to read the result");
        assert!(written.contains(PHP_HANDLER_BEGIN));
        assert_eq!(
            lines.lock().expect("log lock").clone(),
            vec![
                "  patched httpd.conf (SRVROOT, ServerName, DocumentRoot, PHP handler)".to_owned()
            ]
        );
    }

    #[test]
    fn apply_httpd_conf_tolerates_a_missing_configuration() {
        let temp = TempDir::new();
        let install_dir = temp.join("bin").join("apache");
        fs::create_dir_all(&install_dir).expect("failed to create the installation directory");
        let log = crate::logs::nop_log();

        apply_httpd_conf(&install_dir, &log).expect("a missing configuration is not an error");
        assert!(
            !temp.join("www").exists(),
            "the patch alone writes nothing but the configuration"
        );

        // The document root is the hook's business, not the patch's: the
        // original created it even when there was no configuration to patch.
        ensure_document_root(temp.path());
        assert!(temp.join("www").is_dir());
    }
}

/// The post-install hooks other than the `httpd.conf` patch.
///
/// These are the ten remaining entries of the original's `PostInstall` table,
/// tested against the files they actually rewrite: a real excerpt of
/// `php.ini-development`, a real `config.sample.inc.php`, a real Adminer
/// source file. Where a hook only shells out to a tool that is not present on
/// the test machine, what is asserted is the check that runs *before* the
/// process does - a missing `initdb.exe` must be an error, not a silent
/// success - plus the diagnostics the hook produces on the way there.
#[cfg(test)]
mod hook_tests {
    use std::fs;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::catalog_panel::Hook;
    use crate::download_cache::DownloadCache;
    use crate::testutil::{TempDir, fixture};

    /// A log sink that records everything it is given.
    fn recorder() -> (LogFn, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: LogFn = Arc::new(move |line: &str| {
            sink.lock().expect("log lock").push(line.to_owned());
        });
        (log, lines)
    }

    /// The lines a recorder captured.
    fn lines(recorded: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        recorded.lock().expect("log lock").clone()
    }

    /// A downloader that refuses: every hook under test here reaches the network
    /// only on a path the test deliberately does not take.
    struct Refusing;

    /// The one refusing transport, so a `HookContext` can borrow it for as
    /// long as it lives.
    static REFUSING: Refusing = Refusing;

    impl crate::download::Downloader for Refusing {
        fn fetch(&self, url: &str, _destination: &Path) -> Result<()> {
            Err(crate::error::Error::Download {
                url: url.to_owned(),
                reason: "no network in tests".to_owned(),
            })
        }
    }

    /// A `HookContext` over `base_dir`, with a cache rooted inside it.
    fn context<'a>(
        base_dir: &'a Path,
        log: &'a LogFn,
        cache: &'a mut DownloadCache,
    ) -> HookContext<'a> {
        HookContext {
            base_dir,
            log,
            downloader: &REFUSING,
            cache,
        }
    }

    /// The lines of `php.ini-development`, as PHP ships it: the directives the
    /// patch is expected to act on, plus the near misses it must leave alone.
    const PHP_INI: &str = "\
;extension_dir = \"ext\"\n\
;extension_dir = \"./\"\n\
extension_dir = \".\" ; relative, and not the rule's business\n\
;upload_tmp_dir =\n\
;session.save_path = \"/tmp\"\n\
;cgi.force_redirect = 1\n\
;cgi.fix_pathinfo=1\n\
display_errors = On\n\
display_startup_errors = On\n\
;extension=curl\n\
;extension=zip2\n\
;extension=mcrypt   ; removed from PHP years ago\n\
;extension=sodium\n\
extension=pdo_mysql\n\
";

    #[test]
    fn php_ini_is_patched_the_way_the_original_patched_it() {
        let patched = patch_php_ini(PHP_INI, "C:/lambo/tmp");

        // The shipped `extension_dir = "ext"` is turned on, and a `"./"`
        // configuration is commented out first. The escaped dot in the
        // original's pattern is why `"."` - one character, no slash - is not
        // touched at all.
        assert!(patched.contains("extension_dir = \"ext\"\n"));
        assert!(patched.contains(";extension_dir = \"./\"\n"));
        assert!(!patched.contains("\nextension_dir = \"./\"\n"));
        assert!(
            patched.contains("\nextension_dir = \".\" ; relative, and not the rule's business\n")
        );

        // Both temporary directories point at this installation's own `tmp/`.
        assert!(patched.contains("\nupload_tmp_dir = \"C:/lambo/tmp\"\n"));
        assert!(patched.contains("\nsession.save_path = \"C:/lambo/tmp\"\n"));

        // The four switches, with the missing spaces around `=` kept exactly
        // as the original wrote them.
        assert!(patched.contains("\ncgi.force_redirect = 0\n"));
        assert!(patched.contains("\ncgi.fix_pathinfo=1\n"));
        assert!(patched.contains("\ndisplay_errors = Off\n"));
        assert!(patched.contains("\ndisplay_startup_errors = Off\n"));

        // Extensions the product enables are uncommented...
        for extension in ["curl", "sodium"] {
            assert!(
                patched.contains(&format!("\nextension={extension}\n")),
                "{extension}"
            );
        }
        // ...the ones it does not are left commented...
        assert!(patched.contains("\n;extension=mcrypt   ; removed from PHP years ago\n"));
        // ...and `zip2` is not `zip`: the original's `\b` is why.
        assert!(patched.contains("\n;extension=zip2\n"));
    }

    #[test]
    fn php_ini_patching_twice_changes_nothing() {
        let once = patch_php_ini(PHP_INI, "C:/lambo/tmp");
        let twice = patch_php_ini(&once, "C:/lambo/tmp");
        assert_eq!(once, twice);
    }

    #[test]
    fn php_ini_line_endings_follow_the_original() {
        // A whole-line pattern ended `.*$`, which consumed the carriage
        // return with the rest of the line; the two `extension_dir` rules
        // replace only what they matched, so theirs survives.
        let source = "session.save_path = \"x\"\r\n;extension_dir = \"ext\"\r\n;extension=curl\r\n";
        let patched = patch_php_ini(source, "C:/tmp");
        assert!(
            patched.contains("session.save_path = \"C:/tmp\""),
            "{patched:?}"
        );
        assert!(!patched.contains("\"C:/tmp\"\r"), "{patched:?}");
        assert!(
            patched.contains("extension_dir = \"ext\"\r\n"),
            "{patched:?}"
        );
        assert!(patched.contains("\nextension=curl\n"), "{patched:?}");
    }

    #[test]
    fn apply_php_ini_creates_the_configuration_from_the_shipped_template() {
        let temp = TempDir::new();
        let (log, recorded) = recorder();
        fixture(temp.path(), "bin/php/php.ini-development", PHP_INI);

        apply_php_ini(&temp.join("bin/php"), &log).expect("the php.ini patch runs");

        let ini = fs::read_to_string(temp.join("bin/php/php.ini")).expect("php.ini exists");
        assert!(ini.contains("extension_dir = \"ext\"\n"));
        assert!(ini.contains(&format!(
            "session.save_path = \"{}\"",
            temp.join("tmp").to_string_lossy().replace('\\', "/")
        )));
        assert!(
            temp.join("tmp").is_dir(),
            "the temporary directory is created"
        );
        let recorded = lines(&recorded);
        assert!(
            recorded
                .iter()
                .any(|line| line == "  created php.ini from php.ini-development")
        );
        assert!(recorded.iter().any(
            |line| line == "  patched php.ini (extension_dir, session.save_path, 8 extensions)"
        ));
    }

    #[test]
    fn apply_php_ini_leaves_an_installation_without_a_template_alone() {
        let temp = TempDir::new();
        let (log, recorded) = recorder();
        apply_php_ini(&temp.join("bin/php"), &log).expect("nothing to do is not a failure");
        assert!(!temp.join("bin/php/php.ini").exists());
        assert!(lines(&recorded).is_empty());
    }

    #[test]
    fn the_welcome_page_is_written_for_a_missing_or_legacy_page_only() {
        let temp = TempDir::new();
        let www = temp.join("www");
        let (log, recorded) = recorder();

        // Missing: written.
        ensure_welcome_page(temp.path(), &log).expect("the page is written");
        let page = fs::read_to_string(www.join("index.php")).expect("index.php exists");
        assert!(page.contains(WELCOME_MARKER));
        assert!(www.join("phpinfo.php").is_file());

        // Current marker: left exactly as it is, even edited.
        fs::write(www.join("index.php"), "<?php // mine @lambo-welcome v6").expect("rewrite");
        ensure_welcome_page(temp.path(), &log).expect("nothing to do");
        assert_eq!(
            fs::read_to_string(www.join("index.php")).expect("index.php exists"),
            "<?php // mine @lambo-welcome v6"
        );

        // Legacy marker and no current one: rewritten, which is the upgrade
        // path from the previous implementation.
        fs::write(www.join("index.php"), "<?php // @goampp-welcome v5").expect("rewrite");
        ensure_welcome_page(temp.path(), &log).expect("the page is refreshed");
        let page = fs::read_to_string(www.join("index.php")).expect("index.php exists");
        assert!(page.contains(WELCOME_MARKER));
        assert!(!page.contains(LEGACY_WELCOME_MARKER));

        // Neither marker: the user's own page is never overwritten.
        fs::write(www.join("index.php"), "<?php // my own site").expect("rewrite");
        ensure_welcome_page(temp.path(), &log).expect("nothing to do");
        assert_eq!(
            fs::read_to_string(www.join("index.php")).expect("index.php exists"),
            "<?php // my own site"
        );

        // A page that is both - legacy first, current later - counts as ours
        // already and is left alone: the current marker is never a reason to
        // rewrite.
        fs::write(
            www.join("index.php"),
            format!("{LEGACY_WELCOME_MARKER} then {WELCOME_MARKER}"),
        )
        .expect("rewrite");
        ensure_welcome_page(temp.path(), &log).expect("nothing to do");
        assert!(
            fs::read_to_string(www.join("index.php"))
                .expect("index.php exists")
                .starts_with(LEGACY_WELCOME_MARKER)
        );

        // The phpinfo shortcut is only written when it is absent.
        fs::write(www.join("phpinfo.php"), "<?php // mine too").expect("rewrite");
        ensure_welcome_page(temp.path(), &log).expect("nothing to do");
        assert_eq!(
            fs::read_to_string(www.join("phpinfo.php")).expect("phpinfo.php exists"),
            "<?php // mine too"
        );

        let recorded = lines(&recorded);
        let writes = recorded
            .iter()
            .filter(|line| line.starts_with("  self-heal: wrote welcome page"))
            .count();
        assert_eq!(
            writes, 2,
            "the missing page and the legacy one: {recorded:?}"
        );
    }

    #[test]
    fn the_runtime_dlls_reach_every_php_but_the_legacy_copy() {
        let temp = TempDir::new();
        let (log, recorded) = recorder();
        for dll in RUNTIME_DLLS {
            fixture(temp.path(), &format!("runtime/{dll}"), "dll");
        }
        fixture(temp.path(), "bin/php/php-cgi.exe", "MZ");
        fixture(temp.path(), "bin/php-8.3/php-cgi.exe", "MZ");
        fixture(temp.path(), "bin/php-legacy/php-cgi.exe", "MZ");
        // A directory that is not PHP at all, and one that is a PHP copy
        // without the CGI binary - neither is touched.
        fixture(temp.path(), "bin/nginx/nginx.exe", "MZ");
        fixture(temp.path(), "bin/php-9.9/php.exe", "MZ");

        ensure_php_runtime_dlls(temp.path(), &log);

        for dll in RUNTIME_DLLS {
            assert!(temp.join("bin/php").join(dll).is_file(), "{dll} in bin/php");
            assert!(
                temp.join("bin/php-8.3").join(dll).is_file(),
                "{dll} in bin/php-8.3"
            );
            assert!(
                !temp.join("bin/php-legacy").join(dll).exists(),
                "{dll} skipped in legacy"
            );
            assert!(!temp.join("bin/nginx").join(dll).exists());
            assert!(!temp.join("bin/php-9.9").join(dll).exists());
        }
        assert!(
            lines(&recorded).is_empty(),
            "mirroring is silent when it works"
        );
    }

    #[test]
    fn runtime_dlls_are_skipped_when_the_runtime_directory_is_absent() {
        let temp = TempDir::new();
        let (log, _) = recorder();
        fixture(temp.path(), "bin/php/php-cgi.exe", "MZ");
        ensure_php_runtime_dlls(temp.path(), &log);
        assert!(!temp.join("bin/php").join(RUNTIME_DLLS[0]).exists());
    }

    #[test]
    fn the_vhost_include_and_the_welcome_page_are_seeded_for_apache() {
        let temp = TempDir::new();
        let (log, recorded) = recorder();
        ensure_apache_runtime_files(temp.path(), &log).expect("the runtime files are seeded");

        let vhosts = temp.join("conf/apache/vhosts.conf");
        let text = fs::read_to_string(&vhosts).expect("the include is written");
        assert!(text.contains(crate::vhost::APACHE_MARKER_BEGIN));
        assert!(temp.join("www/index.php").is_file());
        assert!(
            lines(&recorded)
                .iter()
                .any(|line| line.starts_with("  self-heal: seeded "))
        );

        // A file that is already there is never replaced.
        fs::write(&vhosts, "# mine\n").expect("rewrite");
        ensure_apache_runtime_files(temp.path(), &log).expect("nothing to do");
        assert_eq!(fs::read_to_string(&vhosts).expect("readable"), "# mine\n");
    }

    #[test]
    fn the_blowfish_secret_and_allow_no_password_follow_the_originals_lines() {
        let sample = "\
$cfg['blowfish_secret'] = ''; /* YOU MUST FILL IN THIS FOR COOKIE AUTH! */\n\
$cfg['Servers'][$i]['AllowNoPassword'] = false;\n\
$cfg['Servers'][$i]['auth_type'] = 'cookie';\n\
$cfg['blowfish_secret'] = 'keep me';\n";

        let patched = set_blowfish_secret(sample, "SECRET");
        // `;.*$` consumed the trailing comment - it is gone in the original
        // too, not moved.
        assert!(
            patched.contains(
                "$cfg['blowfish_secret'] = 'SECRET'; /* auto-generated by Lambo PHP */\n"
            )
        );
        assert!(!patched.contains("YOU MUST FILL IN THIS FOR COOKIE AUTH"));
        // Every assignment is replaced, and the unrelated configuration is not.
        assert!(!patched.contains("'keep me'"));
        assert!(patched.contains("$cfg['Servers'][$i]['auth_type'] = 'cookie';\n"));

        // A configuration without the assignment gets no new line.
        let patched =
            set_blowfish_secret("$cfg['Servers'][$i]['auth_type'] = 'cookie';\n", "SECRET");
        assert!(!patched.contains("SECRET"));

        let patched = set_allow_no_password(sample);
        assert!(patched.contains(
            "$cfg['Servers'][$i]['AllowNoPassword'] = true; /* local dev — empty root password is fine */\n"
        ));
        // Without the terminating semicolon the pattern does not match at all,
        // which is the original's `;` in `(?:true|false);`.
        let untouched = set_allow_no_password("$cfg['Servers'][$i]['AllowNoPassword'] = true\n");
        assert_eq!(untouched, "$cfg['Servers'][$i]['AllowNoPassword'] = true\n");
    }

    #[test]
    fn a_configuration_written_with_carriage_returns_keeps_its_line_endings() {
        let sample =
            "$cfg['blowfish_secret'] = 'x';\r\n$cfg['Servers'][$i]['AllowNoPassword'] = false;\r\n";
        let patched = set_blowfish_secret(sample, "SECRET");
        // `.*$` consumed the carriage return with the comment it followed.
        assert!(patched.contains("= 'SECRET'; /* auto-generated by Lambo PHP */\n"));
        let patched = set_allow_no_password(&patched);
        assert!(patched.contains("/* local dev — empty root password is fine */\n"));
    }

    #[test]
    fn phpmyadmin_gets_a_configuration_of_its_own() {
        let temp = TempDir::new();
        let (log, recorded) = recorder();
        let target = temp.join("www/phpmyadmin");
        fixture(
            temp.path(),
            "www/phpmyadmin/config.sample.inc.php",
            "$cfg['blowfish_secret'] = ''; /* MUST BE FILLED */\n$cfg['Servers'][$i]['AllowNoPassword'] = false;\n$cfg['Servers'][$i]['host'] = '127.0.0.1';\n",
        );

        configure_phpmyadmin(&target, &log).expect("the configuration is created");

        let config = fs::read_to_string(target.join("config.inc.php")).expect("config.inc.php");
        assert!(config.contains("/* auto-generated by Lambo PHP */"));
        assert!(config.contains("'AllowNoPassword'] = true;"));
        assert!(config.contains("$cfg['Servers'][$i]['host'] = '127.0.0.1';"));
        assert!(!config.contains("MUST BE FILLED"));

        // A second run generates a new secret, which is what a fresh random
        // value per install means.
        let first = config
            .lines()
            .find(|line| line.contains("blowfish_secret"))
            .expect("the secret line")
            .to_owned();
        configure_phpmyadmin(&target, &log).expect("patched again");
        let second = fs::read_to_string(target.join("config.inc.php"))
            .expect("config.inc.php")
            .lines()
            .find(|line| line.contains("blowfish_secret"))
            .expect("the secret line")
            .to_owned();
        assert_ne!(first, second);
        assert!(
            second.len() > 32,
            "a real secret, not an empty one: {second}"
        );

        let recorded = lines(&recorded);
        assert!(
            recorded
                .iter()
                .any(|line| line == "  created config.inc.php from sample")
        );
        assert!(
            recorded
                .iter()
                .any(|line| line == "  patched config.inc.php (blowfish_secret, AllowNoPassword)")
        );
    }

    #[test]
    fn a_phpmyadmin_without_a_sample_is_left_alone() {
        let temp = TempDir::new();
        let (log, recorded) = recorder();
        configure_phpmyadmin(&temp.join("www/phpmyadmin"), &log).expect("nothing to do");
        assert!(!temp.join("www/phpmyadmin/config.inc.php").exists());
        assert!(lines(&recorded).is_empty());
    }

    #[test]
    fn the_adminer_guard_is_neutralised_where_it_is_found() {
        let source = "<?php\nfunction adminer() {\n  if($word==\"\")return sprintf('Adminer does not support";
        let patched = disable_password_guard(source).expect("the guard is found");
        assert!(patched.starts_with("<?php\nfunction adminer() {\n"));
        assert!(patched.ends_with("if(false)return sprintf('Adminer does not support"));

        // The whitespace the original's `\s+` consumed may run past a line
        // ending, and comes back as a single space.
        let wrapped = "if($word==\"\")return \n\t  sprintf('Adminer does not support";
        assert_eq!(
            disable_password_guard(wrapped).expect("the wrapped guard is found"),
            "if(false)return sprintf('Adminer does not support"
        );
    }

    #[test]
    fn a_guard_that_appears_twice_is_neutralised_twice() {
        // `ReplaceAllLiteralString` replaced every occurrence, so both copies
        // go: a file that grew a second check keeps working.
        let source = "if($a==\"\")return sprintf('Adminer does not support X; if($b==\"\")return\tsprintf('Adminer does not support Y;";
        let patched = disable_password_guard(source).expect("both are found");
        assert_eq!(patched.matches("if(false)return sprintf(").count(), 2);
        assert!(patched.contains("if(false)return sprintf('Adminer does not support X;"));
        assert!(patched.contains("if(false)return sprintf('Adminer does not support Y;"));
        assert!(!patched.contains("if($"));
    }

    #[test]
    fn a_near_miss_is_not_the_adminer_guard() {
        // No word after `if($`.
        assert_eq!(
            disable_password_guard("if($==\"\")return sprintf('Adminer does not support"),
            None
        );
        // The comparison is not the guard's.
        assert_eq!(
            disable_password_guard("if($word==\"x\")return sprintf('Adminer does not support"),
            None
        );
        // No whitespace between `return` and the message.
        assert_eq!(
            disable_password_guard("if($word==\"\")returnX sprintf('Adminer does not support"),
            None
        );
        // A different message - a newer Adminer may have reworded it, and
        // guessing would be a patch nobody asked for.
        assert_eq!(
            disable_password_guard("if($word==\"\")return sprintf('Something else"),
            None
        );
        assert_eq!(disable_password_guard("<?php // nothing to see"), None);
    }

    #[test]
    fn a_near_miss_before_the_guard_still_leaves_the_guard_patched() {
        let source = "if($==\"\")return sprintf('Adminer does not support nope; if($word==\"\")return sprintf('Adminer does not support yes";
        let patched =
            disable_password_guard(source).expect("the real guard is found after the near miss");
        assert!(patched.starts_with("if($==\"\")return"));
        assert!(patched.ends_with("if(false)return sprintf('Adminer does not support yes"));
    }

    #[test]
    fn the_adminer_hook_reports_rather_than_fails_on_a_file_it_cannot_patch() {
        let temp = TempDir::new();
        let (log, recorded) = recorder();
        fixture(
            temp.path(),
            "www/adminer/index.php",
            "<?php // a newer Adminer",
        );
        neutralise_adminer_password_guard(&temp.join("www/adminer"), &log).expect("not an error");
        assert_eq!(
            fs::read_to_string(temp.join("www/adminer/index.php")).expect("readable"),
            "<?php // a newer Adminer"
        );
        assert!(
            lines(&recorded).iter().any(|line| line
                == "  Adminer: empty-password guard pattern not found (already patched?)")
        );

        // A file that is not there at all *is* an error: the install did not
        // put Adminer where it said it did.
        let error = neutralise_adminer_password_guard(&temp.join("www/absent"), &log)
            .expect_err("adminer must have been unpacked");
        assert!(
            error.to_string().contains("read adminer"),
            "unexpected: {error}"
        );
    }

    #[test]
    fn the_composer_wrapper_is_written_once_and_never_overwritten() {
        let temp = TempDir::new();
        let (log, recorded) = recorder();
        let install = temp.join("bin/php");
        fs::create_dir_all(&install).expect("the install directory");

        write_composer_wrapper(&install, &log).expect("the wrapper is written");
        assert_eq!(
            fs::read_to_string(install.join("composer.bat")).expect("readable"),
            "@echo off\r\nphp \"%~dp0composer.phar\" %*\r\n"
        );

        fs::write(install.join("composer.bat"), "rem mine\r\n").expect("rewrite");
        write_composer_wrapper(&install, &log).expect("nothing to do");
        assert_eq!(
            fs::read_to_string(install.join("composer.bat")).expect("readable"),
            "rem mine\r\n"
        );
        assert_eq!(
            lines(&recorded)
                .iter()
                .filter(|line| *line == "  created composer.bat wrapper")
                .count(),
            1
        );
    }

    #[test]
    fn the_python_bootstrap_files_are_found_by_prefix_and_suffix() {
        let temp = TempDir::new();
        for name in [
            "python311._pth",
            "python312._pth",
            "python._pth",
            "_pth",
            "python311.pth",
        ] {
            fixture(temp.path(), name, "");
        }
        let found: Vec<String> = pth_files(temp.path())
            .iter()
            .map(|path| {
                path.file_name()
                    .expect("a name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            found,
            vec!["python._pth", "python311._pth", "python312._pth"]
        );
        assert!(
            pth_files(&temp.join("absent")).is_empty(),
            "no directory, no files"
        );
    }

    #[test]
    fn python_gets_site_imports_and_keeps_a_pip_it_already_has() {
        let temp = TempDir::new();
        let (log, recorded) = recorder();
        fixture(
            temp.path(),
            "bin/python/python311._pth",
            "python311.zip\n.\n#import site\n",
        );
        fixture(
            temp.path(),
            "bin/python/python312._pth",
            "python312.zip\n.\n",
        );
        // pip is already there, which is what keeps this test off the network:
        // the step that follows *is* a download, and it is covered where the
        // download itself is (`prepare_python` composes the shared HTTPS
        // client with the process runner, both tested on their own).
        fixture(temp.path(), "bin/python/Scripts/pip.exe", "MZ");

        prepare_python(&temp.join("bin/python"), &log, &Refusing).expect("not a failure");

        let patched = fs::read_to_string(temp.join("bin/python/python311._pth")).expect("readable");
        assert!(patched.contains("\nimport site\n"));
        assert!(!patched.contains("#import site"));
        // A file without the line is left alone entirely.
        assert_eq!(
            fs::read_to_string(temp.join("bin/python/python312._pth")).expect("readable"),
            "python312.zip\n.\n"
        );
        let recorded = lines(&recorded);
        assert!(
            recorded
                .iter()
                .any(|line| line == "  uncommented import site in python311._pth")
        );
        assert!(
            recorded
                .iter()
                .any(|line| line == "  pip already installed")
        );
    }

    #[test]
    fn the_bootstrap_urls_are_the_ones_the_original_used() {
        // The two downloads that do not come from the catalogue. They are
        // pinned here because changing either is a behaviour change: the Erlang
        // installer is what RabbitMQ's own release notes pair with, and pip's
        // bootstrap script is fetched from the address pip documents.
        assert_eq!(GET_PIP_URL, "https://bootstrap.pypa.io/get-pip.py");
        assert_eq!(ERLANG_VERSION, "27.3.4");
        assert_eq!(ERLANG_FILE_NAME, "otp_win64_27.3.4.exe");
        assert_eq!(
            ERLANG_URL,
            "https://github.com/erlang/otp/releases/download/OTP-27.3.4/otp_win64_27.3.4.exe"
        );
        assert!(GET_PIP_URL.starts_with("https://"));
    }

    #[test]
    fn the_mariadb_data_directory_is_initialised_once_and_wiped_when_stale() {
        let temp = TempDir::new();
        let (log, recorded) = recorder();
        let install = temp.join("bin/mysql");

        // An initialised cluster is never touched, even if the tool is gone.
        fs::create_dir_all(install.join("data").join("mysql")).expect("the cluster directory");
        initialise_mariadb(&install, &log).expect("already initialised");
        assert!(
            lines(&recorded)
                .iter()
                .any(|line| line == "  data dir already initialized, skipping")
        );
        assert!(install.join("data/mysql").is_dir());
    }

    #[test]
    fn a_stale_mariadb_data_directory_is_wiped_and_a_missing_tool_is_an_error() {
        let temp = TempDir::new();
        let (log, recorded) = recorder();
        let install = temp.join("bin/mysql");
        fixture(temp.path(), "bin/mysql/data/half-written.txt", "left over");

        let error = initialise_mariadb(&install, &log).expect_err("the tool is not installed");
        assert!(
            error
                .to_string()
                .contains("mariadb-install-db.exe not found"),
            "unexpected: {error}"
        );

        // The wipe happened before the tool was looked for, which is what lets
        // the next attempt succeed: MariaDB refuses a non-empty data directory.
        assert!(
            lines(&recorded)
                .iter()
                .any(|line| line == "  wiping stale data/ from earlier broken install")
        );
        assert_eq!(
            fs::read_dir(install.join("data"))
                .map(|entries| entries.count())
                .unwrap_or(0),
            0
        );
    }

    #[test]
    fn the_postgres_cluster_is_initialised_once_and_a_missing_tool_is_an_error() {
        let temp = TempDir::new();
        let (log, recorded) = recorder();
        let install = temp.join("bin/pgsql");

        fixture(temp.path(), "bin/pgsql/data/PG_VERSION", "17\n");
        initialise_postgres(&install, &log).expect("already initialised");
        assert!(
            lines(&recorded)
                .iter()
                .any(|line| line == "  cluster already initialized, skipping")
        );

        let temp = TempDir::new();
        let (log, _) = recorder();
        let error = initialise_postgres(&temp.join("bin/pgsql"), &log).expect_err("no initdb");
        assert!(
            error.to_string().contains("initdb.exe not found"),
            "unexpected: {error}"
        );
    }

    #[test]
    fn minio_gets_its_data_directory() {
        let temp = TempDir::new();
        let (log, recorded) = recorder();
        prepare_minio(&temp.join("bin/minio"), &log).expect("the data directory is created");
        assert!(temp.join("data/minio").is_dir());
        assert!(
            lines(&recorded)
                .iter()
                .any(|line| line == "  created data/minio/ — MinIO will store objects here")
        );
    }

    #[test]
    fn the_rust_toolchain_is_installed_from_the_downloaded_bootstrap_only() {
        let temp = TempDir::new();
        let (log, _) = recorder();
        let error = install_rust_toolchain(&temp.join("bin/rust"), &log)
            .expect_err("rustup-init.exe is not installed");
        assert!(
            error.to_string().contains("rustup-init.exe not found"),
            "unexpected: {error}"
        );
    }

    #[test]
    fn rabbitmq_installs_erlang_from_the_cache_and_creates_its_state_directory() {
        let temp = TempDir::new();
        let (log, recorded) = recorder();
        let mut cache = DownloadCache::new(temp.path(), Arc::clone(&log), Box::new(Refusing));

        // Erlang is missing, so the installer is fetched - and the cache
        // refuses, which surfaces as the download error the original reported.
        let error = prepare_rabbitmq(&temp.join("bin/rabbitmq"), &log, &mut cache)
            .expect_err("the download is refused");
        assert!(
            error.to_string().contains("erlang download: "),
            "unexpected: {error}"
        );
        assert!(
            lines(&recorded)
                .iter()
                .any(|line| line == "  Erlang not found — downloading OTP 27.3.4...")
        );

        // With Erlang present the install goes on to the state directory, and
        // the plugin step is skipped because RabbitMQ is not unpacked yet.
        let temp = TempDir::new();
        let (log, recorded) = recorder();
        let mut cache = DownloadCache::new(temp.path(), Arc::clone(&log), Box::new(Refusing));
        fixture(temp.path(), "bin/erlang/bin/erl.exe", "MZ");
        prepare_rabbitmq(&temp.join("bin/rabbitmq"), &log, &mut cache)
            .expect("state directory only");
        assert!(temp.join("data/rabbitmq").is_dir());
        assert!(
            lines(&recorded)
                .iter()
                .any(|line| line == "  created data/rabbitmq/ — RabbitMQ stores state here")
        );
    }

    #[test]
    fn every_hook_can_be_dispatched() {
        // The dispatcher is the table the original kept inline. Each arm is
        // exercised through it, so a hook that is never reached is a test
        // failure rather than a silent gap.
        let temp = TempDir::new();
        let (log, _) = recorder();
        let mut cache = DownloadCache::new(temp.path(), Arc::clone(&log), Box::new(Refusing));

        fixture(temp.path(), "bin/php/php.ini-development", PHP_INI);
        fixture(
            temp.path(),
            "www/adminer/index.php",
            "<?php if($w==\"\")return sprintf('Adminer does not support",
        );
        fixture(
            temp.path(),
            "bin/apache/conf/httpd.conf",
            "ServerRoot \"C:/Apache24\"\r\n",
        );
        // pip is present, so the Python hook stops before its download.
        fixture(temp.path(), "bin/python/Scripts/pip.exe", "MZ");

        let cases: [(Hook, PathBuf, bool); 11] = [
            (Hook::ApacheHttpdConf, temp.join("bin/apache"), true),
            (Hook::PhpIni, temp.join("bin/php"), true),
            (Hook::MariaDbDataDir, temp.join("bin/mysql"), false),
            (Hook::PostgresCluster, temp.join("bin/pgsql"), false),
            (Hook::PhpMyAdminConfig, temp.join("www/phpmyadmin"), true),
            (Hook::AdminerBlankPassword, temp.join("www/adminer"), true),
            (Hook::ComposerBat, temp.join("bin/php"), true),
            (Hook::PythonPip, temp.join("bin/python"), true),
            (Hook::RustToolchain, temp.join("bin/rust"), false),
            (Hook::RabbitMqErlang, temp.join("bin/rabbitmq"), false),
            (Hook::MinioDataDir, temp.join("bin/minio"), true),
        ];

        for (hook, install_dir, expected) in cases {
            let mut ctx = context(temp.path(), &log, &mut cache);
            let outcome = run(hook, &install_dir, &mut ctx);
            assert_eq!(outcome.is_ok(), expected, "{}: {outcome:?}", hook.as_str());
        }

        // The four that were expected to succeed left the filesystem in the
        // state their hook promises.
        assert!(temp.join("bin/php/php.ini").is_file());
        assert!(temp.join("bin/php/composer.bat").is_file());
        assert!(temp.join("www/adminer/index.php").is_file());
        assert!(
            fs::read_to_string(temp.join("www/adminer/index.php"))
                .expect("readable")
                .contains("if(false)return")
        );
        assert!(temp.join("data/minio").is_dir());
        let conf = fs::read_to_string(temp.join("bin/apache/conf/httpd.conf")).expect("readable");
        assert!(
            conf.contains(PHP_HANDLER_BEGIN),
            "the handler block is appended"
        );
        assert!(
            conf.contains(VHOST_INCLUDE_BEGIN),
            "the vhost include is appended"
        );
    }
}
