//! Working out what kind of PHP project is in a directory.
//!
//! `lambo init` has to produce a `lambo.yml` that is right for *this* project,
//! and the only honest way to do that is to look at the files on disk. Nothing
//! here guesses from a directory name: every conclusion carries the evidence
//! that produced it, which is printed by `lambo init` so a wrong answer is
//! visibly wrong.
//!
//! The consequences of a detection are small and checkable - which directory is
//! served, whether a database is provisioned, which PHP version to prefer - so
//! a user who disagrees with the result can override it in `lambo.yml`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A recognised project kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framework {
    /// Laravel (or Lumen).
    Laravel,
    /// Symfony.
    Symfony,
    /// CodeIgniter, either the 4.x composer layout or the 3.x zip layout.
    CodeIgniter,
    /// WordPress.
    WordPress,
    /// A Composer project that is none of the above.
    Composer,
    /// Plain PHP files, no framework, no Composer.
    PlainPhp,
}

impl Framework {
    /// Human-readable name.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Laravel => "Laravel",
            Self::Symfony => "Symfony",
            Self::CodeIgniter => "CodeIgniter",
            Self::WordPress => "WordPress",
            Self::Composer => "Composer project",
            Self::PlainPhp => "plain PHP",
        }
    }

    /// Whether the framework needs a database to run at all.
    pub fn needs_database(self) -> bool {
        matches!(
            self,
            Self::Laravel | Self::Symfony | Self::CodeIgniter | Self::WordPress
        )
    }
}

/// One thing looked for, and whether it was there.
///
/// Reported even when not found, so `lambo init` can explain what it ruled out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// What was looked for.
    pub description: String,
    /// Where it was looked for.
    pub path: PathBuf,
    /// Whether it was found.
    pub found: bool,
}

/// What detection concluded, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// The project kind.
    pub framework: Framework,
    /// Document root relative to the project directory.
    pub document_root: &'static str,
    /// Whether to provision a database.
    pub needs_database: bool,
    /// PHP constraint read from `composer.json`, when there is one.
    pub php_requirement: Option<String>,
    /// The findings that decided the answer, in order of weight.
    pub matched: Vec<String>,
    /// Everything that was checked.
    pub probes: Vec<Probe>,
}

impl Detection {
    /// A one-line summary, e.g. `Laravel (artisan, composer.json: laravel/framework)`.
    pub fn summary(&self) -> String {
        if self.matched.is_empty() {
            self.framework.display_name().to_owned()
        } else {
            format!(
                "{} ({})",
                self.framework.display_name(),
                self.matched.join(", ")
            )
        }
    }
}

/// The part of `composer.json` Lambo reads.
#[derive(Debug, Deserialize)]
struct ComposerFile {
    #[serde(default)]
    require: ComposerRequire,
}

/// The `require` section of `composer.json`.
#[derive(Debug, Default, Deserialize)]
struct ComposerRequire {
    /// The PHP constraint, e.g. `^8.2`.
    #[serde(default, rename = "php")]
    php: Option<String>,
    /// Laravel's framework package.
    #[serde(default, rename = "laravel/framework")]
    laravel: Option<String>,
    /// Symfony's framework bundle.
    #[serde(default, rename = "symfony/framework-bundle")]
    symfony: Option<String>,
    /// CodeIgniter 4's framework package.
    #[serde(default, rename = "codeigniter4/framework")]
    codeigniter: Option<String>,
}

/// Detects the project kind of a directory.
///
/// Never fails: a directory with nothing recognisable in it is a plain PHP
/// project served from its own root, which is the safe default.
pub fn detect(project_root: &Path) -> Detection {
    let mut probes = Vec::new();
    let mut matched = Vec::new();

    let composer = read_composer(&mut probes, project_root);
    if let Some(requirement) = composer
        .as_ref()
        .and_then(|file| file.require.php.clone())
        .filter(|php| !php.trim().is_empty())
    {
        matched.push(format!("composer.json requires php {requirement}"));
    }

    // Laravel: `artisan` is the unmistakable marker; the package name confirms
    // it and rules out a hand-written `artisan` script.
    let artisan = probe_file(
        &mut probes,
        project_root,
        "artisan (Laravel's console entry point)",
        "artisan",
    );
    let laravel_package = composer
        .as_ref()
        .and_then(|file| file.require.laravel.clone())
        .map(|version| format!("laravel/framework {version}"));
    if artisan || laravel_package.is_some() {
        if let Some(package) = &laravel_package {
            matched.push(format!("composer.json: {package}"));
        }
        if artisan {
            matched.push("artisan".to_owned());
        }
        return finish(
            Framework::Laravel,
            "public",
            probes,
            matched,
            composer.as_ref(),
        );
    }

    // Symfony: `bin/console` plus the framework bundle.
    let console = probe_file(
        &mut probes,
        project_root,
        "bin/console (Symfony's console entry point)",
        "bin/console",
    );
    let symfony_bundle = composer
        .as_ref()
        .and_then(|file| file.require.symfony.clone())
        .map(|version| format!("symfony/framework-bundle {version}"));
    if let (true, Some(bundle)) = (console, symfony_bundle) {
        matched.push(format!("composer.json: symfony/framework-bundle {bundle}"));
        matched.push("bin/console".to_owned());
        return finish(
            Framework::Symfony,
            "public",
            probes,
            matched,
            composer.as_ref(),
        );
    }

    // CodeIgniter 4 is a Composer package; CodeIgniter 3 is the classic layout
    // with `index.php` next to `application/` and `system/`.
    if let Some(version) = composer
        .as_ref()
        .and_then(|file| file.require.codeigniter.clone())
    {
        matched.push(format!("composer.json: codeigniter4/framework {version}"));
        return finish(
            Framework::CodeIgniter,
            "public",
            probes,
            matched,
            composer.as_ref(),
        );
    }
    let ci_index = probe_where(
        &mut probes,
        project_root,
        "index.php that loads the CodeIgniter system directory",
        "index.php",
        is_codeigniter_entry_point,
    );
    let ci_application = probe_dir(
        &mut probes,
        project_root,
        "application/ (CodeIgniter 3 layout)",
        "application",
    );
    let ci_system = probe_dir(
        &mut probes,
        project_root,
        "system/ (CodeIgniter 3 layout)",
        "system",
    );
    if ci_index && ci_application && ci_system {
        matched.push("CodeIgniter 3 layout: index.php + application/ + system/".to_owned());
        return finish(
            Framework::CodeIgniter,
            ".",
            probes,
            matched,
            composer.as_ref(),
        );
    }

    // WordPress: `wp-settings.php` is the core loader, present in every
    // installation; either config file proves it is an installation rather than
    // a directory that happens to hold the loader.
    let wp_settings = probe_file(
        &mut probes,
        project_root,
        "wp-settings.php (WordPress core)",
        "wp-settings.php",
    );
    let wp_config = probe_file(
        &mut probes,
        project_root,
        "wp-config.php (configured WordPress)",
        "wp-config.php",
    );
    let wp_sample = probe_file(
        &mut probes,
        project_root,
        "wp-config-sample.php (unconfigured WordPress)",
        "wp-config-sample.php",
    );
    if wp_settings && (wp_config || wp_sample) {
        matched.push("wp-settings.php".to_owned());
        return finish(
            Framework::WordPress,
            ".",
            probes,
            matched,
            composer.as_ref(),
        );
    }

    // A Composer project with no recognised framework.
    if composer.is_some() {
        matched.push("composer.json".to_owned());
        let document_root = if project_root.join("public").is_dir() {
            "public"
        } else {
            "."
        };
        return finish(
            Framework::Composer,
            document_root,
            probes,
            matched,
            composer.as_ref(),
        );
    }

    // Plain PHP.
    let index_php = probe_file(&mut probes, project_root, "index.php", "index.php");
    if index_php {
        matched.push("index.php".to_owned());
    }
    finish(Framework::PlainPhp, ".", probes, matched, None)
}

/// Assembles a finished detection.
fn finish(
    framework: Framework,
    document_root: &'static str,
    probes: Vec<Probe>,
    matched: Vec<String>,
    composer: Option<&ComposerFile>,
) -> Detection {
    // A project that says it needs PHP 8.3 should not be handed 8.1.
    let php_requirement = composer.and_then(|file| file.require.php.clone());
    Detection {
        framework,
        document_root,
        needs_database: framework.needs_database(),
        php_requirement,
        matched,
        probes,
    }
}

/// Reads and parses `composer.json`, remembering the probe either way.
fn read_composer(probes: &mut Vec<Probe>, project_root: &Path) -> Option<ComposerFile> {
    let path = project_root.join("composer.json");
    let exists = path.is_file();
    probes.push(Probe {
        description: "composer.json".to_owned(),
        path: path.clone(),
        found: exists,
    });
    if !exists {
        return None;
    }
    // A composer.json that does not parse is still evidence of a Composer
    // project; the caller carries on with no requirements read from it.
    Some(
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_else(|| ComposerFile {
                require: ComposerRequire::default(),
            }),
    )
}

/// Probes for a file and records the result.
fn probe_file(probes: &mut Vec<Probe>, root: &Path, description: &str, relative: &str) -> bool {
    probe_where(probes, root, description, relative, |_| true)
}

/// Probes for a file whose *contents* must also match.
fn probe_where(
    probes: &mut Vec<Probe>,
    root: &Path,
    description: &str,
    relative: &str,
    matches: impl FnOnce(&Path) -> bool,
) -> bool {
    let path = root.join(relative);
    let found = path.is_file() && matches(&path);
    probes.push(Probe {
        description: description.to_owned(),
        path,
        found,
    });
    found
}

/// Probes for a directory and records the result.
fn probe_dir(probes: &mut Vec<Probe>, root: &Path, description: &str, relative: &str) -> bool {
    let path = root.join(relative);
    let found = path.is_dir();
    probes.push(Probe {
        description: description.to_owned(),
        path,
        found,
    });
    found
}

/// Whether an `index.php` is a CodeIgniter 3 front controller.
///
/// Every PHP project can have an `index.php`; only CodeIgniter's loads its
/// system directory from there, so the file name alone proves nothing.
fn is_codeigniter_entry_point(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|text| text.contains("$system_path") || text.contains("CodeIgniter"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Writes a project layout from a list of relative paths.
    fn project(temp: &TempDir, files: &[(&str, &str)], dirs: &[&str]) -> PathBuf {
        let root = temp.join("project");
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

    #[test]
    fn an_empty_directory_is_plain_php_served_from_itself() {
        let temp = TempDir::new();
        let root = project(&temp, &[], &[]);

        let detection = detect(&root);
        assert_eq!(detection.framework, Framework::PlainPhp);
        assert_eq!(
            detection.document_root, ".",
            "plain PHP has no public/ directory"
        );
        assert!(!detection.needs_database);
        assert!(detection.php_requirement.is_none());
        assert_eq!(detection.summary(), "plain PHP");
    }

    #[test]
    fn laravel_is_recognised_and_served_from_public() {
        let temp = TempDir::new();
        let root = project(
            &temp,
            &[
                ("artisan", "#!/usr/bin/env php\n"),
                (
                    "composer.json",
                    r#"{"require":{"php":"^8.2","laravel/framework":"^11.0"}}"#,
                ),
                ("public/index.php", "<?php\n"),
            ],
            &["public"],
        );

        let detection = detect(&root);
        assert_eq!(detection.framework, Framework::Laravel);
        assert_eq!(detection.document_root, "public");
        assert!(detection.needs_database);
        assert_eq!(detection.php_requirement.as_deref(), Some("^8.2"));
        assert!(
            detection.summary().contains("laravel/framework"),
            "{}",
            detection.summary()
        );
        assert!(
            detection.summary().contains("artisan"),
            "{}",
            detection.summary()
        );
    }

    #[test]
    fn laravel_is_recognised_from_the_package_name_alone() {
        let temp = TempDir::new();
        let root = project(
            &temp,
            &[(
                "composer.json",
                r#"{"require":{"laravel/framework":"^10.0"}}"#,
            )],
            &[],
        );
        assert_eq!(detect(&root).framework, Framework::Laravel);
    }

    #[test]
    fn symfony_needs_both_the_console_and_the_bundle() {
        let temp = TempDir::new();
        let root = project(
            &temp,
            &[
                ("bin/console", "#!/usr/bin/env php\n"),
                (
                    "composer.json",
                    r#"{"require":{"symfony/framework-bundle":"^7.0"}}"#,
                ),
            ],
            &["bin", "public"],
        );

        let detection = detect(&root);
        assert_eq!(detection.framework, Framework::Symfony);
        assert_eq!(detection.document_root, "public");
        assert!(detection.needs_database);
    }

    #[test]
    fn a_console_script_without_symfony_is_not_symfony() {
        let temp = TempDir::new();
        let root = project(
            &temp,
            &[
                ("bin/console", "#!/usr/bin/env php\n"),
                ("index.php", "<?php\n"),
            ],
            &["bin"],
        );
        let detection = detect(&root);
        assert_ne!(detection.framework, Framework::Symfony);
        assert_eq!(detection.framework, Framework::PlainPhp);
    }

    #[test]
    fn codeigniter_4_comes_from_composer() {
        let temp = TempDir::new();
        let root = project(
            &temp,
            &[(
                "composer.json",
                r#"{"require":{"codeigniter4/framework":"^4.5"}}"#,
            )],
            &["public"],
        );
        let detection = detect(&root);
        assert_eq!(detection.framework, Framework::CodeIgniter);
        assert_eq!(detection.document_root, "public");
    }

    #[test]
    fn codeigniter_3_comes_from_the_classic_layout() {
        let temp = TempDir::new();
        let root = project(
            &temp,
            &[(
                "index.php",
                "<?php\n$system_path = 'system';\nrequire_once 'system/core/CodeIgniter.php';\n",
            )],
            &["application", "system"],
        );
        let detection = detect(&root);
        assert_eq!(detection.framework, Framework::CodeIgniter);
        assert_eq!(
            detection.document_root, ".",
            "CodeIgniter 3 serves from the root"
        );
        assert!(detection.needs_database);
    }

    #[test]
    fn an_index_php_alone_is_not_codeigniter() {
        let temp = TempDir::new();
        let root = project(
            &temp,
            &[("index.php", "<?php echo 'hi';\n")],
            &["application", "system"],
        );
        assert_eq!(detect(&root).framework, Framework::PlainPhp);
    }

    #[test]
    fn wordpress_is_served_from_the_root() {
        let temp = TempDir::new();
        let root = project(
            &temp,
            &[
                ("wp-settings.php", "<?php\n"),
                ("wp-config-sample.php", "<?php\n"),
            ],
            &["wp-admin"],
        );
        let detection = detect(&root);
        assert_eq!(detection.framework, Framework::WordPress);
        assert_eq!(detection.document_root, ".");
        assert!(detection.needs_database);
    }

    #[test]
    fn a_configured_wordpress_is_also_recognised() {
        let temp = TempDir::new();
        let root = project(
            &temp,
            &[
                ("wp-settings.php", "<?php\n"),
                ("wp-config.php", "<?php define('DB_NAME','shop');\n"),
            ],
            &[],
        );
        assert_eq!(detect(&root).framework, Framework::WordPress);
    }

    #[test]
    fn an_unknown_composer_project_uses_public_when_it_exists() {
        let temp = TempDir::new();
        let root = project(
            &temp,
            &[
                ("composer.json", r#"{"require":{"php":">=8.1"}}"#),
                ("public/index.php", "<?php\n"),
            ],
            &["public"],
        );
        let detection = detect(&root);
        assert_eq!(detection.framework, Framework::Composer);
        assert_eq!(detection.document_root, "public");
        assert!(
            !detection.needs_database,
            "an unknown project must not get a database it did not ask for"
        );
    }

    #[test]
    fn an_unknown_composer_project_without_public_is_served_from_the_root() {
        let temp = TempDir::new();
        let root = project(
            &temp,
            &[("composer.json", "{}"), ("index.php", "<?php\n")],
            &[],
        );
        let detection = detect(&root);
        assert_eq!(detection.framework, Framework::Composer);
        assert_eq!(detection.document_root, ".");
    }

    #[test]
    fn a_broken_composer_json_still_counts_as_a_composer_project() {
        let temp = TempDir::new();
        let root = project(&temp, &[("composer.json", "{ this is not json")], &[]);
        let detection = detect(&root);
        assert_eq!(detection.framework, Framework::Composer);
        assert!(detection.php_requirement.is_none());
    }

    #[test]
    fn every_probe_is_reported_so_a_wrong_answer_is_explainable() {
        let temp = TempDir::new();
        let root = project(&temp, &[("index.php", "<?php\n")], &[]);
        let detection = detect(&root);

        let descriptions: Vec<&str> = detection
            .probes
            .iter()
            .map(|probe| probe.description.as_str())
            .collect();
        assert!(
            descriptions
                .iter()
                .any(|description| description.contains("artisan")),
            "{descriptions:?}"
        );
        assert!(
            descriptions
                .iter()
                .any(|description| description.contains("index.php")),
            "{descriptions:?}"
        );
        let index_probe = detection
            .probes
            .iter()
            .find(|probe| probe.description == "index.php")
            .unwrap();
        assert!(index_probe.found);
        assert!(index_probe.path.ends_with("index.php"));
        assert!(
            detection.probes.iter().any(|probe| !probe.found),
            "ruled-out checks must be visible"
        );
    }
}
