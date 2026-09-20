//! Moving from king-PHP to Lambo PHP.
//!
//! Someone who used king-PHP has a `~/.king` directory, a `KING_HOME`
//! environment variable and `kingphp.yml` files in their projects. Throwing
//! that away because the tool was renamed would be hostile, so Lambo offers an
//! upgrade - with two rules:
//!
//! - **Nothing is deleted.** The old home and the old project files stay where
//!   they are. Renaming a user's files is not Lambo's decision, and a half-done
//!   migration that removed the original would be unrecoverable.
//! - **A new installation is Lambo-only.** Nothing creates a `~/.king`
//!   directory or a `kingphp.yml`; the legacy paths are read, never written.
//!
//! What migrates: the global configuration (whose schema Lambo still reads,
//! including king's `http_port` key), the project registry, and each project's
//! `kingphp.yml` → `lambo.yml`. What does not: the downloaded runtimes. PHP,
//! Apache and MariaDB builds are large, platform-specific and cheap to fetch
//! again, so Lambo reports which ones to reinstall instead of copying trees it
//! cannot validate.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::fsx;
use crate::lambofile::{self, Lambofile};
use crate::paths::{self, Paths};
use crate::workspace::Workspaces;

/// What a migration found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The old home directory, when there was one.
    pub legacy_home: Option<PathBuf>,
    /// Whether the global configuration was converted.
    pub config_migrated: bool,
    /// Whether the project registry was carried over.
    pub workspaces_migrated: bool,
    /// Project directories whose `kingphp.yml` was converted.
    pub projects: Vec<PathBuf>,
    /// Project directories that still hold a `kingphp.yml` and were not
    /// converted (for example because a `lambo.yml` already exists).
    pub skipped: Vec<PathBuf>,
    /// Runtimes that were in the old home and need reinstalling.
    pub runtimes_to_reinstall: Vec<String>,
    /// Anything else worth telling the user.
    pub notes: Vec<String>,
}

impl Report {
    /// Whether there was anything to do at all.
    pub fn is_empty(&self) -> bool {
        self.legacy_home.is_none() && self.projects.is_empty()
    }

    /// Renders the report the way `lambo migrate` prints it.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return "nothing to migrate: no king-PHP installation was found".to_owned();
        }
        let mut lines = Vec::new();
        if let Some(home) = &self.legacy_home {
            lines.push(format!("found a king-PHP home at {}", home.display()));
        }
        if self.config_migrated {
            lines.push("+ configuration converted to config/lambo.yml".to_owned());
        }
        if self.workspaces_migrated {
            lines.push("+ project registry carried over".to_owned());
        }
        for project in &self.projects {
            lines.push(format!("+ {} → lambo.yml", project.display()));
        }
        for project in &self.skipped {
            lines.push(format!(
                "- {} already has a lambo.yml; kingphp.yml left alone",
                project.display()
            ));
        }
        for runtime in &self.runtimes_to_reinstall {
            lines.push(format!("! reinstall {runtime}"));
        }
        lines.extend(self.notes.iter().cloned());
        lines.join("\n")
    }
}

/// What an upgrade would find, without changing anything.
///
/// `lambo doctor` and `lambo init` call this to offer a migration instead of
/// silently ignoring a previous installation.
pub fn detect(paths: &Paths) -> Report {
    detect_in(paths, legacy_home())
}

/// [`detect`] against an explicit legacy home.
///
/// Split out so the migration logic is testable without touching the
/// environment: `KING_HOME` and the real user home are exactly the things a
/// test must not depend on.
pub fn detect_in(paths: &Paths, legacy: Option<PathBuf>) -> Report {
    let mut report = Report::default();
    let Some(legacy) = legacy else { return report };
    // A path that is not a directory cannot hold anything to migrate. Reporting
    // it anyway would make `lambo migrate` look like it found something.
    if !legacy.is_dir() {
        return report;
    }
    report.legacy_home = Some(legacy.clone());

    if legacy_config_file(&legacy).is_file() && !paths.config_file().is_file() {
        report.config_migrated = true;
    }
    if legacy_workspaces_file(&legacy).is_file() && !paths.workspaces_file().is_file() {
        report.workspaces_migrated = true;
    }
    report.runtimes_to_reinstall = legacy_runtimes(&legacy);

    for project in registered_projects(&legacy) {
        if Lambofile::path_in(&project).is_file() {
            report.skipped.push(project);
        } else if project.join(lambofile::LEGACY_FILE_NAME).is_file() {
            report.projects.push(project);
        }
    }
    report
}

/// Performs the migration.
///
/// Idempotent: running it twice changes nothing the second time, because each
/// step checks whether its destination already exists.
pub fn run(paths: &Paths) -> Result<Report> {
    run_in(paths, legacy_home())
}

/// [`run`] against an explicit legacy home; see [`detect_in`].
pub fn run_in(paths: &Paths, legacy: Option<PathBuf>) -> Result<Report> {
    let planned = detect_in(paths, legacy);
    let Some(legacy) = planned.legacy_home.clone() else {
        return Ok(planned);
    };
    let mut report = Report {
        config_migrated: false,
        workspaces_migrated: false,
        ..planned.clone()
    };

    paths.ensure_layout()?;

    if planned.config_migrated {
        let source = legacy_config_file(&legacy);
        let text =
            std::fs::read_to_string(&source).map_err(|io_error| Error::io(&source, io_error))?;
        // Parsing through Lambo's own loader is what converts the file: king's
        // `http_port` key is an accepted alias, and unknown sections are
        // ignored rather than fatal.
        let config: Config = crate::yaml::from_str(&text).map_err(|yaml_error| Error::Yaml {
            path: source.clone(),
            source: yaml_error,
        })?;
        config.save(paths)?;
        report.config_migrated = true;
        report
            .notes
            .push(format!("the original was left at {}", source.display()));
    }

    if planned.workspaces_migrated {
        let source = legacy_workspaces_file(&legacy);
        let text =
            std::fs::read_to_string(&source).map_err(|io_error| Error::io(&source, io_error))?;
        fsx::write_atomic(&paths.workspaces_file(), &text)?;
        report.workspaces_migrated = true;
    }

    for project in &planned.projects {
        Lambofile::migrate_legacy(project)?;
    }

    if !report.runtimes_to_reinstall.is_empty() {
        report.notes.push(
            "runtimes are not copied: run `lambo php install <version>` (and `lambo server \
             install` / `lambo db install`) to fetch them again"
                .to_owned(),
        );
    }
    if paths::legacy_home_env_is_set() {
        report.notes.push(
            "KING_HOME is still set in your environment; Lambo reads LAMBO_HOME instead".to_owned(),
        );
    }

    Ok(report)
}

/// Converts one project's `kingphp.yml` to `lambo.yml`.
pub fn migrate_project(dir: &Path) -> Result<Option<PathBuf>> {
    Lambofile::migrate_legacy(dir)
}

/// The old home directory, if it exists.
pub fn legacy_home() -> Option<PathBuf> {
    Paths::legacy_root()
}

/// Where king-PHP kept its global configuration.
pub fn legacy_config_file(legacy: &Path) -> PathBuf {
    legacy.join("config").join("king.yml")
}

/// Where king-PHP kept its project registry.
pub fn legacy_workspaces_file(legacy: &Path) -> PathBuf {
    legacy.join("config").join("workspaces.yml")
}

/// The runtime families present in an old home, as install hints.
pub fn legacy_runtimes(legacy: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for (directory, hint) in [
        ("php", "lambo php install <version>"),
        ("apache", "lambo up"),
        ("mariadb", "lambo db install"),
        ("mysql", "lambo db install"),
    ] {
        let path = legacy.join(directory);
        if path.is_dir() {
            let versions = std::fs::read_dir(&path)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if versions.is_empty() {
                found.push(hint.to_owned());
            } else {
                found.push(format!("{hint} (was: {})", versions.join(", ")));
            }
        }
    }
    found
}

/// The projects registered in an old home.
fn registered_projects(legacy: &Path) -> Vec<PathBuf> {
    let file = legacy_workspaces_file(legacy);
    let Ok(workspaces) = Workspaces::load_from(&file) else {
        return Vec::new();
    };
    workspaces
        .iter()
        .flat_map(|(_, projects)| projects.to_vec())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Builds a fake king-PHP home with a registry pointing at `projects`.
    fn legacy_home_in(temp: &TempDir, config: &str, projects: &[PathBuf]) -> PathBuf {
        let root = temp.join(".king");
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(root.join("config/king.yml"), config).unwrap();

        let mut registry = String::from("version: 1\nworkspaces:\n  default:\n");
        for project in projects {
            // YAML flow scalars keep Windows backslashes literal.
            registry.push_str(&format!("    - '{}'\n", project.display()));
        }
        if projects.is_empty() {
            registry.push_str("    []\n");
        }
        std::fs::write(root.join("config/workspaces.yml"), &registry).unwrap();
        std::fs::create_dir_all(root.join("php/8.3.10")).unwrap();
        root
    }

    /// A project holding a legacy `kingphp.yml`.
    fn legacy_project(root: &Path, name: &str) -> PathBuf {
        let project = root.join(name);
        std::fs::create_dir_all(project.join("public")).unwrap();
        std::fs::write(project.join("public/index.php"), "<?php\n").unwrap();
        std::fs::write(
            project.join("kingphp.yml"),
            "name: shop\nphp: '8.2'\nserver: apache\ndatabase: mariadb\ndocument_root: public\n",
        )
        .unwrap();
        project
    }

    #[test]
    fn nothing_happens_when_there_is_no_old_installation() {
        let temp = TempDir::new();
        let paths = temp.home();

        let report = run_in(&paths, None).unwrap();
        assert!(report.is_empty());
        assert_eq!(
            report.render(),
            "nothing to migrate: no king-PHP installation was found"
        );
        assert!(
            !paths.config_file().is_file(),
            "no configuration may be invented"
        );
    }

    #[test]
    fn an_old_home_is_detected_with_its_config_registry_and_runtimes() {
        let temp = TempDir::new();
        let paths = temp.home();
        let project = legacy_project(temp.path(), "shop");
        let legacy = legacy_home_in(
            &temp,
            "server:\n  http_port: 9090\n",
            std::slice::from_ref(&project),
        );

        let report = detect_in(&paths, Some(legacy.clone()));
        assert_eq!(report.legacy_home.as_deref(), Some(legacy.as_path()));
        assert!(report.config_migrated, "king.yml should be converted");
        assert!(report.workspaces_migrated);
        assert_eq!(report.projects, vec![project.clone()]);
        assert!(report.skipped.is_empty());
        assert!(
            report
                .runtimes_to_reinstall
                .iter()
                .any(|hint| hint.contains("8.3.10")),
            "{:?}",
            report.runtimes_to_reinstall
        );
    }

    #[test]
    fn migrating_converts_the_config_the_registry_and_the_projects() {
        let temp = TempDir::new();
        let paths = temp.home();
        let shop = legacy_project(temp.path(), "shop");
        let legacy = legacy_home_in(
            &temp,
            "server:\n  http_port: 9090\n",
            std::slice::from_ref(&shop),
        );

        let report = run_in(&paths, Some(legacy.clone())).unwrap();

        assert!(report.config_migrated);
        let config = Config::load(&paths).unwrap();
        assert_eq!(
            config.server.port, 9090,
            "king's http_port must survive the rename"
        );
        assert!(
            !std::fs::read_to_string(paths.config_file())
                .unwrap()
                .contains("king"),
            "the new file must not mention the old name"
        );

        assert!(report.workspaces_migrated);
        let registry = Workspaces::load(&paths).unwrap();
        assert_eq!(registry.project_count(), 1);

        assert_eq!(report.projects, vec![shop.clone()]);
        let converted = std::fs::read_to_string(shop.join("lambo.yml")).unwrap();
        assert!(converted.contains("document_root: public"), "{converted}");
        assert!(
            shop.join("kingphp.yml").is_file(),
            "the original must not be deleted"
        );
        assert!(
            legacy.join("config/king.yml").is_file(),
            "the old home must be untouched"
        );
    }

    #[test]
    fn migrating_twice_changes_nothing() {
        let temp = TempDir::new();
        let paths = temp.home();
        let shop = legacy_project(temp.path(), "shop");
        let legacy = legacy_home_in(
            &temp,
            "server:\n  http_port: 9090\n",
            std::slice::from_ref(&shop),
        );

        let first = run_in(&paths, Some(legacy.clone())).unwrap();
        assert!(first.config_migrated);
        assert_eq!(first.projects.len(), 1);

        let converted = std::fs::read_to_string(shop.join("lambo.yml")).unwrap();
        let second = run_in(&paths, Some(legacy.clone())).unwrap();
        assert!(
            !second.config_migrated,
            "an existing lambo.yml is never overwritten"
        );
        assert!(second.projects.is_empty());
        assert_eq!(second.skipped, vec![shop.clone()]);
        assert_eq!(
            std::fs::read_to_string(shop.join("lambo.yml")).unwrap(),
            converted
        );
    }

    #[test]
    fn a_project_that_already_has_a_lambofile_is_skipped_not_overwritten() {
        let temp = TempDir::new();
        let project = legacy_project(temp.path(), "shop");
        std::fs::write(project.join("lambo.yml"), "name: hand-written\n").unwrap();

        assert_eq!(migrate_project(&project).unwrap(), None);
        assert_eq!(
            std::fs::read_to_string(project.join("lambo.yml")).unwrap(),
            "name: hand-written\n"
        );
    }

    #[test]
    fn runtimes_are_listed_as_install_hints_not_copied() {
        let temp = TempDir::new();
        let paths = temp.home();
        let legacy = legacy_home_in(&temp, "server: {}\n", &[]);
        std::fs::create_dir_all(legacy.join("mariadb/11.4.4")).unwrap();

        let hints = legacy_runtimes(&legacy);
        assert!(
            hints
                .iter()
                .any(|hint| hint.contains("lambo php install") && hint.contains("8.3.10")),
            "{hints:?}"
        );
        assert!(
            hints
                .iter()
                .any(|hint| hint.contains("lambo db install") && hint.contains("11.4.4")),
            "{hints:?}"
        );
        assert!(
            crate::runtime::installed(&paths, crate::runtime::RuntimeKind::Php)
                .unwrap()
                .is_empty(),
            "no runtime may be copied into the new home"
        );

        let report = run_in(&paths, Some(legacy)).unwrap();
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("runtimes are not copied")),
            "{:?}",
            report.notes
        );
    }

    #[test]
    fn an_empty_runtime_directory_still_produces_a_hint() {
        let temp = TempDir::new();
        let legacy = temp.join(".king");
        std::fs::create_dir_all(legacy.join("apache")).unwrap();
        assert_eq!(legacy_runtimes(&legacy), ["lambo up"]);
    }

    #[test]
    fn an_unreadable_registry_is_not_fatal() {
        let temp = TempDir::new();
        let paths = temp.home();
        let legacy = temp.join(".king");
        std::fs::create_dir_all(legacy.join("config")).unwrap();
        std::fs::write(
            legacy.join("config/king.yml"),
            "server:\n  http_port: 9090\n",
        )
        .unwrap();
        std::fs::write(legacy.join("config/workspaces.yml"), "not: [valid").unwrap();

        let report = run_in(&paths, Some(legacy)).unwrap();
        assert!(
            report.config_migrated,
            "a broken registry must not stop the config migration"
        );
        assert!(report.projects.is_empty());
    }

    #[test]
    fn the_report_explains_what_happened_and_what_is_left() {
        let report = Report {
            legacy_home: Some(PathBuf::from(r"C:\Users\dev\.king")),
            config_migrated: true,
            workspaces_migrated: false,
            projects: vec![PathBuf::from(r"C:\code\shop")],
            skipped: vec![PathBuf::from(r"C:\code\blog")],
            runtimes_to_reinstall: vec!["lambo php install <version> (was: 8.3.10)".to_owned()],
            notes: vec!["KING_HOME is still set".to_owned()],
        };

        let rendered = report.render();
        assert!(rendered.contains(r"C:\Users\dev\.king"), "{rendered}");
        assert!(rendered.contains("+ configuration converted"), "{rendered}");
        assert!(
            rendered.contains(r"+ C:\code\shop → lambo.yml"),
            "{rendered}"
        );
        assert!(rendered.contains(r"- C:\code\blog"), "{rendered}");
        assert!(rendered.contains("! reinstall"), "{rendered}");
        assert!(rendered.contains("KING_HOME"), "{rendered}");
    }
}
