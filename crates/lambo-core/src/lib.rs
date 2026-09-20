//! lambo-core - the engine of **Lambo PHP**, the native local PHP
//! development environment for Windows, Linux and macOS.
//!
//! All business logic lives in this crate. The CLI today (and the TUI/GUI
//! later) are thin shells over it and contain no logic of their own; that
//! layering rule is what keeps every interface consistent and everything
//! testable. See `docs/architecture.md` for the full map.
//!
//! # Windows is a first-class platform
//!
//! Nothing in this crate assumes a Unix host: no `/bin/sh`, no `/etc/hosts`,
//! no symlinks, no `kill -9`, no `/usr/local/bin`. Process control, path
//! handling, runtime discovery and service supervision all go through the
//! cross-platform seams documented in [`platform`], [`process`], [`paths`]
//! and [`runtime`]. Where an operating system needs something specific it
//! lives behind a `#[cfg]` gate with an implementation for *both* families,
//! never only for Unix.
//!
//! # Module map
//!
//! Configuration and layout
//! - [`config`] - global configuration (`$LAMBO_HOME/config/lambo.yml`)
//! - [`lambofile`] - per-project `lambo.yml` files
//! - [`panel`] - the control panel's installation state (`config.json`)
//! - [`paths`] - the filesystem layout (`LAMBO_HOME`)
//! - [`platform`] - OS/architecture facts and platform-specific defaults
//! - [`migration`] - upgrades from legacy king-PHP installations
//!
//! Runtimes and downloads
//! - [`archive`] - safe `.zip` / `.tar.gz` extraction
//! - [`catalog`] - the download catalogue (URLs and checksums)
//! - [`catalog_panel`] - the control panel's own list of 29 installable components
//! - [`download`] - HTTPS-only fetching with checksum verification
//! - [`download_cache`] - the shared `downloads/` cache: adoption, validation, sweep
//! - [`installer`] - the install workflow: download, unpack, mirror, activate
//! - [`runtime`] - installed runtime registry (PHP, Apache, MariaDB, …)
//! - [`php`] - PHP runtime management, `php.ini` generation
//! - [`postinstall`] - what runs after an archive is unpacked (all eleven hooks)
//! - [`vhost`] - virtual hosts: the `hosts` file, Apache vhosts, nginx sites
//! - [`zombies`] - the startup sweep for children a previous run left behind
//! - [`sha256`] - the checksum primitive used by [`download`]
//!
//! Services
//! - [`apache`] - the Apache httpd service
//! - [`database`] - the MariaDB/MySQL service
//! - [`dbui`] - the bundled database manager (Adminer)
//! - [`http`] - the tiny HTTP client used for health checks
//! - [`logs`] - log locations and tailing
//! - [`pathenv`] - the user `PATH`: which of Lambo's directories belong on it
//! - [`port`] - TCP port availability and conflict diagnosis
//! - [`process`] - cross-platform process supervision
//! - [`service`] - the service engine: one supervised process, its start, its
//!   output, its stop and its PostgreSQL path
//! - [`session`] - service orchestration (`lambo up` / `down` / `restart`)
//! - [`stack`] - the installation's services as one stack: the cards, the
//!   essential passes, Stop All, auto-start and the startup sweep
//! - [`state`] - which services Lambo owns right now
//! - [`tray`] - the tray icon, its menu, and starting with Windows
//!
//! Projects
//! - [`browser`] - opening URLs in the default browser
//! - [`detect`] - evidence-based framework detection
//! - [`envfile`] - non-destructive `.env` handling
//! - [`naming`] - slug, database-name and URL helpers
//! - [`project`] - the resolved view of one project
//! - [`workspace`] - named collections of projects
//!
//! Support
//! - [`doctor`] - environment diagnostics and automated repair
//! - [`error`] - the unified error type
//! - [`version`] - PHP version specifications (`stable`, `8.4`, `^8.3`, …)

#![forbid(unsafe_code)]

pub mod apache;
pub mod archive;
pub mod browser;
pub mod catalog;
pub mod catalog_panel;
pub mod config;
pub mod console;
pub mod database;
pub mod dbui;
pub mod detect;
pub mod doctor;
pub mod download;
pub mod download_cache;
pub mod envfile;
pub mod error;
pub mod frameworks;
pub mod fsx;
pub mod http;
pub mod installer;
pub mod lambofile;
pub mod logs;
pub mod migration;
pub mod naming;
pub mod panel;
pub mod pathenv;
pub mod paths;
pub mod php;
pub mod platform;
pub mod port;
pub mod postinstall;
pub mod process;
pub mod project;
pub mod runtime;
pub mod secret;
pub mod serde_defaults;
pub mod service;
pub mod session;
pub mod sha256;
pub mod sources;
pub mod stack;
pub mod tray;
pub mod ui_state;
pub mod vendor;
pub mod version;
pub mod vhost;
pub mod workspace;
pub mod zombies;

pub use error::{Error, Result};

/// Product name shown in output, documentation and generated files.
pub const PRODUCT: &str = "Lambo PHP";

/// Name of the command-line executable (without platform suffix).
pub const BIN_NAME: &str = "lambo";

/// Crate-internal YAML helpers. The YAML backend is deliberately hidden
/// behind this module so it stays replaceable (docs/adr/0004-yaml-backend.md).
pub(crate) mod yaml;

/// Test utilities shared by the module unit tests.
///
/// Compiled for this crate's own tests, and for an interface crate's when it
/// asks for the `testutil` feature - the panel's unit tests want the same
/// `TempDir` and fixture writers the engine's do, and one copy of them is
/// better than two.
#[cfg(any(test, feature = "testutil"))]
pub mod testutil;
