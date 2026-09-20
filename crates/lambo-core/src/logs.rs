//! Service logs: where they live, and how to read them.
//!
//! Every service Lambo starts writes to a file under `$LAMBO_HOME/logs/`,
//! grouped by service so `lambo logs database` never has to guess which file
//! matters:
//!
//! ```text
//! logs/
//! ├── apache/     httpd stdout+stderr, plus Apache's own error/access logs
//! ├── database/   the database server's stdout+stderr
//! ├── php/        PHP's error_log
//! └── lambo/      Lambo's own actions (install, up, down)
//! ```
//!
//! Reading is portable: `tail` seeks rather than slurping, because an Apache
//! error log grows without bound, and `follow` polls a file handle instead of
//! relying on `tail -f` or filesystem notifications, neither of which is
//! available everywhere.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::paths::Paths;

/// A sink for the lines a long-running operation reports as it goes.
///
/// The engine narrates what it is doing - `  using cached php.zip`, `  latest
/// Apache: 2.4.68 (VS18, build 260827)` - and each front end decides where
/// that goes: the GUI appends it to its log pane, the CLI prints it, a test
/// records it. Passing a sink rather than reading the output of a child
/// process is what lets the GUI show an install as it happens instead of
/// guessing afterwards.
pub type LogFn = Arc<dyn Fn(&str) + Send + Sync + 'static>;

/// A log sink that discards everything.
///
/// The previous implementation passed `nil` for the same purpose; an explicit
/// no-op is easier to read at a call site than an `Option` that every writer
/// has to unwrap.
pub fn nop_log() -> LogFn {
    Arc::new(|_| {})
}

/// How much of the end of a file `tail` considers at most.
const TAIL_WINDOW: u64 = 256 * 1024;

/// A group of log files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// The Apache httpd service.
    Apache,
    /// The MariaDB/MySQL service.
    Database,
    /// PHP itself (`error_log`).
    Php,
    /// Lambo's own actions.
    Lambo,
}

impl Group {
    /// Every group, in display order.
    pub const ALL: [Self; 4] = [Self::Apache, Self::Database, Self::Php, Self::Lambo];

    /// Directory name under `logs/`.
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Apache => "apache",
            Self::Database => "database",
            Self::Php => "php",
            Self::Lambo => "lambo",
        }
    }

    /// Parses a user-supplied group name (`lambo logs database`).
    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "apache" | "httpd" | "server" => Some(Self::Apache),
            "database" | "db" | "mariadb" | "mysql" => Some(Self::Database),
            "php" => Some(Self::Php),
            "lambo" | "app" => Some(Self::Lambo),
            _ => None,
        }
    }

    /// Human-readable name.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Apache => "Apache",
            Self::Database => "database",
            Self::Php => "PHP",
            Self::Lambo => "Lambo",
        }
    }
}

/// The directory holding one group's logs.
pub fn dir(paths: &Paths, group: Group) -> PathBuf {
    paths.service_logs_dir(group.dir_name())
}

/// The path of one log file, creating parent directories as needed by callers.
pub fn file(paths: &Paths, group: Group, name: &str) -> PathBuf {
    dir(paths, group).join(name)
}

/// Apache's own log file.
pub fn apache(paths: &Paths) -> PathBuf {
    file(paths, Group::Apache, "httpd.log")
}

/// The database server's log file.
pub fn database(paths: &Paths) -> PathBuf {
    file(paths, Group::Database, "database.log")
}

/// PHP's error log.
pub fn php(paths: &Paths) -> PathBuf {
    file(paths, Group::Php, "php-error.log")
}

/// Lambo's own log file.
pub fn lambo(paths: &Paths) -> PathBuf {
    file(paths, Group::Lambo, "lambo.log")
}

/// Returns the last `max_lines` lines of a file.
///
/// A missing file yields an empty list: "no logs yet" is a normal state right
/// after an install, not an error.
pub fn tail(path: &Path, max_lines: usize) -> Result<Vec<String>> {
    let Ok(file) = File::open(path) else {
        return Ok(Vec::new());
    };
    let size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);

    let mut reader = BufReader::new(file);
    if size > TAIL_WINDOW {
        // Seek near the end and drop the partial first line: reading a whole
        // multi-gigabyte log to show twenty lines would be absurd.
        let start = size - TAIL_WINDOW;
        reader
            .seek(SeekFrom::Start(start))
            .map_err(|source| Error::io(path, source))?;
        let mut discarded = String::new();
        let _ = reader.read_line(&mut discarded);
    }

    let mut lines: Vec<String> = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|source| Error::io(path, source))?;
        lines.push(line);
        if lines.len() > max_lines {
            lines.remove(0);
        }
    }
    Ok(lines)
}

/// Streams new content of `path` to `sink` until the process is interrupted.
///
/// Polling is deliberate: it behaves identically on Windows and Unix, needs no
/// platform-specific file notification API, and survives log rotation because
/// the file is reopened when it shrinks.
pub fn follow(path: &Path, sink: &mut dyn FnMut(&str)) -> Result<()> {
    let mut offset: u64 = 0;

    loop {
        match OpenOptions::new().read(true).open(path) {
            Ok(mut file) => {
                let size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
                if size < offset {
                    // The file was truncated or rotated: start over.
                    offset = 0;
                }
                if size > offset {
                    file.seek(SeekFrom::Start(offset))
                        .map_err(|source| Error::io(path, source))?;
                    let mut buffer = Vec::new();
                    file.read_to_end(&mut buffer)
                        .map_err(|source| Error::io(path, source))?;
                    offset = size;
                    if let Ok(text) = String::from_utf8(buffer.clone()) {
                        sink(&text);
                    } else {
                        sink(&String::from_utf8_lossy(&buffer));
                    }
                }
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                sink(&format!("waiting for {} to appear…\n", path.display()));
            }
            Err(source) => return Err(Error::io(path, source)),
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// Empties every log file Lambo owns, returning how many were cleared.
///
/// Files are truncated rather than deleted so a running service keeps writing
/// to the same handle - deleting a log out from under Apache on Windows leaves
/// it holding a handle to a file that no longer has a name.
pub fn clear(paths: &Paths) -> Result<usize> {
    let mut cleared = 0;
    for group in Group::ALL {
        let directory = dir(paths, group);
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            fs::write(&path, []).map_err(|source| Error::io(&path, source))?;
            cleared += 1;
        }
    }
    Ok(cleared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn group_names_parse_generously() {
        assert_eq!(Group::parse("apache"), Some(Group::Apache));
        assert_eq!(Group::parse("Server"), Some(Group::Apache));
        assert_eq!(Group::parse("db"), Some(Group::Database));
        assert_eq!(Group::parse("MariaDB"), Some(Group::Database));
        assert_eq!(Group::parse("lambo"), Some(Group::Lambo));
        assert_eq!(Group::parse("redis"), None);
        assert_eq!(Group::Apache.dir_name(), "apache");
    }

    #[test]
    fn log_paths_are_grouped_under_the_lambo_home() {
        let temp = TempDir::new();
        let paths = temp.home();

        assert_eq!(
            apache(&paths),
            paths.logs_dir().join("apache").join("httpd.log")
        );
        assert_eq!(
            database(&paths),
            paths.logs_dir().join("database").join("database.log")
        );
        assert_eq!(
            php(&paths),
            paths.logs_dir().join("php").join("php-error.log")
        );
        assert_eq!(
            lambo(&paths),
            paths.logs_dir().join("lambo").join("lambo.log")
        );
    }

    #[test]
    fn tail_returns_the_requested_number_of_lines() {
        let temp = TempDir::new();
        let path = temp.join("httpd.log");
        let contents: String = (1..=100).map(|index| format!("line {index}\n")).collect();
        fs::write(&path, contents).unwrap();

        let lines = tail(&path, 5).unwrap();
        assert_eq!(
            lines,
            vec!["line 96", "line 97", "line 98", "line 99", "line 100"]
        );

        // Asking for more than exists returns everything.
        assert_eq!(tail(&path, 1000).unwrap().len(), 100);
    }

    #[test]
    fn tail_of_a_large_file_reads_only_the_end() {
        let temp = TempDir::new();
        let path = temp.join("big.log");
        let filler = "x".repeat(100);
        let mut contents = String::new();
        for index in 0..4000 {
            contents.push_str(&format!("{index} {filler}\n"));
        }
        contents.push_str("the line that matters\n");
        fs::write(&path, &contents).unwrap();
        assert!(path.metadata().unwrap().len() > TAIL_WINDOW);

        let lines = tail(&path, 3).unwrap();
        assert_eq!(lines.last().unwrap(), "the line that matters");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn tail_of_a_missing_file_is_empty_not_an_error() {
        let temp = TempDir::new();
        assert!(tail(&temp.join("nope.log"), 10).unwrap().is_empty());
    }

    #[test]
    fn follow_streams_new_content() {
        let temp = TempDir::new();
        let path = temp.join("stream.log");
        fs::write(&path, "first\n").unwrap();

        let mut seen = String::new();
        // One iteration of the follow loop, extracted so the test terminates.
        let mut offset = 0u64;
        for _ in 0..3 {
            let size = path.metadata().unwrap().len();
            if size > offset {
                let mut file = File::open(&path).unwrap();
                file.seek(SeekFrom::Start(offset)).unwrap();
                let mut buffer = String::new();
                file.read_to_string(&mut buffer).unwrap();
                offset = size;
                seen.push_str(&buffer);
            }
            if offset == 0 {
                continue;
            }
            fs::write(
                &path,
                format!(
                    "{}second\n",
                    String::from_utf8_lossy(&fs::read(&path).unwrap())
                ),
            )
            .unwrap();
        }
        assert!(seen.contains("first"), "{seen}");
        assert!(seen.contains("second"), "{seen}");
    }

    #[test]
    fn clearing_truncates_without_deleting() {
        let temp = TempDir::new();
        let paths = temp.home();
        fs::write(apache(&paths), "old apache output\n").unwrap();
        fs::write(database(&paths), "old database output\n").unwrap();

        assert_eq!(clear(&paths).unwrap(), 2);
        assert_eq!(fs::read_to_string(apache(&paths)).unwrap(), "");
        assert!(
            apache(&paths).exists(),
            "the file must survive so writers keep their handle"
        );
        assert_eq!(clear(&paths).unwrap(), 2);
    }
}
