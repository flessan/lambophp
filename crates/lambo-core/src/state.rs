//! Which services Lambo owns right now.
//!
//! Every `lambo up` records what it started - PID, command line, port, log
//! file, project - in `$LAMBO_HOME/data/services.yml`. That record is what
//! makes the later commands honest:
//!
//! - `lambo status` reports *observed* state: the record says where to look,
//!   and the process/port check decides whether it is really running. A record
//!   whose process is gone is reported as stopped, never as running.
//! - `lambo down` stops exactly what Lambo started, and nothing else.
//! - `lambo doctor` can point at the log of a service that died.
//!
//! The file is YAML like every other Lambo document, and it is written
//! atomically so an interrupted `lambo up` cannot leave a half-written state.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fsx;
use crate::paths::Paths;
use crate::platform::Os;
use crate::process::{self, ProcessIdentity};
use crate::yaml;

/// Well-known service names.
pub mod names {
    /// The Apache httpd service.
    pub const APACHE: &str = "apache";
    /// The MariaDB/MySQL service.
    pub const DATABASE: &str = "database";
    /// The bundled database manager.
    pub const DBUI: &str = "dbui";
    /// PHP's built-in development server, when used instead of Apache.
    pub const PHP_SERVER: &str = "php-server";
}

/// The order in which services are started, and the reverse in which they are
/// stopped. Dependencies go first: the database is up before Apache serves a
/// page that queries it.
pub const START_ORDER: [&str; 4] = [
    names::DATABASE,
    names::APACHE,
    names::PHP_SERVER,
    names::DBUI,
];

/// One supervised service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceRecord {
    /// Service name; one of the [`names`] constants.
    pub name: String,
    /// Operating-system process identifier.
    pub pid: u32,
    /// Seconds since the Unix epoch when the service was started.
    pub started_at: u64,
    /// The full command line, for diagnostics.
    pub command: String,
    /// Port the service listens on, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Project root the service was started for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
    /// Log file the service writes to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<PathBuf>,
    /// Start time of the process when it was launched, for identity.
    ///
    /// A PID is not identity: the operating system reuses them. Before Lambo
    /// terminates a recorded service it re-reads this and refuses to act if the
    /// process holding the PID is not the one it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<u64>,
    /// The image name of the process when it was launched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
}

impl ServiceRecord {
    /// Creates a record for a service that was just started.
    pub fn new(name: impl Into<String>, pid: u32, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            pid,
            started_at: now_epoch_seconds(),
            command: command.into(),
            port: None,
            project: None,
            log: None,
            start_time: None,
            executable: None,
        }
    }

    /// Attaches the log file.
    pub fn with_log(mut self, log: impl Into<PathBuf>) -> Self {
        self.log = Some(log.into());
        self
    }

    /// Records the identity of the process that was just started.
    ///
    /// Called with the live PID immediately after spawning, while the identity
    /// is still unambiguous.
    pub fn with_identity(mut self, identity: &ProcessIdentity) -> Self {
        self.start_time = identity.start_time;
        self.executable = identity.executable.clone();
        self
    }

    /// This record's identity, when it carries one.
    pub fn identity(&self) -> Option<ProcessIdentity> {
        match (self.start_time, &self.executable) {
            (None, None) => None,
            (start_time, executable) => Some(ProcessIdentity {
                pid: self.pid,
                start_time,
                executable: executable.clone(),
            }),
        }
    }

    /// Attaches the listening port.
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// Attaches the project the service belongs to.
    pub fn with_project(mut self, project: impl Into<PathBuf>) -> Self {
        self.project = Some(project.into());
        self
    }

    /// How long the service has been running.
    pub fn uptime(&self) -> Duration {
        let now = now_epoch_seconds();
        Duration::from_secs(now.saturating_sub(self.started_at))
    }

    /// A human-readable uptime, e.g. `2m 5s`.
    pub fn uptime_text(&self) -> String {
        let seconds = self.uptime().as_secs();
        match seconds {
            0..=59 => format!("{seconds}s"),
            60..=3599 => format!("{}m {}s", seconds / 60, seconds % 60),
            _ => format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60),
        }
    }

    /// Whether the recorded process is still alive *and still ours*.
    ///
    /// A PID that has been handed to an unrelated process reports as not alive:
    /// the service is gone, and the record is stale. Reporting it as alive
    /// would lead `lambo down` to terminate somebody else's process.
    pub fn is_alive(&self, os: Os) -> bool {
        if !process::is_running(self.pid, os) {
            return false;
        }
        // A record written before identity was tracked has nothing to compare
        // against; liveness is the best available answer for it.
        let Some(expected) = self.identity() else {
            return true;
        };
        match process::identity(self.pid, os) {
            Some(actual) => expected.matches(&actual),
            None => false,
        }
    }
}

/// The set of services Lambo currently owns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Records keyed by service name.
    #[serde(default)]
    pub services: BTreeMap<String, ServiceRecord>,
}

impl State {
    /// Reads the state file; a missing file means nothing is running.
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.state_file();
        match fsx::read_optional(&path)? {
            Some(text) if !text.trim().is_empty() => {
                yaml::from_str(&text).map_err(|source| Error::Yaml { path, source })
            }
            _ => Ok(Self::default()),
        }
    }

    /// Writes the state file atomically.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        let body = yaml::to_string(self).map_err(Error::Serialize)?;
        fsx::write_atomic(&paths.state_file(), &body)
    }

    /// Records a started service, replacing any previous record.
    pub fn record(&mut self, record: ServiceRecord) {
        self.services.insert(record.name.clone(), record);
    }

    /// The record of one service.
    pub fn get(&self, name: &str) -> Option<&ServiceRecord> {
        self.services.get(name)
    }

    /// Forgets one service.
    pub fn remove(&mut self, name: &str) -> Option<ServiceRecord> {
        self.services.remove(name)
    }

    /// Whether anything is recorded at all.
    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }

    /// All records, in service start order.
    pub fn ordered(&self) -> Vec<&ServiceRecord> {
        let mut records: Vec<&ServiceRecord> = START_ORDER
            .iter()
            .filter_map(|name| self.services.get(*name))
            .collect();
        // Anything not in START_ORDER (a future service) still shows up.
        for (name, record) in &self.services {
            if !START_ORDER.contains(&name.as_str()) {
                records.push(record);
            }
        }
        records
    }

    /// All records in the reverse of [`Self::ordered`], for shutdown.
    pub fn ordered_for_shutdown(&self) -> Vec<&ServiceRecord> {
        let mut records = self.ordered();
        records.reverse();
        records
    }

    /// Records whose process is actually alive.
    pub fn alive(&self, os: Os) -> Vec<&ServiceRecord> {
        self.ordered()
            .into_iter()
            .filter(|record| record.is_alive(os))
            .collect()
    }

    /// Drops records whose process is gone, reporting whether anything changed.
    ///
    /// Called after every status check so a crashed service does not linger in
    /// the state file and confuse the next `lambo up`.
    pub fn prune_dead(&mut self, os: Os) -> bool {
        let dead: Vec<String> = self
            .services
            .iter()
            .filter(|(_, record)| !record.is_alive(os))
            .map(|(name, _)| name.clone())
            .collect();
        for name in &dead {
            self.services.remove(name);
        }
        !dead.is_empty()
    }
}

/// Current time as seconds since the Unix epoch.
pub fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// The state file location, for messages that need to name it.
pub fn state_path(paths: &Paths) -> PathBuf {
    paths.state_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::testutil::TempDir;

    fn record(name: &str, pid: u32) -> ServiceRecord {
        ServiceRecord::new(name, pid, format!("fake-service --name {name}"))
    }

    #[test]
    fn empty_state_loads_as_nothing_running() {
        let temp = TempDir::new();
        let paths = temp.home();
        let state = State::load(&paths).unwrap();
        assert!(state.is_empty());
        assert!(state.ordered().is_empty());
    }

    #[test]
    fn records_roundtrip_through_the_state_file() {
        let temp = TempDir::new();
        let paths = temp.home();

        let mut state = State::default();
        state.record(
            record(names::DATABASE, 4240)
                .with_port(3306)
                .with_project(r"C:\Lambo\projects\shop")
                .with_log(r"C:\Lambo\logs\database\mariadb.log"),
        );
        state.record(record(names::APACHE, 512).with_port(8080));
        state.save(&paths).unwrap();

        let loaded = State::load(&paths).unwrap();
        assert_eq!(loaded, state);
        let database = loaded.get(names::DATABASE).unwrap();
        assert_eq!(database.pid, 4240);
        assert_eq!(database.port, Some(3306));
        assert_eq!(
            database.project.as_deref(),
            Some(Path::new(r"C:\Lambo\projects\shop"))
        );
    }

    #[test]
    fn ordering_puts_dependencies_first() {
        let mut state = State::default();
        state.record(record(names::DBUI, 3));
        state.record(record(names::APACHE, 2));
        state.record(record(names::DATABASE, 1));

        let names: Vec<&str> = state.ordered().iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, [names::DATABASE, names::APACHE, names::DBUI]);

        let shutdown: Vec<&str> = state
            .ordered_for_shutdown()
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(shutdown, [names::DBUI, names::APACHE, names::DATABASE]);
    }

    #[test]
    fn unknown_services_are_still_reported() {
        let mut state = State::default();
        state.record(record(names::APACHE, 1));
        state.record(record("redis", 2));

        let names: Vec<&str> = state.ordered().iter().map(|r| r.name.as_str()).collect();
        assert!(
            names.contains(&"redis"),
            "future services must not vanish: {names:?}"
        );
    }

    #[test]
    fn dead_records_are_pruned() {
        let temp = TempDir::new();
        let paths = temp.home();
        let mut state = State::default();
        // PID 0 and u32::MAX are never alive.
        state.record(record(names::APACHE, u32::MAX));
        state.save(&paths).unwrap();

        assert!(state.prune_dead(Os::host()));
        assert!(state.is_empty());
        state.save(&paths).unwrap();
        assert!(State::load(&paths).unwrap().is_empty());
    }

    #[test]
    fn a_record_whose_pid_was_reused_is_not_alive() {
        // The whole point of recording an identity: `is_alive` must report the
        // service as gone when the PID has been handed to another program,
        // because the next thing that trusts it is `lambo down`.
        let os = Os::host();
        let alive_pid = std::process::id();

        let honest = record("apache", alive_pid)
            .with_identity(&process::identity(alive_pid, os).expect("our own identity"));
        assert!(
            honest.is_alive(os),
            "a record matching the live process is alive"
        );

        let stale = record("apache", alive_pid).with_identity(&ProcessIdentity {
            pid: alive_pid,
            start_time: None,
            executable: Some("some-other-program".to_owned()),
        });
        assert!(
            !stale.is_alive(os),
            "a live PID held by a different program must not count as our service"
        );
    }

    #[test]
    fn a_record_without_an_identity_still_reports_liveness() {
        // Records written by an older Lambo carry no identity. Falling back to
        // liveness keeps them working instead of silently marking every
        // running service dead.
        let os = Os::host();
        assert!(record("apache", std::process::id()).is_alive(os));
        assert!(!record("apache", u32::MAX).is_alive(os));
    }

    #[test]
    fn identity_survives_the_state_file() {
        // The identity has to make it through a save/load cycle, or restarting
        // Lambo would forget which processes are its own.
        let os = Os::host();
        let temp = TempDir::new();
        let paths = temp.home();

        let identity = process::identity(std::process::id(), os).expect("identity");
        let mut state = State::default();
        state.record(record("apache", std::process::id()).with_identity(&identity));
        state.save(&paths).unwrap();

        let loaded = State::load(&paths).unwrap();
        let record = loaded.get("apache").expect("the record must survive");
        assert_eq!(record.identity().as_ref(), Some(&identity));
        assert!(
            record.is_alive(os),
            "and it must still be recognised as ours"
        );
    }

    #[test]
    fn uptime_is_reported_in_human_units() {
        let mut record = record(names::APACHE, 1);
        assert!(record.uptime_text().ends_with('s'));

        record.started_at = now_epoch_seconds().saturating_sub(125);
        assert_eq!(record.uptime_text(), "2m 5s");

        record.started_at = now_epoch_seconds().saturating_sub(7200);
        assert_eq!(record.uptime_text(), "2h 0m");

        // A clock that jumps backwards must not panic or print nonsense.
        record.started_at = now_epoch_seconds() + 60;
        assert_eq!(record.uptime_text(), "0s");
    }

    #[test]
    fn a_broken_state_file_is_reported_not_ignored() {
        let temp = TempDir::new();
        let paths = temp.home();
        fsx::write_atomic(&paths.state_file(), "services: [ this is not yaml").unwrap();

        let error = State::load(&paths).unwrap_err();
        assert!(matches!(error, Error::Yaml { .. }), "{error:?}");
    }
}
