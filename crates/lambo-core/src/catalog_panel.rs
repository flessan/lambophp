//! The control-panel catalogue: every component the dashboard can install.
//!
//! This is the table behind the service cards. It is separate from
//! [`crate::catalog`], which serves the CLI's runtime manager: that one is a
//! JSON document a user can override, keyed by family and platform, and it
//! describes where to find *PHP*, *Apache* and *MariaDB* for `lambo php
//! install`. This one is the panel's own list - thirty services including
//! Redis, RabbitMQ, MinIO, Mailpit and sixteen language runtimes - with the
//! install shape (archive kind, wrapper directory to strip, check file,
//! post-install hook) that each one needs.
//!
//! Ported from the original implementation's `catalog.go`. Every version, URL,
//! file name, install
//! directory, strip prefix, check file, note, variant, resolver and hook is
//! transcribed, because they are the product: a card that installs the wrong
//! build is a broken feature, not a stale comment. The two entries whose URL
//! cannot be hard-coded (Apache, Zig) keep their resolver and their bundled
//! fallback URL, and fall back the same way the original did.
//!
//! # Shape
//!
//! A [`Component`] describes *what* to install; [`plan`] turns it into an
//! [`InstallPlan`] - the exact URL, file name, strip prefix, destination and
//! hook for one request. Planning is pure, so the GUI can show what an install
//! is going to do before it does it, and the installer executes a plan rather
//! than re-deriving one.
//!
//! # Windows-only, deliberately
//!
//! Every archive here is a Win32, Win64 or `.exe` build, and three of the
//! hooks run Windows executables with Windows-specific arguments. The parts
//! that are pure data - the table, planning, install detection - work
//! anywhere, and their tests run everywhere; what cannot work elsewhere says
//! so instead of pretending ([`crate::postinstall`]).
//!
//! # Known defects, preserved rather than silently fixed
//!
//! * **`Swift` has no entry.** It appears in the default configuration's
//!   service list and in the dashboard's icon map, but never here - so its card
//!   has nothing to install and its terminal button opens a bare `cmd.exe`.
//!   That is deliberate, and a test asserts the absence so the gap stays a
//!   decision rather than becoming an accident.
//! * **PHP's note is stale.** The entry installs PHP 8.4.22 while its note
//!   still talks about PHP 7.4 and the VC15 runtime. Correcting the text would
//!   change what the dashboard shows, so it is preserved verbatim.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::vendor::Resolved;

/// How a downloaded artifact is installed.
///
/// The previous implementation compared bare strings in a switch, so a typo in
/// the catalogue fell through to an "unknown kind" error at install time
/// rather than failing to compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// An archive: extracted, with [`Component::strip_top`] removed.
    Zip,
    /// A silent installer: run with `/S /D=<dir>`.
    Exe,
    /// A single file: copied to [`Component::target_file`].
    File,
}

impl Kind {
    /// The kind's wire name, as the original spelled it.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Zip => "zip",
            Kind::Exe => "exe",
            Kind::File => "file",
        }
    }
}

/// One alternative build of a component.
///
/// Only PHP and the language runtimes have these, and only they install into a
/// version-suffixed directory (`bin/php-8.3`) with the canonical directory
/// mirroring whichever is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Variant {
    /// The short version the dashboard shows and the user picks, e.g. `8.3`.
    pub version: &'static str,
    /// Where to download that build.
    pub url: &'static str,
    /// What to call it in the download cache.
    pub file_name: &'static str,
    /// The wrapper directory this variant's archive has.
    ///
    /// Always set in practice: the original assigned the variant's own prefix
    /// unconditionally, so an empty one - `Some("")` - is what says "this build
    /// ships no wrapper directory" and stops the component's prefix from being
    /// used. `None` means the same thing here, but the catalogue spells it out.
    pub strip_top: Option<&'static str>,
    /// What the dashboard says about this version. Empty means "keep the
    /// component's own note", which is what the original did.
    pub notes: &'static str,
}

/// A download whose URL is discovered at install time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolver {
    /// The newest Win64 build listed on Apache Lounge.
    ApacheLatest,
    /// The newest stable release in Zig's index.
    ZigLatest,
}

impl Resolver {
    /// The resolver's name as the previous implementation spelled it.
    pub fn as_str(self) -> &'static str {
        match self {
            Resolver::ApacheLatest => "resolveApacheLatest",
            Resolver::ZigLatest => "resolveZigLatest",
        }
    }
}

/// What has to run after a component is unpacked.
///
/// One variant per hook, so the install path is a dispatch rather than a
/// function pointer: the GUI can name the step it is about to run, and a hook
/// can be tested by calling it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hook {
    /// Apache: patch `httpd.conf`, then seed the runtime files.
    ApacheHttpdConf,
    /// PHP: create and patch `php.ini`, then mirror the runtime DLLs.
    PhpIni,
    /// MariaDB: initialise the data directory.
    MariaDbDataDir,
    /// PostgreSQL: seed the VC++ runtime, then run `initdb`.
    PostgresCluster,
    /// phpMyAdmin: create and patch `config.inc.php`.
    PhpMyAdminConfig,
    /// Adminer: neutralise the empty-password guard.
    AdminerBlankPassword,
    /// Composer: write the `composer.bat` wrapper.
    ComposerBat,
    /// Python: uncomment `import site`, then bootstrap pip.
    PythonPip,
    /// Rust: run `rustup-init` into the component's own toolchain directories.
    RustToolchain,
    /// RabbitMQ: ensure Erlang, create the data directory, enable management.
    RabbitMqErlang,
    /// MinIO: create the object-storage data directory.
    MinioDataDir,
}

impl Hook {
    /// The name the log uses for this hook.
    pub fn as_str(self) -> &'static str {
        match self {
            Hook::ApacheHttpdConf => "httpd.conf",
            Hook::PhpIni => "php.ini",
            Hook::MariaDbDataDir => "mariadb-data-dir",
            Hook::PostgresCluster => "postgres-cluster",
            Hook::PhpMyAdminConfig => "phpmyadmin-config",
            Hook::AdminerBlankPassword => "adminer-blank-password",
            Hook::ComposerBat => "composer-bat",
            Hook::PythonPip => "python-pip",
            Hook::RustToolchain => "rust-toolchain",
            Hook::RabbitMqErlang => "rabbitmq-erlang",
            Hook::MinioDataDir => "minio-data-dir",
        }
    }
}

/// One installable component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Component {
    /// The dashboard's name for it, and the key everything looks it up by.
    pub name: &'static str,
    /// The version as shown, e.g. `8.4.22 NTS x64` or `latest stable`.
    pub version: &'static str,
    /// Where to download it, unless [`Component::resolver`] says otherwise.
    pub url: &'static str,
    /// What to call it in the download cache.
    pub file_name: &'static str,
    /// Where it is installed, relative to the installation root and with `/`
    /// separators (`bin/apache`, `www/adminer`).
    pub install_dir: &'static str,
    /// The wrapper directory the archive wraps its contents in, if any.
    pub strip_top: Option<&'static str>,
    /// How the download is installed.
    pub kind: Kind,
    /// The file a single-file download is written as.
    pub target_file: Option<&'static str>,
    /// The file whose presence means "installed".
    pub check_file: &'static str,
    /// What the dashboard says about it.
    pub notes: &'static str,
    /// Alternative builds, when the component is multi-version.
    pub variants: &'static [Variant],
    /// The resolver to ask first, when the URL is not fixed.
    pub resolver: Option<Resolver>,
    /// What runs after the install.
    pub hook: Option<Hook>,
}

impl Component {
    /// Whether this component offers more than one version.
    pub fn is_multi_version(&self) -> bool {
        !self.variants.is_empty()
    }

    /// The canonical directory: where a single-version component lives, and
    /// what a multi-version component's active build mirrors into.
    pub fn canonical_dir(&self, base_dir: &Path) -> PathBuf {
        base_dir.join(self.install_dir)
    }

    /// The directory this component (or one of its versions) installs into.
    ///
    /// A multi-version component with a version selected installs into a
    /// version-suffixed sibling (`bin/php-8.3`); everything else installs into
    /// the canonical directory. Named `install_dir_for` rather than
    /// `install_dir` because [`entry`]'s builder already owns that name on the
    /// same type.
    pub fn install_dir_for(&self, base_dir: &Path, version: &str) -> PathBuf {
        let canonical = self.canonical_dir(base_dir);
        if self.is_multi_version() && !version.is_empty() {
            let mut name = canonical
                .file_name()
                .map(|name| name.to_os_string())
                .unwrap_or_default();
            name.push("-");
            name.push(version);
            canonical.with_file_name(name)
        } else {
            canonical
        }
    }

    /// The variant matching `version`.
    ///
    /// The error text is the original's, including the `[Name] ` prefix, so a
    /// caller that only forwards the message still reports which component was
    /// asked for.
    pub fn variant(&self, version: &str) -> Result<&'static Variant> {
        self.variants
            .iter()
            .find(|variant| variant.version == version)
            .ok_or_else(|| {
                Error::InvalidInput(format!(
                    "[{}] version {version:?} not in catalogue",
                    self.name
                ))
            })
    }
}

/// A resolved install: everything needed to fetch, unpack and finish one
/// component, with nothing left to look up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    /// The component's name.
    pub name: String,
    /// The version as shown, after any resolver or variant override.
    pub version: String,
    /// The note to log before the download.
    pub notes: String,
    /// Where to download from.
    pub url: String,
    /// What to call the download in the cache.
    pub file_name: String,
    /// The wrapper directory to strip, when there is one.
    pub strip_top: Option<String>,
    /// How to install it.
    pub kind: Kind,
    /// The file a single-file download is written as.
    pub target_file: String,
    /// The file whose presence means "already installed".
    pub check_file: String,
    /// Where the artifact is unpacked.
    pub install_dir: PathBuf,
    /// The canonical directory, which differs from `install_dir` only for a
    /// versioned install.
    pub canonical_dir: PathBuf,
    /// What runs after the install.
    pub hook: Option<Hook>,
}

impl InstallPlan {
    /// Whether the install lands in a version-suffixed directory.
    pub fn is_versioned(&self) -> bool {
        self.install_dir != self.canonical_dir
    }

    /// Whether the component is already installed, by its own check file.
    pub fn is_installed(&self) -> bool {
        probe(&self.check_file, &self.install_dir)
    }
}

/// Whether `check_file` exists inside `install_dir`.
///
/// An empty check file means "cannot tell", and the original answered `false`
/// for exactly that reason: it is better to re-run an install than to claim a
/// component is present without evidence. The shipped catalogue has no such
/// entry, but the rule is part of the behaviour.
pub fn probe(check_file: &str, install_dir: &Path) -> bool {
    if check_file.is_empty() {
        return false;
    }
    install_dir.join(check_file).exists()
}

/// Whether a component is installed, by name.
///
/// **An unknown name reports `true`.** That is the original's behaviour and it
/// is deliberate: the dashboard asks about every card it draws, and a component
/// it cannot install is one it must not offer to install. Callers that need to
/// know whether a component *exists* use [`find`] or [`plan`], both of which
/// distinguish the two cases honestly.
pub fn is_installed(name: &str, base_dir: &Path) -> bool {
    let Some(component) = find(name) else {
        return true;
    };
    probe(component.check_file, &component.canonical_dir(base_dir))
}

/// Plans an install, resolving the URL, the version and the destination.
///
/// The order is the original's, and it matters:
///
/// 1. A [`Component::resolver`], when the caller resolved one, replaces the URL,
///    file name, strip prefix and version label. When the resolver *failed*, the
///    caller passes `None` and the bundled URL is used instead - the fallback is
///    the point of having one.
/// 2. A variant overrides all four of those again, and its note replaces the
///    component's when it has one.
/// 3. The install directory gains a `-{version}` suffix only when the component
///    has variants *and* a version was asked for, which is also the only case in
///    which the version is validated.
pub fn plan(
    component: &Component,
    version: &str,
    base_dir: &Path,
    resolved: Option<&Resolved>,
) -> Result<InstallPlan> {
    let mut url = component.url.to_owned();
    let mut file_name = component.file_name.to_owned();
    let mut strip_top = component.strip_top.map(str::to_owned);
    let mut notes = component.notes.to_owned();
    let mut version_label = component.version.to_owned();

    if let Some(resolved) = resolved {
        url.clone_from(&resolved.url);
        file_name.clone_from(&resolved.file_name);
        // The resolver always answers with a prefix, empty when the archive is
        // not wrapped; the plan's field is an `Option`, and the filter below is
        // the `stripTop != ""` the original's extraction checked.
        strip_top = Some(resolved.strip_top.clone());
        version_label.clone_from(&resolved.version);
    }

    if component.is_multi_version() && !version.is_empty() {
        let variant = component.variant(version)?;
        url = variant.url.to_owned();
        file_name = variant.file_name.to_owned();
        strip_top = variant.strip_top.map(str::to_owned);
        if !variant.notes.is_empty() {
            notes = variant.notes.to_owned();
        }
        version_label = variant.version.to_owned();
    }

    // An empty prefix filters nothing, so it is normalised away: a plan either
    // has a wrapper directory to strip or it does not.
    let strip_top = strip_top.filter(|prefix| !prefix.is_empty());

    let canonical_dir = component.canonical_dir(base_dir);
    let install_dir = component.install_dir_for(base_dir, version);

    Ok(InstallPlan {
        name: component.name.to_owned(),
        version: version_label,
        notes,
        url,
        file_name,
        strip_top,
        kind: component.kind,
        target_file: component
            .target_file
            .map(str::to_owned)
            .unwrap_or_else(|| component.file_name.to_owned()),
        check_file: component.check_file.to_owned(),
        install_dir,
        canonical_dir,
        hook: component.hook,
    })
}

/// Plans an install by component name.
pub fn plan_for(
    name: &str,
    version: &str,
    base_dir: &Path,
    resolved: Option<&Resolved>,
) -> Result<InstallPlan> {
    let component = find(name)
        .ok_or_else(|| Error::InvalidInput(format!("no download info registered for {name:?}")))?;
    plan(component, version, base_dir, resolved)
}

/// The component with this name, if the dashboard can install it.
///
/// The lookup is case-sensitive, as the original's map lookups were: the
/// dashboard only ever passes names it took from the configuration, which are
/// the catalogue's own spellings.
pub fn find(name: &str) -> Option<&'static Component> {
    entries().iter().find(|component| component.name == name)
}

/// The catalogue.
///
/// The order is the order the entries appear in the original file. The original
/// held them in a map, so it had no order at all; a list is used here because
/// every screen that shows "all installable components" needs one, and a
/// deterministic order is better than a random one.
pub fn entries() -> &'static [Component] {
    COMPONENTS
}

/// Builds a catalogue entry. Used by [`COMPONENTS`] to stay readable, and
/// public so a test can describe a shape the shipped table does not have.
pub const fn entry(name: &'static str) -> Component {
    Component {
        name,
        version: "",
        url: "",
        file_name: "",
        install_dir: "",
        strip_top: None,
        kind: Kind::Zip,
        target_file: None,
        check_file: "",
        notes: "",
        variants: &[],
        resolver: None,
        hook: None,
    }
}

impl Component {
    /// Sets the displayed version. Builder for [`entry`].
    pub const fn version(mut self, version: &'static str) -> Self {
        self.version = version;
        self
    }

    /// Sets the download URL. Builder for [`entry`].
    pub const fn url(mut self, url: &'static str) -> Self {
        self.url = url;
        self
    }

    /// Sets the cache file name. Builder for [`entry`].
    pub const fn file_name(mut self, file_name: &'static str) -> Self {
        self.file_name = file_name;
        self
    }

    /// Sets the install directory. Builder for [`entry`].
    pub const fn install_dir(mut self, install_dir: &'static str) -> Self {
        self.install_dir = install_dir;
        self
    }

    /// Sets the wrapper directory to strip. Builder for [`entry`].
    pub const fn strip_top(mut self, strip_top: &'static str) -> Self {
        self.strip_top = Some(strip_top);
        self
    }

    /// Sets how the download is installed. Builder for [`entry`].
    pub const fn kind(mut self, kind: Kind) -> Self {
        self.kind = kind;
        self
    }

    /// Sets the file a single-file download is written as. Builder for
    /// [`entry`].
    pub const fn target_file(mut self, target_file: &'static str) -> Self {
        self.target_file = Some(target_file);
        self
    }

    /// Sets the file whose presence means "installed". Builder for [`entry`].
    pub const fn check_file(mut self, check_file: &'static str) -> Self {
        self.check_file = check_file;
        self
    }

    /// Sets the note shown on the card. Builder for [`entry`].
    pub const fn notes(mut self, notes: &'static str) -> Self {
        self.notes = notes;
        self
    }

    /// Sets the alternative builds. Builder for [`entry`].
    pub const fn variants(mut self, variants: &'static [Variant]) -> Self {
        self.variants = variants;
        self
    }

    /// Sets the URL resolver. Builder for [`entry`].
    pub const fn resolver(mut self, resolver: Resolver) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// Sets the post-install hook. Builder for [`entry`].
    pub const fn hook(mut self, hook: Hook) -> Self {
        self.hook = Some(hook);
        self
    }
}

/// Every installable component, in the order the original listed them.
pub static COMPONENTS: &[Component] = &[
    entry("Apache")
        .version("2.4.68 (VS18, win64)")
        .url("https://www.apachelounge.com/download/VS18/binaries/httpd-2.4.68-260827-Win64-VS18.zip")
        .file_name("httpd-2.4.68-260827-Win64-VS18.zip")
        .install_dir("bin/apache")
        .strip_top("Apache24/")
        .kind(Kind::Zip)
        .check_file("conf/httpd.conf")
        .notes("Apache Lounge build — requires VC++ 2015-2022 Redistributable.")
        .resolver(Resolver::ApacheLatest)
        .hook(Hook::ApacheHttpdConf),
    entry("Nginx")
        .version("1.28.3 stable")
        .url("https://nginx.org/download/nginx-1.28.3.zip")
        .file_name("nginx-1.28.3.zip")
        .install_dir("bin/nginx")
        .strip_top("nginx-1.28.3/")
        .kind(Kind::Zip)
        .check_file("nginx.exe"),
    entry("PHP-FPM")
        .version("8.4.22 NTS x64")
        .url("https://windows.php.net/downloads/releases/php-8.4.22-nts-Win32-vs17-x64.zip")
        .file_name("php-8.4.22-nts-Win32-vs17-x64.zip")
        .install_dir("bin/php")
        .strip_top("")
        .kind(Kind::Zip)
        .check_file("php-cgi.exe")
        .notes("Requires VC++ 2015-2022 Redistributable x64.")
        .hook(Hook::PhpIni)
        .variants(&[
            Variant {
                version: "7.4",
                url: "https://windows.php.net/downloads/releases/archives/php-7.4.33-nts-Win32-vc15-x64.zip",
                file_name: "php-7.4.33-nts-Win32-vc15-x64.zip",
                strip_top: Some(""),
                notes: "PHP 7.4 — needs VC15 (VS2017) runtime.",
            },
            Variant {
                version: "8.0",
                url: "https://windows.php.net/downloads/releases/archives/php-8.0.30-nts-Win32-vs16-x64.zip",
                file_name: "php-8.0.30-nts-Win32-vs16-x64.zip",
                strip_top: Some(""),
                notes: "PHP 8.0 — VS16 (VS2019) runtime.",
            },
            Variant {
                version: "8.1",
                url: "https://windows.php.net/downloads/releases/archives/php-8.1.31-nts-Win32-vs16-x64.zip",
                file_name: "php-8.1.31-nts-Win32-vs16-x64.zip",
                strip_top: Some(""),
                notes: "PHP 8.1 — VS16 (VS2019) runtime.",
            },
            Variant {
                version: "8.2",
                url: "https://windows.php.net/downloads/releases/archives/php-8.2.27-nts-Win32-vs16-x64.zip",
                file_name: "php-8.2.27-nts-Win32-vs16-x64.zip",
                strip_top: Some(""),
                notes: "PHP 8.2 — VS16 (VS2019) runtime.",
            },
            Variant {
                version: "8.3",
                url: "https://windows.php.net/downloads/releases/archives/php-8.3.15-nts-Win32-vs16-x64.zip",
                file_name: "php-8.3.15-nts-Win32-vs16-x64.zip",
                strip_top: Some(""),
                notes: "PHP 8.3 — VS16 (VS2019) runtime.",
            },
            Variant {
                version: "8.4",
                url: "https://windows.php.net/downloads/releases/php-8.4.22-nts-Win32-vs17-x64.zip",
                file_name: "php-8.4.22-nts-Win32-vs17-x64.zip",
                strip_top: Some(""),
                notes: "PHP 8.4 — VS17 (VS2022) runtime, v14.4+.",
            },
            Variant {
                version: "8.5",
                url: "https://downloads.php.net/~windows/releases/archives/php-8.5.7-nts-Win32-vs17-x64.zip",
                file_name: "php-8.5.7-nts-Win32-vs17-x64.zip",
                strip_top: Some(""),
                notes: "PHP 8.5 — VS17 (VS2022) runtime, v14.4+.",
            },
        ]),
    entry("MySQL")
        .version("MariaDB 11.4.10 LTS")
        .url("https://archive.mariadb.org/mariadb-11.4.10/winx64-packages/mariadb-11.4.10-winx64.zip")
        .file_name("mariadb-11.4.10-winx64.zip")
        .install_dir("bin/mysql")
        .strip_top("mariadb-11.4.10-winx64/")
        .kind(Kind::Zip)
        .check_file("data/mysql")
        .notes("MariaDB — MySQL-compatible drop-in.")
        .hook(Hook::MariaDbDataDir),
    entry("PostgreSQL")
        .version("16.6 LTS (EDB)")
        .url("https://get.enterprisedb.com/postgresql/postgresql-16.6-1-windows-x64-binaries.zip")
        .file_name("postgresql-16.6-1-windows-x64-binaries.zip")
        .install_dir("bin/pgsql")
        .strip_top("pgsql/")
        .kind(Kind::Zip)
        .check_file("data/PG_VERSION")
        .notes("EDB 16.6 LTS — most stable Postgres on Windows; supported until 2028.")
        .hook(Hook::PostgresCluster),
    entry("Redis")
        .version("5.0.14.1 (tporadowski)")
        .url("https://github.com/tporadowski/redis/releases/download/v5.0.14.1/Redis-x64-5.0.14.1.zip")
        .file_name("Redis-x64-5.0.14.1.zip")
        .install_dir("bin/redis")
        .strip_top("")
        .kind(Kind::Zip)
        .check_file("redis-server.exe")
        .notes("Port by Tomasz Poradowski — the Microsoft fork is abandoned."),
    entry("phpMyAdmin")
        .version("5.2.3")
        .url("https://files.phpmyadmin.net/phpMyAdmin/5.2.3/phpMyAdmin-5.2.3-all-languages.zip")
        .file_name("phpMyAdmin-5.2.3-all-languages.zip")
        .install_dir("www/phpmyadmin")
        .strip_top("phpMyAdmin-5.2.3-all-languages/")
        .kind(Kind::Zip)
        .check_file("index.php")
        .notes("Served by Apache — make sure Apache's DocumentRoot points at {base}/www.")
        .hook(Hook::PhpMyAdminConfig),
    entry("pgweb")
        .version("0.16.2")
        .url("https://github.com/sosedoff/pgweb/releases/download/v0.16.2/pgweb_windows_amd64.zip")
        .file_name("pgweb_windows_amd64.zip")
        .install_dir("bin/pgweb")
        .kind(Kind::Zip)
        .check_file("pgweb.exe")
        .notes("Modern PostgreSQL web client — runs at http://localhost:8081 once started."),
    entry("Adminer")
        .version("5.4.2")
        .url("https://github.com/vrana/adminer/releases/download/v5.4.2/adminer-5.4.2-en.php")
        .file_name("adminer-5.4.2-en.php")
        .install_dir("www/adminer")
        .kind(Kind::File)
        .target_file("index.php")
        .check_file("index.php")
        .hook(Hook::AdminerBlankPassword),
    entry("Composer")
        .version("latest stable")
        .url("https://getcomposer.org/composer-stable.phar")
        .file_name("composer-stable.phar")
        .install_dir("bin/php")
        .kind(Kind::File)
        .target_file("composer.phar")
        .check_file("composer.bat")
        .notes("Composer — PHP dependency manager. Installs into bin/php alongside php.exe.")
        .hook(Hook::ComposerBat),
    entry("Node.js")
        .version("22.22.2 LTS")
        .url("https://nodejs.org/dist/v22.22.2/node-v22.22.2-win-x64.zip")
        .file_name("node-v22.22.2-win-x64.zip")
        .install_dir("bin/node")
        .strip_top("node-v22.22.2-win-x64/")
        .kind(Kind::Zip)
        .check_file("node.exe")
        .notes("JavaScript runtime — npm and npx are included alongside node.exe.")
        .variants(&[
            Variant {
                version: "18",
                url: "https://nodejs.org/dist/v18.20.5/node-v18.20.5-win-x64.zip",
                file_name: "node-v18.20.5-win-x64.zip",
                strip_top: Some("node-v18.20.5-win-x64/"),
                notes: "Node 18 LTS (Hydrogen, EOL April 2025).",
            },
            Variant {
                version: "20",
                url: "https://nodejs.org/dist/v20.18.1/node-v20.18.1-win-x64.zip",
                file_name: "node-v20.18.1-win-x64.zip",
                strip_top: Some("node-v20.18.1-win-x64/"),
                notes: "Node 20 LTS (Iron).",
            },
            Variant {
                version: "22",
                url: "https://nodejs.org/dist/v22.22.2/node-v22.22.2-win-x64.zip",
                file_name: "node-v22.22.2-win-x64.zip",
                strip_top: Some("node-v22.22.2-win-x64/"),
                notes: "Node 22 LTS (Jod) — current default.",
            },
        ]),
    entry("Python")
        .version("3.13.13 (embeddable)")
        .url("https://www.python.org/ftp/python/3.13.13/python-3.13.13-embed-amd64.zip")
        .file_name("python-3.13.13-embed-amd64.zip")
        .install_dir("bin/python")
        .strip_top("")
        .kind(Kind::Zip)
        .check_file("python.exe")
        .notes("Embeddable Python — pip is bootstrapped automatically post-install.")
        .hook(Hook::PythonPip)
        .variants(&[
            Variant {
                version: "3.10",
                url: "https://www.python.org/ftp/python/3.10.11/python-3.10.11-embed-amd64.zip",
                file_name: "python-3.10.11-embed-amd64.zip",
                strip_top: Some(""),
                notes: "Python 3.10 (security-only).",
            },
            Variant {
                version: "3.11",
                url: "https://www.python.org/ftp/python/3.11.9/python-3.11.9-embed-amd64.zip",
                file_name: "python-3.11.9-embed-amd64.zip",
                strip_top: Some(""),
                notes: "Python 3.11.",
            },
            Variant {
                version: "3.12",
                url: "https://www.python.org/ftp/python/3.12.7/python-3.12.7-embed-amd64.zip",
                file_name: "python-3.12.7-embed-amd64.zip",
                strip_top: Some(""),
                notes: "Python 3.12.",
            },
            Variant {
                version: "3.13",
                url: "https://www.python.org/ftp/python/3.13.13/python-3.13.13-embed-amd64.zip",
                file_name: "python-3.13.13-embed-amd64.zip",
                strip_top: Some(""),
                notes: "Python 3.13 — current default.",
            },
        ]),
    entry("Go")
        .version("1.27.1")
        .url("https://go.dev/dl/go1.27.1.windows-amd64.zip")
        .file_name("go1.27.1.windows-amd64.zip")
        .install_dir("bin/go")
        .strip_top("go/")
        .kind(Kind::Zip)
        .check_file("bin/go.exe")
        .notes("Go compiler + standard tooling. Use `go run`, `go build`, `go mod`."),
    entry("Java")
        .version("Temurin JDK 21.0.10+7")
        .url("https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.10%2B7/OpenJDK21U-jdk_x64_windows_hotspot_21.0.10_7.zip")
        .file_name("OpenJDK21U-jdk_x64_windows_hotspot_21.0.10_7.zip")
        .install_dir("bin/java")
        .strip_top("jdk-21.0.10+7/")
        .kind(Kind::Zip)
        .check_file("bin/java.exe")
        .notes("Eclipse Temurin LTS — javac, java, jar all under bin/."),
    entry("Julia")
        .version("1.11.5")
        .url("https://julialang-s3.julialang.org/bin/winnt/x64/1.11/julia-1.11.5-win64.zip")
        .file_name("julia-1.11.5-win64.zip")
        .install_dir("bin/julia")
        .strip_top("julia-1.11.5/")
        .kind(Kind::Zip)
        .check_file("bin/julia.exe")
        .notes("Julia — high-performance scientific computing language. REPL + package manager included."),
    entry("Zig")
        .version("latest stable")
        .url("https://ziglang.org/download/0.14.0/zig-windows-x86_64-0.14.0.zip")
        .file_name("zig-windows-x86_64-0.14.0.zip")
        .install_dir("bin/zig")
        .strip_top("zig-windows-x86_64-0.14.0/")
        .kind(Kind::Zip)
        .check_file("zig.exe")
        .notes("Zig — systems programming language + build system + C compiler.")
        .resolver(Resolver::ZigLatest),
    entry("Dart")
        .version("3.7.2")
        .url("https://storage.googleapis.com/dart-archive/channels/stable/release/3.7.2/sdk/dartsdk-windows-x64-release.zip")
        .file_name("dartsdk-windows-x64-release.zip")
        .install_dir("bin/dart")
        .strip_top("dart-sdk/")
        .kind(Kind::Zip)
        .check_file("bin/dart.exe")
        .notes("Dart SDK — optimised for client + server; use with Flutter."),
    entry("Lua")
        .version("5.4.7")
        .url("https://iweb.dl.sourceforge.net/project/luabinaries/5.4.7/Tools%20Executables/lua-5.4.7_Win64_bin.zip")
        .file_name("lua-5.4.7_Win64_bin.zip")
        .install_dir("bin/lua")
        .strip_top("")
        .kind(Kind::Zip)
        .check_file("lua54.exe")
        .notes("Lua 5.4 scripting language. Executable is lua54.exe."),
    entry("Ruby")
        .version("3.4.4")
        .url("https://github.com/oneclick/rubyinstaller2/releases/download/RubyInstaller-3.4.4-1/rubyinstaller-3.4.4-1-x64.exe")
        .file_name("rubyinstaller-3.4.4-1-x64.exe")
        .install_dir("bin/ruby")
        .kind(Kind::Exe)
        .check_file("bin/ruby.exe")
        .notes("Ruby 3.4.4 via RubyInstaller2 — silent NSIS install."),
    entry("Rust")
        .version("stable (via rustup)")
        .url("https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe")
        .file_name("rustup-init.exe")
        .install_dir("bin/rust")
        .kind(Kind::File)
        .target_file("rustup-init.exe")
        .check_file(".cargo/bin/cargo.exe")
        .notes("Rust stable toolchain — cargo + rustc land in bin/rust/.cargo/bin/.")
        .hook(Hook::RustToolchain),
    entry("Kotlin")
        .version("2.1.21")
        .url("https://github.com/JetBrains/kotlin/releases/download/v2.1.21/kotlin-compiler-2.1.21.zip")
        .file_name("kotlin-compiler-2.1.21.zip")
        .install_dir("bin/kotlin")
        .strip_top("kotlinc/")
        .kind(Kind::Zip)
        .check_file("bin/kotlinc.bat")
        .notes("Kotlin compiler — requires Java (install Java card first)."),
    entry("Haskell")
        .version("GHC 9.10.1")
        .url("https://downloads.haskell.org/ghc/9.10.1/ghc-9.10.1-x86_64-unknown-mingw32.zip")
        .file_name("ghc-9.10.1-x86_64-unknown-mingw32.zip")
        .install_dir("bin/haskell")
        .strip_top("ghc-9.10.1/")
        .kind(Kind::Zip)
        .check_file("bin/ghc.exe")
        .notes("GHC 9.10.1 — Glasgow Haskell Compiler. Large download (~570 MB)."),
    entry("Elixir")
        .version("1.18.3 (OTP 27)")
        .url("https://github.com/elixir-lang/elixir/releases/download/v1.18.3/elixir-otp-27.zip")
        .file_name("elixir-otp-27.zip")
        .install_dir("bin/elixir")
        .strip_top("")
        .kind(Kind::Zip)
        .check_file("bin/elixir.bat")
        .notes("Elixir 1.18.3 — requires Erlang OTP 27 (install Erlang card first)."),
    entry("Crystal")
        .version("1.15.1")
        .url("https://github.com/crystal-lang/crystal/releases/download/1.15.1/crystal-1.15.1-windows-x86_64-msvc-unsupported.zip")
        .file_name("crystal-1.15.1-windows-x86_64-msvc-unsupported.zip")
        .install_dir("bin/crystal")
        .strip_top("crystal-1.15.1-windows-x86_64-msvc-unsupported/")
        .kind(Kind::Zip)
        .check_file("crystal.exe")
        .notes("Crystal 1.15.1 — experimental Windows build; statically-typed Ruby-like language."),
    entry("Scala")
        .version("2.13.16")
        .url("https://downloads.lightbend.com/scala/2.13.16/scala-2.13.16.zip")
        .file_name("scala-2.13.16.zip")
        .install_dir("bin/scala")
        .strip_top("scala-2.13.16/")
        .kind(Kind::Zip)
        .check_file("bin/scala.bat")
        .notes("Scala 2.13 LTS — requires Java (install Java card first)."),
    entry("Erlang")
        .version("27.3.4 (OTP-27)")
        .url("https://github.com/erlang/otp/releases/download/OTP-27.3.4/otp_win64_27.3.4.exe")
        .file_name("otp_win64_27.3.4.exe")
        .install_dir("bin/erlang")
        .kind(Kind::Exe)
        .check_file("bin/erl.exe")
        .notes("Erlang/OTP runtime — required by RabbitMQ. Installed silently."),
    entry("RabbitMQ")
        .version("4.3.0")
        .url("https://github.com/rabbitmq/rabbitmq-server/releases/download/v4.3.0/rabbitmq-server-windows-4.3.0.zip")
        .file_name("rabbitmq-server-windows-4.3.0.zip")
        .install_dir("bin/rabbitmq")
        .strip_top("rabbitmq_server-4.3.0/")
        .kind(Kind::Zip)
        .check_file("sbin/rabbitmq-server.bat")
        .notes("AMQP message broker — AMQP :5672, management UI :15672 (guest/guest).")
        .hook(Hook::RabbitMqErlang),
    entry("MinIO")
        .version("latest")
        .url("https://dl.min.io/server/minio/release/windows-amd64/minio.exe")
        .file_name("minio.exe")
        .install_dir("bin/minio")
        .kind(Kind::File)
        .target_file("minio.exe")
        .check_file("minio.exe")
        .notes("S3-compatible object storage — API :9010, console at http://localhost:9011 (user: minioadmin).")
        .hook(Hook::MinioDataDir),
    entry("Mailpit")
        .version("1.30.0")
        .url("https://github.com/axllent/mailpit/releases/download/v1.30.0/mailpit-windows-amd64.zip")
        .file_name("mailpit-windows-amd64.zip")
        .install_dir("bin/mailpit")
        .strip_top("")
        .kind(Kind::Zip)
        .check_file("mailpit.exe")
        .notes("Email testing — SMTP :1025, web UI at http://localhost:8025."),
];

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::testutil::TempDir;

    /// The 29 names, in the order the original file lists them.
    const NAMES: [&str; 29] = [
        "Apache",
        "Nginx",
        "PHP-FPM",
        "MySQL",
        "PostgreSQL",
        "Redis",
        "phpMyAdmin",
        "pgweb",
        "Adminer",
        "Composer",
        "Node.js",
        "Python",
        "Go",
        "Java",
        "Julia",
        "Zig",
        "Dart",
        "Lua",
        "Ruby",
        "Rust",
        "Kotlin",
        "Haskell",
        "Elixir",
        "Crystal",
        "Scala",
        "Erlang",
        "RabbitMQ",
        "MinIO",
        "Mailpit",
    ];

    fn component(name: &str) -> &'static Component {
        find(name).unwrap_or_else(|| panic!("`{name}` must be in the catalogue"))
    }

    #[test]
    fn the_catalogue_holds_every_component_in_source_order() {
        let names: Vec<&str> = entries().iter().map(|entry| entry.name).collect();
        assert_eq!(names, NAMES.to_vec());
        assert_eq!(entries().len(), 29);
    }

    #[test]
    fn every_component_describes_what_to_install() {
        for entry in entries() {
            assert!(!entry.version.is_empty(), "{}: no version", entry.name);
            assert!(
                entry.url.starts_with("https://"),
                "{}: `{}` is not https",
                entry.name,
                entry.url
            );
            assert!(!entry.file_name.is_empty(), "{}: no file name", entry.name);
            assert!(
                !entry.install_dir.contains('\\'),
                "{}: install dir must use `/`",
                entry.name
            );
            assert!(
                !entry.install_dir.starts_with('/') && !entry.install_dir.contains(".."),
                "{}: install dir must be relative to the installation root",
                entry.name
            );
            // Every component has a check file, which is what makes
            // `is_installed` able to tell an installed one from a missing one.
            assert!(
                !entry.check_file.is_empty(),
                "{}: no check file",
                entry.name
            );
            if entry.kind == Kind::File {
                assert!(
                    entry.target_file.is_some(),
                    "{}: a single-file component needs a target name",
                    entry.name
                );
            }
            for variant in entry.variants {
                assert!(
                    variant.url.starts_with("https://"),
                    "{} {}: not https",
                    entry.name,
                    variant.version
                );
                assert!(!variant.file_name.is_empty());
            }
        }
    }

    #[test]
    fn no_two_components_share_a_name() {
        let mut names: Vec<&str> = entries().iter().map(|entry| entry.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "a duplicated name breaks `find`");
    }

    #[test]
    fn the_url_resolvers_are_the_two_the_original_had() {
        let with_resolvers: Vec<(&str, Resolver)> = entries()
            .iter()
            .filter_map(|entry| entry.resolver.map(|resolver| (entry.name, resolver)))
            .collect();
        assert_eq!(
            with_resolvers,
            vec![
                ("Apache", Resolver::ApacheLatest),
                ("Zig", Resolver::ZigLatest)
            ]
        );

        // Both keep a bundled fallback URL, which is what an install uses when
        // the resolver cannot reach the index.
        assert_eq!(
            component("Apache").url,
            "https://www.apachelounge.com/download/VS18/binaries/httpd-2.4.68-260827-Win64-VS18.zip"
        );
        assert_eq!(
            component("Zig").url,
            "https://ziglang.org/download/0.14.0/zig-windows-x86_64-0.14.0.zip"
        );
        assert_eq!(Resolver::ApacheLatest.as_str(), "resolveApacheLatest");
        assert_eq!(Resolver::ZigLatest.as_str(), "resolveZigLatest");
    }

    #[test]
    fn the_eleven_post_install_hooks_are_attached_to_their_components() {
        let hooked: Vec<(&str, Hook)> = entries()
            .iter()
            .filter_map(|entry| entry.hook.map(|hook| (entry.name, hook)))
            .collect();
        assert_eq!(
            hooked,
            vec![
                ("Apache", Hook::ApacheHttpdConf),
                ("PHP-FPM", Hook::PhpIni),
                ("MySQL", Hook::MariaDbDataDir),
                ("PostgreSQL", Hook::PostgresCluster),
                ("phpMyAdmin", Hook::PhpMyAdminConfig),
                ("Adminer", Hook::AdminerBlankPassword),
                ("Composer", Hook::ComposerBat),
                ("Python", Hook::PythonPip),
                ("Rust", Hook::RustToolchain),
                ("RabbitMQ", Hook::RabbitMqErlang),
                ("MinIO", Hook::MinioDataDir),
            ]
        );
        assert_eq!(hooked.len(), 11);

        // Every hook names itself, which is what the log and the dashboard use.
        for (_, hook) in hooked {
            assert!(!hook.as_str().is_empty());
        }
    }

    #[test]
    fn the_swift_gap_is_preserved() {
        // Swift is a service card in the default configuration and has an icon,
        // but no catalogue entry: the original could not install it either.
        // Asserted so a later "tidy-up" cannot claim the omission was an
        // oversight, silently adding an entry with invented coordinates.
        assert!(find("Swift").is_none());
    }

    #[test]
    fn the_fourteen_variants_are_exactly_the_originals() {
        let php = component("PHP-FPM");
        assert_eq!(
            php.variants
                .iter()
                .map(|variant| variant.version)
                .collect::<Vec<_>>(),
            vec!["7.4", "8.0", "8.1", "8.2", "8.3", "8.4", "8.5"]
        );
        // PHP's archive has no wrapper directory, at the top level or per
        // variant: the component's own prefix is empty, and every variant
        // overrides it with an empty one - which is what the original's
        // unconditional `stripTop = v.StripTop` did.
        assert_eq!(php.strip_top, Some(""));
        // `Some("")` is not the same as "unset": the original assigned the
        // variant's own prefix unconditionally, so an empty one is what stops
        // the component's prefix from being used. PHP ships no wrapper
        // directory, and says so per variant.
        assert!(
            php.variants
                .iter()
                .all(|variant| variant.strip_top == Some(""))
        );
        // The 8.4 variant shares the default's URL, which is the point: it is
        // the same build.
        assert_eq!(
            php.variant("8.4").unwrap().url,
            "https://windows.php.net/downloads/releases/php-8.4.22-nts-Win32-vs17-x64.zip"
        );
        assert_eq!(
            php.variant("8.5").unwrap().url,
            "https://downloads.php.net/~windows/releases/archives/php-8.5.7-nts-Win32-vs17-x64.zip"
        );

        let node = component("Node.js");
        assert_eq!(
            node.variants
                .iter()
                .map(|variant| variant.version)
                .collect::<Vec<_>>(),
            vec!["18", "20", "22"]
        );
        assert_eq!(
            node.variant("20").unwrap().strip_top,
            Some("node-v20.18.1-win-x64/")
        );

        let python = component("Python");
        assert_eq!(
            python
                .variants
                .iter()
                .map(|variant| variant.version)
                .collect::<Vec<_>>(),
            vec!["3.10", "3.11", "3.12", "3.13"]
        );

        let total: usize = entries().iter().map(|entry| entry.variants.len()).sum();
        assert_eq!(total, 14, "7 PHP + 3 Node + 4 Python");
    }

    #[test]
    fn only_php_node_and_python_are_multi_version() {
        let multi: Vec<&str> = entries()
            .iter()
            .filter(|entry| entry.is_multi_version())
            .map(|entry| entry.name)
            .collect();
        assert_eq!(multi, vec!["PHP-FPM", "Node.js", "Python"]);
    }

    #[test]
    fn an_unknown_version_is_an_error_naming_the_component() {
        let php = component("PHP-FPM");
        let error = php.variant("9.9").unwrap_err();
        assert_eq!(
            error.to_string(),
            "[PHP-FPM] version \"9.9\" not in catalogue"
        );
    }

    #[test]
    fn lookup_is_by_exact_name() {
        assert!(find("PHP-FPM").is_some());
        assert!(find("php").is_none(), "names are case-sensitive");
        assert!(find("Node.js").is_some());
        assert!(find("Node").is_none());
        assert!(find("").is_none());
    }

    #[test]
    fn install_directories_are_versioned_only_for_a_versioned_component() {
        let base = Path::new("/stack");

        // A single-version component ignores the version entirely - passing one
        // is not an error, it is simply not a multi-version component.
        assert_eq!(
            component("Nginx").install_dir_for(base, ""),
            base.join("bin/nginx")
        );
        assert_eq!(
            component("Nginx").install_dir_for(base, "1.28.3"),
            base.join("bin/nginx")
        );

        // A multi-version component installs beside its canonical directory.
        assert_eq!(
            component("PHP-FPM").install_dir_for(base, "8.3"),
            base.join("bin/php-8.3")
        );
        assert_eq!(
            component("PHP-FPM").install_dir_for(base, ""),
            base.join("bin/php")
        );
        assert_eq!(
            component("PHP-FPM").canonical_dir(base),
            base.join("bin/php"),
            "the canonical directory stays put"
        );
    }

    #[test]
    fn is_installed_reports_an_unknown_component_as_installed() {
        let temp = TempDir::new();
        // Deliberate: the dashboard asks about every card it draws, and a card
        // it cannot install must not offer to install anything.
        assert!(is_installed("Swift", temp.path()));
        assert!(is_installed("", temp.path()));
        assert!(is_installed("Not A Component", temp.path()));
    }

    #[test]
    fn is_installed_uses_the_check_file() {
        let temp = TempDir::new();

        // Apache's check file is a nested path, and MySQL's is a *directory*
        // inside the installation.
        assert!(!is_installed("Apache", temp.path()));
        assert!(!is_installed("MySQL", temp.path()));

        let conf = temp.path().join("bin/apache/conf");
        fs::create_dir_all(&conf).unwrap();
        fs::write(conf.join("httpd.conf"), b"# config").unwrap();
        assert!(is_installed("Apache", temp.path()));

        fs::create_dir_all(temp.path().join("bin/mysql/data/mysql")).unwrap();
        assert!(is_installed("MySQL", temp.path()));

        // A file is not enough where a directory is expected? The original
        // used `os.Stat`, which reports a file as present, so this stays.
        let temp2 = TempDir::new();
        fs::create_dir_all(temp2.path().join("bin/mysql/data")).unwrap();
        fs::write(
            temp2.path().join("bin/mysql/data/mysql"),
            b"not a directory",
        )
        .unwrap();
        assert!(is_installed("MySQL", temp2.path()));
    }

    #[test]
    fn probe_answers_false_for_an_empty_check_file() {
        let temp = TempDir::new();
        assert!(!probe("", temp.path()));
        assert!(!probe("php-cgi.exe", temp.path()));

        fs::write(temp.path().join("php-cgi.exe"), b"MZ").unwrap();
        assert!(probe("php-cgi.exe", temp.path()));
    }

    #[test]
    fn a_plan_carries_the_components_own_coordinates() {
        let temp = TempDir::new();
        let nginx = plan(component("Nginx"), "", temp.path(), None).unwrap();

        assert_eq!(nginx.name, "Nginx");
        assert_eq!(nginx.version, "1.28.3 stable");
        assert_eq!(nginx.url, "https://nginx.org/download/nginx-1.28.3.zip");
        assert_eq!(nginx.file_name, "nginx-1.28.3.zip");
        assert_eq!(nginx.strip_top.as_deref(), Some("nginx-1.28.3/"));
        assert_eq!(nginx.kind, Kind::Zip);
        assert_eq!(nginx.install_dir, temp.path().join("bin/nginx"));
        assert_eq!(nginx.canonical_dir, nginx.install_dir);
        assert!(!nginx.is_versioned());
        assert_eq!(nginx.hook, None);
        assert!(!nginx.is_installed());
        // A single-file component's target defaults to the download's name.
        let redis = plan(component("Redis"), "", temp.path(), None).unwrap();
        assert_eq!(redis.target_file, "Redis-x64-5.0.14.1.zip");
    }

    #[test]
    fn a_plan_for_a_resolved_component_uses_the_resolved_coordinates() {
        let temp = TempDir::new();
        let resolved = Resolved {
            url: "https://www.apachelounge.com/download/VS18/binaries/httpd-2.4.70-269999-Win64-VS18.zip".to_owned(),
            file_name: "httpd-2.4.70-269999-Win64-VS18.zip".to_owned(),
            strip_top: "Apache24/".to_owned(),
            version: "2.4.70 (VS18, win64)".to_owned(),
        };

        let planned = plan(component("Apache"), "", temp.path(), Some(&resolved)).unwrap();
        assert_eq!(planned.url, resolved.url);
        assert_eq!(planned.file_name, resolved.file_name);
        assert_eq!(planned.version, "2.4.70 (VS18, win64)");
        assert_eq!(planned.strip_top.as_deref(), Some("Apache24/"));

        // Without a resolved build - the resolver failed, or was never asked -
        // the bundled URL is used, which is the documented fallback.
        let fallback = plan(component("Apache"), "", temp.path(), None).unwrap();
        assert_eq!(fallback.url, component("Apache").url);
        assert_eq!(fallback.version, "2.4.68 (VS18, win64)");
    }

    #[test]
    fn a_resolved_version_wins_over_the_bundled_one() {
        let temp = TempDir::new();
        let resolved = Resolved {
            url: "https://ziglang.org/download/0.15.1/zig-x86_64-windows-0.15.1.zip".to_owned(),
            file_name: "zig-x86_64-windows-0.15.1.zip".to_owned(),
            strip_top: "zig-x86_64-windows-0.15.1/".to_owned(),
            version: "0.15.1".to_owned(),
        };
        // Zig's table entry has no variants, so its baked-in strip prefix -
        // which names 0.14.0 - must not survive a resolver's answer.
        let planned = plan(component("Zig"), "", temp.path(), Some(&resolved)).unwrap();
        assert_eq!(planned.version, "0.15.1");
        assert_eq!(
            planned.strip_top.as_deref(),
            Some("zig-x86_64-windows-0.15.1/")
        );
        assert_eq!(planned.file_name, "zig-x86_64-windows-0.15.1.zip");
    }

    #[test]
    fn a_plan_for_a_variant_overrides_everything() {
        let temp = TempDir::new();
        let planned = plan(component("PHP-FPM"), "8.3", temp.path(), None).unwrap();

        assert_eq!(planned.version, "8.3");
        assert_eq!(
            planned.url,
            "https://windows.php.net/downloads/releases/archives/php-8.3.15-nts-Win32-vs16-x64.zip"
        );
        assert_eq!(planned.file_name, "php-8.3.15-nts-Win32-vs16-x64.zip");
        assert_eq!(
            planned.notes, "PHP 8.3 — VS16 (VS2019) runtime.",
            "the variant's note replaces the component's"
        );
        assert_eq!(planned.install_dir, temp.path().join("bin/php-8.3"));
        assert_eq!(planned.canonical_dir, temp.path().join("bin/php"));
        assert!(planned.is_versioned());
        assert_eq!(planned.hook, Some(Hook::PhpIni));
    }

    #[test]
    fn a_variant_without_a_note_keeps_the_components() {
        // The shipped table has no such variant, so the rule is tested on one
        // built here - it is behaviour, not an accident of this data.
        const VARIANTS: &[Variant] = &[Variant {
            version: "9",
            url: "https://example.com/thing-9.zip",
            file_name: "thing-9.zip",
            strip_top: None,
            notes: "",
        }];
        let synthetic = entry("Thing")
            .version("9 default")
            .url("https://example.com/thing.zip")
            .file_name("thing.zip")
            .install_dir("bin/thing")
            .kind(Kind::Zip)
            .check_file("thing.exe")
            .notes("The component's own note.")
            .variants(VARIANTS);

        let temp = TempDir::new();
        let planned = plan(&synthetic, "9", temp.path(), None).unwrap();
        assert_eq!(planned.notes, "The component's own note.");
        assert_eq!(
            planned.version, "9",
            "the version label is still the variant's"
        );

        // And the default plan for the same component keeps its own URL.
        let default_plan = plan(&synthetic, "", temp.path(), None).unwrap();
        assert_eq!(default_plan.url, "https://example.com/thing.zip");
        assert_eq!(default_plan.notes, "The component's own note.");
        assert_eq!(default_plan.install_dir, temp.path().join("bin/thing"));
        assert!(!default_plan.is_versioned());
    }

    #[test]
    fn an_empty_strip_prefix_is_the_same_as_no_prefix() {
        let temp = TempDir::new();
        // Redis, Mailpit, Elixir, Lua and friends are listed with an empty
        // StripTop; a plan says "nothing to strip" rather than "strip nothing".
        for name in ["Redis", "Mailpit", "Elixir", "Lua", "PHP-FPM", "Python"] {
            let planned = plan(component(name), "", temp.path(), None).unwrap();
            assert_eq!(planned.strip_top, None, "{name}");
        }
        // A real prefix survives.
        for (name, prefix) in [
            ("Apache", "Apache24/"),
            ("MySQL", "mariadb-11.4.10-winx64/"),
            ("PostgreSQL", "pgsql/"),
            ("Go", "go/"),
            ("Java", "jdk-21.0.10+7/"),
            ("Dart", "dart-sdk/"),
            ("Kotlin", "kotlinc/"),
            ("Haskell", "ghc-9.10.1/"),
            ("Scala", "scala-2.13.16/"),
            ("Crystal", "crystal-1.15.1-windows-x86_64-msvc-unsupported/"),
            ("RabbitMQ", "rabbitmq_server-4.3.0/"),
            ("Julia", "julia-1.11.5/"),
            ("phpMyAdmin", "phpMyAdmin-5.2.3-all-languages/"),
        ] {
            assert_eq!(
                plan(component(name), "", temp.path(), None)
                    .unwrap()
                    .strip_top
                    .as_deref(),
                Some(prefix),
                "{name}"
            );
        }
    }

    #[test]
    fn the_three_kinds_are_used_as_the_original_used_them() {
        let by_kind = |kind: Kind| -> Vec<&'static str> {
            entries()
                .iter()
                .filter(|entry| entry.kind == kind)
                .map(|entry| entry.name)
                .collect()
        };

        assert_eq!(by_kind(Kind::Exe), vec!["Ruby", "Erlang"]);
        assert_eq!(
            by_kind(Kind::File),
            vec!["Adminer", "Composer", "Rust", "MinIO"]
        );
        assert_eq!(by_kind(Kind::Zip).len(), 23);
        assert_eq!(Kind::Zip.as_str(), "zip");
        assert_eq!(Kind::Exe.as_str(), "exe");
        assert_eq!(Kind::File.as_str(), "file");

        // The two silent installers take `/S /D=<dir>`, and the four single
        // files name their target.
        assert_eq!(component("Ruby").target_file, None);
        assert_eq!(component("Adminer").target_file, Some("index.php"));
        assert_eq!(component("Composer").target_file, Some("composer.phar"));
        assert_eq!(component("Rust").target_file, Some("rustup-init.exe"));
        assert_eq!(component("MinIO").target_file, Some("minio.exe"));
    }

    #[test]
    fn notes_are_preserved_verbatim() {
        // Including the stale one: the PHP entry installs 8.4.22 while its note
        // still describes 7.4 and the VC15 runtime. Rewriting it would change
        // what the dashboard shows, so it stays as the original had it.
        assert_eq!(
            component("PHP-FPM").notes,
            "Requires VC++ 2015-2022 Redistributable x64."
        );
        assert_eq!(
            component("PHP-FPM").variant("7.4").unwrap().notes,
            "PHP 7.4 — needs VC15 (VS2017) runtime."
        );
        assert_eq!(
            component("Apache").notes,
            "Apache Lounge build — requires VC++ 2015-2022 Redistributable."
        );
        assert_eq!(
            component("MySQL").notes,
            "MariaDB — MySQL-compatible drop-in."
        );
        assert_eq!(
            component("phpMyAdmin").notes,
            "Served by Apache — make sure Apache's DocumentRoot points at {base}/www."
        );
        assert_eq!(
            component("Composer").notes,
            "Composer — PHP dependency manager. Installs into bin/php alongside php.exe."
        );
        assert_eq!(
            component("MinIO").notes,
            "S3-compatible object storage — API :9010, console at http://localhost:9011 (user: minioadmin)."
        );
        assert_eq!(
            component("RabbitMQ").notes,
            "AMQP message broker — AMQP :5672, management UI :15672 (guest/guest)."
        );
        assert_eq!(
            component("Mailpit").notes,
            "Email testing — SMTP :1025, web UI at http://localhost:8025."
        );
    }

    #[test]
    fn a_plan_for_an_unknown_component_is_refused() {
        let temp = TempDir::new();
        let error = plan_for("Swift", "", temp.path(), None).unwrap_err();
        assert_eq!(
            error.to_string(),
            "no download info registered for \"Swift\""
        );
    }

    #[test]
    fn plan_for_finds_the_component_by_name() {
        let temp = TempDir::new();
        let planned = plan_for("Mailpit", "", temp.path(), None).unwrap();
        assert_eq!(planned.name, "Mailpit");
        assert_eq!(planned.install_dir, temp.path().join("bin/mailpit"));
    }

    #[test]
    fn goldens_for_the_entries_whose_coordinates_are_easiest_to_break() {
        // A small number of exact values, on top of the structural tests
        // above: these are the ones a hand-edit is most likely to damage.
        let temp = TempDir::new();

        let mariadb = plan(component("MySQL"), "", temp.path(), None).unwrap();
        assert_eq!(
            mariadb.url,
            "https://archive.mariadb.org/mariadb-11.4.10/winx64-packages/mariadb-11.4.10-winx64.zip"
        );
        assert_eq!(mariadb.version, "MariaDB 11.4.10 LTS");
        assert_eq!(mariadb.install_dir, temp.path().join("bin/mysql"));
        assert_eq!(mariadb.check_file, "data/mysql");
        assert_eq!(mariadb.hook, Some(Hook::MariaDbDataDir));

        let postgres = plan(component("PostgreSQL"), "", temp.path(), None).unwrap();
        assert_eq!(
            postgres.url,
            "https://get.enterprisedb.com/postgresql/postgresql-16.6-1-windows-x64-binaries.zip"
        );
        assert_eq!(postgres.strip_top.as_deref(), Some("pgsql/"));
        assert_eq!(postgres.check_file, "data/PG_VERSION");

        let adminer = plan(component("Adminer"), "", temp.path(), None).unwrap();
        assert_eq!(
            adminer.url,
            "https://github.com/vrana/adminer/releases/download/v5.4.2/adminer-5.4.2-en.php"
        );
        assert_eq!(adminer.file_name, "adminer-5.4.2-en.php");
        assert_eq!(adminer.target_file, "index.php");
        assert_eq!(adminer.install_dir, temp.path().join("www/adminer"));

        let composer = plan(component("Composer"), "", temp.path(), None).unwrap();
        assert_eq!(composer.url, "https://getcomposer.org/composer-stable.phar");
        assert_eq!(composer.target_file, "composer.phar");
        assert_eq!(composer.check_file, "composer.bat");

        // Two components share an install directory: Composer installs into
        // PHP's, which is why its check file is `composer.bat` and not a
        // version of PHP.
        assert_eq!(
            component("Composer").install_dir,
            component("PHP-FPM").install_dir
        );
    }
}
