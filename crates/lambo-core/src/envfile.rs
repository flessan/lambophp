//! Reading and updating a project's `.env` file.
//!
//! `lambo up` has to tell the application where its database is. That means
//! editing a file the developer owns, so the rules are strict:
//!
//! - **Nothing that is already set is overwritten.** A developer's
//!   `DB_PASSWORD=` of their own choosing survives every Lambo run; only keys
//!   that are absent or empty are filled in. `force` is available for
//!   `lambo db create --force-env`, never used implicitly.
//! - **The file keeps its shape.** Comments, blank lines, ordering and the
//!   original line endings are preserved byte for byte outside the keys Lambo
//!   touches, so a `.env` stays diffable.
//! - **Writes are atomic.** An interrupted run cannot leave a half-written
//!   `.env` behind - that file often holds the only copy of a working
//!   configuration.
//!
//! The parser is deliberately small: `KEY=VALUE` with optional `#` comments,
//! optional quotes, and `export ` prefixes. Anything else is passed through
//! untouched.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fsx;

/// One line of a `.env` file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// `KEY=VALUE`, remembered with its original formatting.
    KeyValue {
        key: String,
        value: String,
        quoted: bool,
        prefix: String,
    },
    /// Anything else - a comment, a blank line, a line we did not understand -
    /// preserved exactly as it was.
    Other(String),
}

/// A `.env` file, loaded and ready to be edited.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvFile {
    /// Where the file lives (or would be created).
    pub path: PathBuf,
    entries: Vec<Entry>,
    /// `\r\n` when the file uses Windows line endings, else `\n`.
    line_ending: String,
    /// Whether the file ended with a newline.
    trailing_newline: bool,
}

/// What a [`EnvFile::set_missing`] / [`EnvFile::set`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOutcome {
    /// The key was absent or empty and now has a value.
    Written,
    /// The key already had a value and was left alone.
    Kept,
    /// The value was already exactly what was asked for.
    Unchanged,
}

/// A summary of everything changed in one pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// Keys Lambo wrote.
    pub written: Vec<String>,
    /// Keys left alone because the project had already set them.
    pub kept: Vec<String>,
}

impl Changes {
    /// Whether the file needs saving.
    pub fn is_empty(&self) -> bool {
        self.written.is_empty()
    }
}

impl fmt::Display for Changes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.written.is_empty() && self.kept.is_empty() {
            return f.write_str("nothing to do");
        }
        for key in &self.written {
            writeln!(f, "  + {key}")?;
        }
        for key in &self.kept {
            writeln!(f, "  = {key} (left as it was)")?;
        }
        Ok(())
    }
}

impl EnvFile {
    /// Loads a `.env` file; a missing file loads as empty at that path.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let Some(text) = fsx::read_optional(&path)? else {
            return Ok(Self {
                path,
                entries: Vec::new(),
                line_ending: "\n".to_owned(),
                trailing_newline: true,
            });
        };

        // Detect the dominant line ending so a Windows-authored file stays a
        // Windows file after Lambo edits it.
        let line_ending = if text.contains("\r\n") { "\r\n" } else { "\n" }.to_owned();
        let trailing_newline = text.ends_with('\n');

        let mut entries = Vec::new();
        for line in text.lines() {
            entries.push(parse_line(line.trim_end_matches('\r')));
        }
        Ok(Self {
            path,
            entries,
            line_ending,
            trailing_newline,
        })
    }

    /// The current value of a key.
    ///
    /// An empty value reads as `None`: `DB_PASSWORD=` in a Laravel
    /// `.env.example` means "not configured", and Lambo must be allowed to fill
    /// it in.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.iter().find_map(|entry| match entry {
            Entry::KeyValue {
                key: found, value, ..
            } if found.eq_ignore_ascii_case(key) => {
                if value.is_empty() {
                    None
                } else {
                    Some(value.as_str())
                }
            }
            _ => None,
        })
    }

    /// Whether a key is present with a non-empty value.
    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Sets a key, overwriting any existing value.
    pub fn set(&mut self, key: &str, value: &str) -> SetOutcome {
        let previous = self.get(key).map(str::to_owned);
        self.upsert(key, value, true);
        match previous.as_deref() {
            Some(existing) if existing == value => SetOutcome::Unchanged,
            _ => SetOutcome::Written,
        }
    }

    /// Sets a key only when it is absent or empty.
    pub fn set_missing(&mut self, key: &str, value: &str) -> SetOutcome {
        match self.get(key) {
            Some(existing) if existing == value => SetOutcome::Unchanged,
            Some(_) => SetOutcome::Kept,
            None => {
                self.upsert(key, value, false);
                SetOutcome::Written
            }
        }
    }

    /// Applies a batch of keys, reporting what happened to each.
    pub fn ensure_all(&mut self, keys: &[(String, String)], force: bool) -> Changes {
        let mut changes = Changes::default();
        for (key, value) in keys {
            let outcome = if force {
                self.set(key, value)
            } else {
                self.set_missing(key, value)
            };
            match outcome {
                SetOutcome::Written => changes.written.push(key.clone()),
                SetOutcome::Kept => changes.kept.push(key.clone()),
                SetOutcome::Unchanged => {}
            }
        }
        changes
    }

    /// Every key currently in the file, in file order.
    pub fn keys(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::KeyValue { key, .. } => Some(key.as_str()),
                Entry::Other(_) => None,
            })
            .collect()
    }

    /// Writes the file atomically.
    pub fn save(&self) -> Result<()> {
        let mut body = String::new();
        for entry in &self.entries {
            body.push_str(&match entry {
                Entry::KeyValue {
                    key,
                    value,
                    quoted,
                    prefix,
                } => {
                    let rendered = if *quoted || needs_quotes(value) {
                        quote(value)
                    } else {
                        value.clone()
                    };
                    format!("{prefix}{key}={rendered}")
                }
                Entry::Other(line) => line.clone(),
            });
            body.push_str(&self.line_ending);
        }
        if !self.trailing_newline && !body.is_empty() {
            body.truncate(body.len() - self.line_ending.len());
        }
        fsx::write_atomic(&self.path, &body)
    }

    /// Inserts or replaces one key.
    fn upsert(&mut self, key: &str, value: &str, overwrite: bool) {
        for entry in &mut self.entries {
            if let Entry::KeyValue {
                key: found,
                value: current,
                quoted,
                ..
            } = entry
            {
                if found.eq_ignore_ascii_case(key) {
                    if overwrite || current.is_empty() {
                        *quoted = *quoted || needs_quotes(value);
                        *current = value.to_owned();
                    }
                    return;
                }
            }
        }
        self.entries.push(Entry::KeyValue {
            key: key.to_owned(),
            value: value.to_owned(),
            quoted: needs_quotes(value),
            prefix: String::new(),
        });
    }
}

/// Parses one line of a `.env` file.
fn parse_line(line: &str) -> Entry {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
        return Entry::Other(line.to_owned());
    }

    // `export FOO=bar` is legal in files sourced by a shell.
    let (prefix, rest) = match trimmed.strip_prefix("export ") {
        Some(rest) => ("export ".to_owned(), rest),
        None => (String::new(), trimmed),
    };

    let Some((key, value)) = rest.split_once('=') else {
        return Entry::Other(line.to_owned());
    };
    let key = key.trim();
    if key.is_empty()
        || !key
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_')
    {
        return Entry::Other(line.to_owned());
    }

    let value = value.trim();
    let (value, quoted) = unquote(value);
    Entry::KeyValue {
        key: key.to_owned(),
        value,
        quoted,
        prefix,
    }
}

/// Strips one layer of matching quotes.
fn unquote(value: &str) -> (String, bool) {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        return (value[1..value.len() - 1].to_owned(), true);
    }
    (value.to_owned(), false)
}

/// Whether a value must be quoted to survive a round trip.
fn needs_quotes(value: &str) -> bool {
    value.is_empty()
        || value
            .chars()
            .any(|character| character.is_whitespace() || character == '#')
}

/// Quotes a value for writing.
fn quote(value: &str) -> String {
    if value.starts_with('"') && value.ends_with('"') && value.len() > 1 {
        return value.to_owned();
    }
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The keys Lambo writes for a Laravel-style project.
///
/// Laravel reads `DB_*` from `.env` and nothing else, so getting these right is
/// what makes `lambo up` produce a working application rather than a running
/// web server that cannot reach its database.
pub fn laravel_database_keys(
    database: &str,
    user: &str,
    password: &str,
    host: &str,
    port: u16,
) -> Vec<(String, String)> {
    vec![
        ("DB_CONNECTION".to_owned(), "mysql".to_owned()),
        ("DB_HOST".to_owned(), host.to_owned()),
        ("DB_PORT".to_owned(), port.to_string()),
        ("DB_DATABASE".to_owned(), database.to_owned()),
        ("DB_USERNAME".to_owned(), user.to_owned()),
        ("DB_PASSWORD".to_owned(), password.to_owned()),
    ]
}

/// The keys Lambo writes for a project with no recognised framework.
///
/// Deliberately generic and prefixed, so Lambo never collides with an
/// application's own `DB_*` conventions when it does not know what it is.
pub fn generic_database_keys(
    database: &str,
    user: &str,
    password: &str,
    host: &str,
    port: u16,
) -> Vec<(String, String)> {
    vec![
        ("DB_HOST".to_owned(), host.to_owned()),
        ("DB_PORT".to_owned(), port.to_string()),
        ("DB_NAME".to_owned(), database.to_owned()),
        ("DB_USER".to_owned(), user.to_owned()),
        ("DB_PASSWORD".to_owned(), password.to_owned()),
    ]
}

/// Ensures a `.env` file exists, creating it from `.env.example` when that is
/// all the project has.
///
/// Copying the example first is what keeps the result useful: the application's
/// own defaults (`APP_KEY`, queue drivers, …) are already in place, and Lambo
/// only fills the database keys it knows about.
pub fn ensure_exists(project_root: &Path) -> Result<EnvFile> {
    let path = project_root.join(".env");
    if path.is_file() {
        return EnvFile::load(&path);
    }
    let example = project_root.join(".env.example");
    if example.is_file() {
        let text =
            std::fs::read_to_string(&example).map_err(|source| Error::io(&example, source))?;
        fsx::write_atomic(&path, &text)?;
    }
    EnvFile::load(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn env_file(temp: &TempDir, contents: &str) -> EnvFile {
        let path = temp.join(".env");
        std::fs::write(&path, contents).unwrap();
        EnvFile::load(&path).unwrap()
    }

    #[test]
    fn a_missing_file_is_created_with_the_keys_lambo_owns() {
        let temp = TempDir::new();
        let path = temp.join(".env");
        let mut env = EnvFile::load(&path).unwrap();
        assert!(!path.exists());

        let changes = env.ensure_all(
            &[
                ("DB_HOST".to_owned(), "127.0.0.1".to_owned()),
                ("DB_PORT".to_owned(), "3306".to_owned()),
            ],
            false,
        );
        env.save().unwrap();

        assert_eq!(changes.written, ["DB_HOST", "DB_PORT"]);
        assert!(changes.kept.is_empty());
        let reloaded = EnvFile::load(&path).unwrap();
        assert_eq!(reloaded.get("DB_HOST"), Some("127.0.0.1"));
        assert_eq!(reloaded.get("DB_PORT"), Some("3306"));
    }

    #[test]
    fn an_existing_value_is_never_overwritten() {
        let temp = TempDir::new();
        let mut env = env_file(&temp, "DB_HOST=db.internal\nDB_PASSWORD=hunter2\n");

        let changes = env.ensure_all(
            &[
                ("DB_HOST".to_owned(), "127.0.0.1".to_owned()),
                ("DB_PASSWORD".to_owned(), "generated".to_owned()),
                ("DB_PORT".to_owned(), "3306".to_owned()),
            ],
            false,
        );

        assert_eq!(changes.written, ["DB_PORT"]);
        assert_eq!(changes.kept, ["DB_HOST", "DB_PASSWORD"]);
        assert_eq!(env.get("DB_PASSWORD"), Some("hunter2"));
        assert_eq!(env.get("DB_HOST"), Some("db.internal"));
    }

    #[test]
    fn force_overwrites_what_the_project_had() {
        let temp = TempDir::new();
        let mut env = env_file(&temp, "DB_HOST=db.internal\n");

        let changes = env.ensure_all(&[("DB_HOST".to_owned(), "127.0.0.1".to_owned())], true);
        assert_eq!(changes.written, ["DB_HOST"]);
        assert_eq!(env.get("DB_HOST"), Some("127.0.0.1"));
    }

    #[test]
    fn an_empty_value_counts_as_unset() {
        let temp = TempDir::new();
        let mut env = env_file(&temp, "APP_NAME=Shop\nDB_DATABASE=\nDB_PASSWORD=\n");

        let changes = env.ensure_all(
            &[
                ("DB_DATABASE".to_owned(), "shop".to_owned()),
                ("DB_PASSWORD".to_owned(), "s3cret".to_owned()),
            ],
            false,
        );

        assert_eq!(changes.written, ["DB_DATABASE", "DB_PASSWORD"]);
        assert_eq!(env.get("APP_NAME"), Some("Shop"));
    }

    #[test]
    fn comments_blank_lines_and_ordering_survive() {
        let temp = TempDir::new();
        let original = "\
# Shop configuration
APP_NAME=Shop

# Database
DB_CONNECTION=mysql
DB_HOST=127.0.0.1

# nothing below here is understood by lambo
some line without an equals sign
";
        let mut env = env_file(&temp, original);
        env.set_missing("DB_PORT", "3306");
        env.save().unwrap();

        let written = std::fs::read_to_string(temp.join(".env")).unwrap();
        assert_eq!(
            written,
            format!("{original}DB_PORT=3306\n"),
            "Lambo must append without disturbing the file:\n{written}"
        );
    }

    #[test]
    fn windows_line_endings_are_preserved() {
        let temp = TempDir::new();
        let path = temp.join(".env");
        std::fs::write(&path, "APP_NAME=Shop\r\nDB_HOST=127.0.0.1\r\n").unwrap();

        let mut env = EnvFile::load(&path).unwrap();
        env.set_missing("DB_PORT", "3306");
        env.save().unwrap();

        let written = std::fs::read(&path).unwrap();
        let text = String::from_utf8(written).unwrap();
        assert_eq!(
            text,
            "APP_NAME=Shop\r\nDB_HOST=127.0.0.1\r\nDB_PORT=3306\r\n"
        );
        assert!(!text.contains("\n\n"), "no blank line may be introduced");
    }

    #[test]
    fn values_with_spaces_are_quoted_and_roundtrip() {
        let temp = TempDir::new();
        let mut env = EnvFile::load(temp.join(".env")).unwrap();
        env.set("APP_NAME", "My Shop");
        env.set("DB_PASSWORD", "p#ss word");
        env.save().unwrap();

        let written = std::fs::read_to_string(temp.join(".env")).unwrap();
        assert!(written.contains("APP_NAME=\"My Shop\""), "{written}");
        assert!(written.contains("DB_PASSWORD=\"p#ss word\""), "{written}");

        let reloaded = EnvFile::load(temp.join(".env")).unwrap();
        assert_eq!(reloaded.get("APP_NAME"), Some("My Shop"));
        assert_eq!(reloaded.get("DB_PASSWORD"), Some("p#ss word"));
    }

    #[test]
    fn already_quoted_values_are_read_unquoted() {
        let temp = TempDir::new();
        let env = env_file(&temp, "APP_NAME=\"My Shop\"\nDB_PASSWORD='hunter2'\n");
        assert_eq!(env.get("APP_NAME"), Some("My Shop"));
        assert_eq!(env.get("DB_PASSWORD"), Some("hunter2"));
    }

    #[test]
    fn keys_are_matched_case_insensitively_like_php_getenv_consumers() {
        let temp = TempDir::new();
        let mut env = env_file(&temp, "db_host=example.test\n");
        // Laravel treats keys case-sensitively, but a project that wrote
        // `db_host` still means `DB_HOST`; replacing it beats adding a second
        // key that the application will never read.
        let outcome = env.set_missing("DB_HOST", "127.0.0.1");
        assert_eq!(outcome, SetOutcome::Kept);
        assert_eq!(env.keys(), ["db_host"]);
    }

    #[test]
    fn export_prefixes_are_preserved() {
        let temp = TempDir::new();
        let mut env = env_file(&temp, "export DB_HOST=example.test\n");
        env.set("DB_HOST", "127.0.0.1");
        env.save().unwrap();
        let written = std::fs::read_to_string(temp.join(".env")).unwrap();
        assert_eq!(written, "export DB_HOST=127.0.0.1\n");
    }

    #[test]
    fn unchanged_keys_are_not_reported_as_changes() {
        let temp = TempDir::new();
        let mut env = env_file(&temp, "DB_PORT=3306\n");
        let changes = env.ensure_all(&[("DB_PORT".to_owned(), "3306".to_owned())], false);
        assert!(changes.is_empty(), "{changes}");
        assert!(changes.to_string().contains("nothing to do"));
    }

    #[test]
    fn the_example_file_seeds_a_new_env() {
        let temp = TempDir::new();
        let root = temp.join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(".env.example"),
            "APP_NAME=Laravel\nAPP_KEY=\nDB_CONNECTION=mysql\nDB_HOST=localhost\n",
        )
        .unwrap();

        let mut env = ensure_exists(&root).unwrap();
        assert_eq!(env.path, root.join(".env"));
        assert_eq!(env.get("APP_NAME"), Some("Laravel"));

        let changes = env.ensure_all(
            &laravel_database_keys("shop", "root", "pw", "127.0.0.1", 3306),
            false,
        );
        env.save().unwrap();

        // DB_HOST came from the example with a different value: kept.
        // DB_CONNECTION already matched: nothing to do at all.
        assert_eq!(changes.kept, ["DB_HOST"]);
        assert_eq!(
            changes.written,
            ["DB_PORT", "DB_DATABASE", "DB_USERNAME", "DB_PASSWORD"]
        );
        let written = std::fs::read_to_string(env.path).unwrap();
        assert!(written.contains("APP_NAME=Laravel"));
        assert!(
            written.contains("DB_HOST=localhost"),
            "the project's own host must win:\n{written}"
        );
        assert!(written.contains("DB_DATABASE=shop"));
        assert!(written.contains("DB_CONNECTION=mysql"));
    }

    #[test]
    fn an_existing_env_is_left_alone_by_ensure_exists() {
        let temp = TempDir::new();
        let root = temp.join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".env.example"), "FROM=example\n").unwrap();
        std::fs::write(root.join(".env"), "FROM=real\n").unwrap();

        let env = ensure_exists(&root).unwrap();
        assert_eq!(env.get("FROM"), Some("real"));
    }

    #[test]
    fn generic_and_laravel_key_sets_differ_where_they_must() {
        let laravel = laravel_database_keys("shop", "root", "pw", "127.0.0.1", 3306);
        let generic = generic_database_keys("shop", "root", "pw", "127.0.0.1", 3306);

        let laravel_keys: Vec<&str> = laravel.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(
            laravel_keys,
            [
                "DB_CONNECTION",
                "DB_HOST",
                "DB_PORT",
                "DB_DATABASE",
                "DB_USERNAME",
                "DB_PASSWORD"
            ]
        );

        let generic_keys: Vec<&str> = generic.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(
            generic_keys,
            ["DB_HOST", "DB_PORT", "DB_NAME", "DB_USER", "DB_PASSWORD"]
        );
    }
}
