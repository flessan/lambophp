//! A project on disk, and what Lambo will do with it.
//!
//! [`Project`] is the object every command works from. It bundles three things
//! that must agree with each other:
//!
//! 1. where the project lives,
//! 2. what [`detect`] concluded about it (the evidence),
//! 3. what `lambo.yml` says - the user's word, which always wins.
//!
//! Keeping them together is what makes `lambo init` and `lambo up` consistent:
//! `init` writes the detection into `lambo.yml`, and `up` reads it back rather
//! than re-guessing. A project whose file disagrees with its contents is the
//! user's decision, and Lambo respects it.
//!
//! Nothing here holds platform-absolute paths: `document_root` is relative to
//! the project, ports are numbers, and the Lambo home is passed in by the
//! caller. A `lambo.yml` written on Windows is valid on Linux and vice versa.

use std::path::{Path, PathBuf};

use crate::config::{Config, DatabaseKind, ServerKind};
use crate::detect::{self, Detection, Framework};
use crate::envfile;
use crate::error::{Error, Result};
use crate::lambofile::{self, DatabaseSettings, Lambofile, ServerSettings, Source};
use crate::naming;
use crate::platform::Os;
use crate::version::VersionSpec;

/// A project, its configuration and the evidence behind it.
#[derive(Debug, Clone)]
pub struct Project {
    /// Project directory (the one holding `lambo.yml`).
    pub root: PathBuf,
    /// Effective configuration.
    pub file: Lambofile,
    /// Where the configuration came from.
    pub source: Source,
    /// What the directory looks like, for reporting and for `init`.
    pub detection: Detection,
}

/// Options for `lambo init`.
#[derive(Debug, Clone, Default)]
pub struct InitOptions {
    /// Override the detected project name.
    pub name: Option<String>,
    /// Override the PHP requirement.
    pub php: Option<VersionSpec>,
    /// Override the HTTP port.
    pub port: Option<u16>,
    /// Override the database engine (including [`DatabaseKind::None`]).
    pub database: Option<DatabaseKind>,
    /// Overwrite an existing `lambo.yml`.
    pub force: bool,
    /// Detect and report without writing anything (`lambo init --dry-run`).
    pub dry_run: bool,
}

/// What `lambo init` did.
#[derive(Debug, Clone)]
pub struct InitReport {
    /// Where the project file is (or would be).
    pub path: PathBuf,
    /// What was detected.
    pub detection: Detection,
    /// Whether a new file was written.
    pub written: bool,
    /// A `kingphp.yml` that was converted.
    pub migrated_from: Option<PathBuf>,
    /// An existing file that was left untouched.
    pub existing: bool,
}

impl Project {
    /// Finds the nearest project at or above `start` and loads it.
    pub fn load(start: &Path) -> Result<Self> {
        let (root, loaded) = Lambofile::find(start)?.ok_or_else(|| Error::NotAProject {
            dir: start.to_path_buf(),
        })?;
        let detection = detect::detect(&root);
        Ok(Self {
            root,
            file: loaded.file,
            source: loaded.source,
            detection,
        })
    }

    /// Loads a project from an exact directory, without searching upwards.
    pub fn load_exact(dir: &Path) -> Result<Self> {
        let loaded = Lambofile::load_with_source(dir)?.ok_or_else(|| Error::NotAProject {
            dir: dir.to_path_buf(),
        })?;
        let detection = detect::detect(dir);
        Ok(Self {
            root: dir.to_path_buf(),
            file: loaded.file,
            source: loaded.source,
            detection,
        })
    }

    /// The project name.
    pub fn name(&self) -> String {
        self.file.project_name(&self.root)
    }

    /// Absolute document root.
    pub fn document_root(&self) -> PathBuf {
        self.file.document_root(&self.root)
    }

    /// The PHP requirement, resolved against the global default when the
    /// project does not pin one.
    pub fn php_spec(&self, config: &Config) -> VersionSpec {
        match &self.file.php {
            VersionSpec::Stable | VersionSpec::Latest | VersionSpec::Nightly => {
                config.php.default.clone()
            }
            spec => spec.clone(),
        }
    }

    /// Which server fronts this project.
    pub fn server_kind(&self, config: &Config) -> ServerKind {
        self.file.server.kind.unwrap_or(config.server.kind)
    }

    /// The HTTP port.
    pub fn http_port(&self, config: &Config) -> u16 {
        self.file.http_port(config)
    }

    /// The local URL of the project.
    pub fn url(&self, config: &Config) -> String {
        naming::local_url(self.http_port(config))
    }

    /// The database engine for this project.
    ///
    /// An absent `database.kind` inherits the global engine; an explicit
    /// `database.kind: none` turns provisioning off for this project.
    pub fn database_kind(&self, config: &Config) -> DatabaseKind {
        self.file.database.kind.unwrap_or(config.database.kind)
    }

    /// The database name for this project.
    pub fn database_name(&self) -> String {
        self.file.database_name(&self.root)
    }

    /// Whether the browser opens after `lambo up`.
    pub fn opens_browser(&self, config: &Config) -> bool {
        self.file.should_open_browser(config)
    }

    /// Checks the project configuration and reports every problem found.
    ///
    /// `lambo up` calls this before starting anything, so a broken
    /// configuration is reported as a configuration problem rather than as a
    /// mysterious server failure.
    pub fn validate(&self) -> Result<()> {
        self.file.validate(&self.root)?;

        let document_root = self.document_root();
        if !document_root.is_dir() {
            return Err(Error::InvalidProjectFile {
                path: Lambofile::path_in(&self.root),
                reason: format!("document root `{}` does not exist", document_root.display()),
            });
        }

        // A project served by Apache that needs PHP must have an entry point;
        // otherwise the browser gets a directory listing and the user has no
        // idea what went wrong.
        if !has_entry_point(&document_root) {
            return Err(Error::InvalidProjectFile {
                path: Lambofile::path_in(&self.root),
                reason: format!(
                    "document root `{}` has no index.php or index.html",
                    document_root.display()
                ),
            });
        }

        Ok(())
    }

    /// The `.env` keys this project needs.
    pub fn env_keys(&self, config: &Config, password: &str) -> Vec<(String, String)> {
        let kind = self.database_kind(config);
        if !kind.is_enabled() {
            return Vec::new();
        }
        let host = "127.0.0.1";
        let port = self.file.database_port(config);
        let database = self.database_name();
        let user = config.database.username.as_str();

        let mut keys = match self.detection.framework {
            Framework::Laravel => {
                envfile::laravel_database_keys(&database, user, password, host, port)
            }
            _ => envfile::generic_database_keys(&database, user, password, host, port),
        };
        // Whatever the project file asks for in `env:` wins over the defaults.
        for (key, value) in &self.file.env {
            keys.retain(|(existing, _)| existing != key);
            keys.push((key.clone(), value.clone()));
        }
        keys
    }
}

/// Whether a document root can actually serve something.
pub fn has_entry_point(document_root: &Path) -> bool {
    ["index.php", "index.html", "index.htm"]
        .iter()
        .any(|name| document_root.join(name).is_file())
}

/// Initializes a project: detect, then write `lambo.yml`.
///
/// An existing `lambo.yml` is left alone unless `force` is set - overwriting a
/// file the user has edited would be destructive. A `kingphp.yml` is migrated
/// instead of being overwritten, and the migration is reported so the user
/// knows what happened.
pub fn init(dir: &Path, config: &Config, options: &InitOptions, os: Os) -> Result<InitReport> {
    let path = Lambofile::path_in(dir);
    let detection = detect::detect(dir);

    // A legacy file is converted rather than replaced.
    let legacy = dir.join(lambofile::LEGACY_FILE_NAME);
    if !path.is_file() && legacy.is_file() {
        // `migrate_legacy` reports the file it wrote; the report needs to name
        // the file it came from.
        let migrated = Lambofile::migrate_legacy(dir)?.is_some();
        let loaded = Lambofile::load_with_source(dir)?;
        return Ok(InitReport {
            path,
            detection,
            written: false,
            migrated_from: migrated.then(|| legacy.clone()),
            existing: loaded.is_some(),
        });
    }

    if path.is_file() && !options.force {
        return Ok(InitReport {
            path,
            detection,
            written: false,
            migrated_from: None,
            existing: true,
        });
    }

    let file = lambofile_for(dir, config, &detection, options, os);
    file.validate(dir)?;
    if !options.dry_run {
        file.save(dir)?;
    }

    Ok(InitReport {
        path,
        detection,
        written: !options.dry_run,
        migrated_from: None,
        existing: false,
    })
}

/// Builds the `lambo.yml` for a detected project.
pub fn lambofile_for(
    dir: &Path,
    config: &Config,
    detection: &Detection,
    options: &InitOptions,
    os: Os,
) -> Lambofile {
    let name = options
        .name
        .clone()
        .unwrap_or_else(|| project_name_from_dir(dir));

    // A project that declares a PHP requirement gets it; a broken or absent
    // one falls back to the configured default.
    let php = options
        .php
        .clone()
        .or_else(|| {
            detection
                .php_requirement
                .as_deref()
                .and_then(|requirement| requirement.parse::<VersionSpec>().ok())
        })
        .unwrap_or_else(|| config.php.default.clone());

    let database_kind = match options.database {
        Some(kind) => kind,
        None if detection.needs_database => config.database.kind,
        None => DatabaseKind::None,
    };

    let _ = os;
    Lambofile {
        name: Some(name.clone()),
        php,
        server: ServerSettings {
            // Pinning the server makes the project reproducible; leaving it out
            // would inherit whatever this machine happens to prefer. The port
            // stays unset unless the user asked for one, so the project follows
            // the global default.
            kind: Some(config.server.kind),
            port: options.port,
            https_port: None,
            document_root: detection.document_root.to_owned(),
        },
        database: DatabaseSettings {
            kind: Some(database_kind),
            port: None,
            name: database_kind
                .is_enabled()
                .then(|| naming::database_name(&name)),
        },
        ..Lambofile::default()
    }
}

/// The project name taken from its directory.
fn project_name_from_dir(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "app".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{self, TempDir};

    /// A project directory with the given files.
    fn project(temp: &TempDir, files: &[(&str, &str)], dirs: &[&str]) -> PathBuf {
        let root = temp.join("shop");
        std::fs::create_dir_all(&root).unwrap();
        for directory in dirs {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        for (relative, contents) in files {
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, contents).unwrap();
        }
        root
    }

    fn laravel(temp: &TempDir) -> PathBuf {
        project(
            temp,
            &[
                ("artisan", "#!/usr/bin/env php\n"),
                (
                    "composer.json",
                    r#"{"require":{"php":"^8.3","laravel/framework":"^11.0"}}"#,
                ),
                ("public/index.php", "<?php\n"),
                (
                    ".env.example",
                    "APP_NAME=Laravel\nDB_CONNECTION=mysql\nDB_HOST=127.0.0.1\n",
                ),
            ],
            &["public"],
        )
    }

    #[test]
    fn init_writes_a_lambofile_that_matches_what_was_detected() {
        let temp = TempDir::new();
        let config = Config::default();
        let root = laravel(&temp);

        let report = init(&root, &config, &InitOptions::default(), Os::host()).unwrap();
        assert!(report.written);
        assert!(!report.existing);
        assert_eq!(report.detection.framework, Framework::Laravel);
        assert_eq!(report.path, root.join("lambo.yml"));

        let written = std::fs::read_to_string(&report.path).unwrap();
        assert!(written.contains("document_root: public"), "{written}");
        assert!(written.contains("php: ^8.3"), "{written}");
        assert!(written.contains("name: shop"), "{written}");
        assert!(written.contains("kind: mariadb"), "{written}");
        assert!(written.contains("name: shop"), "{written}");
        assert!(
            !written.contains("C:\\") && !written.contains("/tmp"),
            "no absolute paths: {written}"
        );

        let reloaded = Project::load(&root).unwrap();
        assert_eq!(reloaded.name(), "shop");
        assert_eq!(reloaded.document_root(), root.join("public"));
        assert_eq!(reloaded.php_spec(&config).to_string(), "^8.3");
        assert!(reloaded.validate().is_ok());
    }

    #[test]
    fn plain_php_is_served_from_its_own_directory_without_a_database() {
        let temp = TempDir::new();
        let config = Config::default();
        let root = project(&temp, &[("index.php", "<?php echo 'hi';\n")], &[]);

        let report = init(&root, &config, &InitOptions::default(), Os::host()).unwrap();
        let written = std::fs::read_to_string(&report.path).unwrap();
        assert!(written.contains("document_root: ."), "{written}");
        assert!(
            written.contains("kind: none"),
            "plain PHP must not be given a database: {written}"
        );

        let loaded = Project::load(&root).unwrap();
        assert_eq!(loaded.database_kind(&config), DatabaseKind::None);
        assert!(loaded.env_keys(&config, "pw").is_empty());
    }

    #[test]
    fn an_existing_lambofile_is_never_overwritten_without_force() {
        let temp = TempDir::new();
        let config = Config::default();
        let root = laravel(&temp);
        std::fs::write(root.join("lambo.yml"), "name: hand-written\n").unwrap();

        let report = init(&root, &config, &InitOptions::default(), Os::host()).unwrap();
        assert!(!report.written);
        assert!(report.existing);
        assert_eq!(
            std::fs::read_to_string(root.join("lambo.yml")).unwrap(),
            "name: hand-written\n"
        );

        let forced = InitOptions {
            force: true,
            ..Default::default()
        };
        let report = init(&root, &config, &forced, Os::host()).unwrap();
        assert!(report.written);
        assert!(
            std::fs::read_to_string(root.join("lambo.yml"))
                .unwrap()
                .contains("document_root: public")
        );
    }

    #[test]
    fn dry_run_reports_without_writing() {
        let temp = TempDir::new();
        let config = Config::default();
        let root = laravel(&temp);

        let options = InitOptions {
            dry_run: true,
            ..Default::default()
        };
        let report = init(&root, &config, &options, Os::host()).unwrap();
        assert!(!report.written);
        assert_eq!(report.detection.framework, Framework::Laravel);
        assert!(!root.join("lambo.yml").exists(), "a dry run must not write");
    }

    #[test]
    fn options_override_what_was_detected() {
        let temp = TempDir::new();
        let config = Config::default();
        let root = laravel(&temp);

        let options = InitOptions {
            name: Some("storefront".to_owned()),
            php: Some("8.2".parse().unwrap()),
            port: Some(9000),
            database: Some(DatabaseKind::Mysql),
            ..Default::default()
        };
        let report = init(&root, &config, &options, Os::host()).unwrap();
        let loaded = Project::load(&root).unwrap();

        assert_eq!(loaded.name(), "storefront");
        // `8.2` is a minor-version requirement, which Lambo records as `~8.2`.
        assert_eq!(loaded.php_spec(&config).to_string(), "~8.2");
        assert_eq!(loaded.http_port(&config), 9000);
        assert_eq!(loaded.database_kind(&config), DatabaseKind::Mysql);
        assert_eq!(loaded.database_name(), "storefront");
        assert!(report.written);
    }

    #[test]
    fn a_legacy_project_file_is_migrated_not_clobbered() {
        let temp = TempDir::new();
        let config = Config::default();
        let root = laravel(&temp);
        // The legacy schema is flat: `server: apache` is a scalar and
        // `document_root` sits at the top level.
        std::fs::write(
            root.join("kingphp.yml"),
            "name: shop\nphp: '8.2'\nserver: apache\ndatabase: mariadb\ndocument_root: public\n",
        )
        .unwrap();

        let report = init(&root, &config, &InitOptions::default(), Os::host()).unwrap();
        assert!(!report.written);
        assert_eq!(report.migrated_from, Some(root.join("kingphp.yml")));
        assert!(root.join("lambo.yml").is_file());
        assert!(
            root.join("kingphp.yml").is_file(),
            "the legacy file is left for the user to remove"
        );
        assert_eq!(Project::load(&root).unwrap().source, Source::Current);
    }

    #[test]
    fn loading_outside_a_project_says_so() {
        let temp = TempDir::new();
        let empty = temp.join("empty");
        std::fs::create_dir_all(&empty).unwrap();

        let error = Project::load(&empty).unwrap_err();
        assert!(matches!(error, Error::NotAProject { .. }), "{error:?}");
        assert!(
            error
                .details()
                .iter()
                .any(|line| line.contains("lambo init")),
            "{:?}",
            error.details()
        );
    }

    #[test]
    fn a_missing_document_root_is_reported_before_anything_starts() {
        let temp = TempDir::new();
        let config = Config::default();
        let root = laravel(&temp);
        init(&root, &config, &InitOptions::default(), Os::host()).unwrap();
        std::fs::remove_dir_all(root.join("public")).unwrap();

        let error = Project::load(&root).unwrap().validate().unwrap_err();
        let message = error.to_string();
        assert!(message.contains("document root"), "{message}");
        assert!(message.contains("does not exist"), "{message}");
    }

    #[test]
    fn a_document_root_with_nothing_to_serve_is_reported() {
        let temp = TempDir::new();
        let config = Config::default();
        let root = laravel(&temp);
        init(&root, &config, &InitOptions::default(), Os::host()).unwrap();
        std::fs::remove_file(root.join("public/index.php")).unwrap();

        let error = Project::load(&root).unwrap().validate().unwrap_err();
        assert!(error.to_string().contains("no index.php"), "{error}");
    }

    #[test]
    fn an_invalid_database_name_is_rejected_at_validation_time() {
        let temp = TempDir::new();
        let config = Config::default();
        let root = laravel(&temp);
        init(&root, &config, &InitOptions::default(), Os::host()).unwrap();

        let mut project = Project::load(&root).unwrap();
        project.file.database.name = Some("My DB".to_owned());
        let error = project.validate().unwrap_err();
        let message = error.to_string();
        assert!(message.contains("database.name"), "{message}");
        assert!(
            message.contains("not a valid MySQL identifier"),
            "{message}"
        );
    }

    #[test]
    fn laravel_gets_laravel_keys_and_other_projects_get_generic_ones() {
        let temp = TempDir::new();
        let config = Config::default();
        let root = laravel(&temp);
        init(&root, &config, &InitOptions::default(), Os::host()).unwrap();
        let project = Project::load(&root).unwrap();

        let keys = project.env_keys(&config, "s3cret");
        let names: Vec<&str> = keys.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(
            names,
            [
                "DB_CONNECTION",
                "DB_HOST",
                "DB_PORT",
                "DB_DATABASE",
                "DB_USERNAME",
                "DB_PASSWORD"
            ]
        );
        assert!(
            keys.iter()
                .any(|(key, value)| key == "DB_DATABASE" && value == "shop")
        );
        assert!(
            keys.iter()
                .any(|(key, value)| key == "DB_PASSWORD" && value == "s3cret")
        );
    }

    #[test]
    fn env_entries_in_the_project_file_win_over_the_defaults() {
        let temp = TempDir::new();
        let config = Config::default();
        let root = laravel(&temp);
        init(&root, &config, &InitOptions::default(), Os::host()).unwrap();

        let mut project = Project::load(&root).unwrap();
        project
            .file
            .env
            .insert("DB_HOST".to_owned(), "db.internal".to_owned());
        project
            .file
            .env
            .insert("APP_URL".to_owned(), "http://shop.test".to_owned());

        let keys = project.env_keys(&config, "s3cret");
        let host = keys.iter().find(|(key, _)| key == "DB_HOST").unwrap();
        assert_eq!(host.1, "db.internal");
        assert!(
            keys.iter()
                .any(|(key, value)| key == "APP_URL" && value == "http://shop.test")
        );
        assert_eq!(keys.iter().filter(|(key, _)| key == "DB_HOST").count(), 1);
    }

    #[test]
    fn project_settings_inherit_from_the_global_config() {
        let temp = TempDir::new();
        let mut config = Config::default();
        config.server.port = 9090;
        config.database.kind = DatabaseKind::Mysql;

        let root = project(&temp, &[("index.php", "<?php\n")], &[]);
        std::fs::write(root.join("lambo.yml"), "server:\n  document_root: .\n").unwrap();
        let loaded = Project::load(&root).unwrap();

        assert_eq!(
            loaded.http_port(&config),
            9090,
            "an unset port follows the global default"
        );
        assert_eq!(loaded.url(&config), "http://localhost:9090");
        // An omitted engine inherits the global one rather than falling back to
        // a default the user did not choose.
        assert_eq!(loaded.database_kind(&config), DatabaseKind::Mysql);
        assert_eq!(loaded.server_kind(&config), config.server.kind);
    }

    #[test]
    fn a_project_that_wants_no_database_is_honoured() {
        let temp = TempDir::new();
        let mut config = Config::default();
        config.database.kind = DatabaseKind::Mariadb;

        let root = project(&temp, &[("index.php", "<?php\n")], &[]);
        std::fs::write(root.join("lambo.yml"), "database:\n  kind: none\n").unwrap();
        let loaded = Project::load(&root).unwrap();

        assert_eq!(loaded.database_kind(&config), DatabaseKind::None);
        assert!(loaded.validate().is_ok());
    }

    #[test]
    fn entry_point_detection_covers_the_common_files() {
        let temp = TempDir::new();
        let root = temp.join("root");
        std::fs::create_dir_all(&root).unwrap();
        assert!(!has_entry_point(&root));

        testutil::fixture(&root, "index.html", "<html></html>");
        assert!(has_entry_point(&root));
    }
}
