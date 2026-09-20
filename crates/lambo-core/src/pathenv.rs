//! The user `PATH`: which of Lambo's directories belong on it.
//!
//! Ported from the original implementation's `pathenv.go`. The step exists
//! because installing a tool
//! is only half of what a service card promises: a terminal the user opens has
//! to find `php`, `composer` and `node` without being told where they are.
//!
//! # Why this is split the way it is
//!
//! The rules are the interesting part and they are not about Windows at all:
//! which directories are candidates, that a directory already on `PATH` is not
//! added again regardless of case or separator, that a directory holding an
//! executable which shadows one of Windows' own is skipped, that blank entries
//! do not survive a rewrite, and that a `PATH` written with `%USERPROFILE%` in
//! it stays expandable. Those live here, behind [`EnvironmentStore`], and are
//! tested with a store in memory.
//!
//! Only the registry calls are platform work, and they are in
//! `lambo_process_windows::win32` - see [`SystemEnvironment`], which is the
//! store the product uses on Windows.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Whether a `PATH` value holds `%name%` references and must stay expandable.
///
/// The original did not read the value's type: it wrote `REG_EXPAND_SZ` when the
/// text contained a `%`, and `REG_SZ` otherwise, so a `PATH` that used
/// `%USERPROFILE%` kept working and one that did not stayed a plain string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// `REG_SZ`.
    Plain,
    /// `REG_EXPAND_SZ`.
    Expandable,
}

/// The platform's environment store.
///
/// A trait rather than free functions because the rules above have to be
/// exercised without touching the registry of the machine running the tests -
/// and because the tests have to be able to make a write fail.
pub trait EnvironmentStore {
    /// The user's `PATH`, or `None` when the value does not exist.
    ///
    /// Errors keep the original's wording: `open HKCU\Environment: ..` when the
    /// key cannot be opened, `read Path: ..` when the value cannot be read.
    fn read_user_path(&self) -> Result<Option<String>>;

    /// Replaces the user's `PATH`.
    ///
    /// An error keeps the original's wording: `write Path: ..`.
    fn write_user_path(&self, value: &str, kind: PathKind) -> Result<()>;

    /// The machine's `PATH`, or `None` when it cannot be read.
    ///
    /// `None` is not a failure: it means no directory is skipped for shadowing
    /// an executable Windows already provides.
    fn machine_path(&self) -> Option<String>;

    /// Announces the change, so a shell that is already running picks it up.
    fn broadcast_change(&self);
}

/// The directories holding this installation's tools, in the original's order.
///
/// Only directories that exist are returned: the list is a fixed catalogue of
/// every tool Lambo can install, and a user has a handful of them. The Composer
/// global directory comes from `%APPDATA%`, because that is where Composer
/// installs global binaries, and it is skipped when `APPDATA` is unset.
pub fn tool_dirs(base_dir: &Path, appdata: Option<&str>) -> Vec<PathBuf> {
    /// Relative directories, in the order the original listed them.
    const CANDIDATES: [&[&str]; 26] = [
        &["bin", "apache", "bin"],
        &["bin", "nginx"],
        &["bin", "php"],
        &["bin", "mysql", "bin"],
        &["bin", "pgsql", "bin"],
        &["bin", "redis"],
        &["bin", "pgweb"],
        &["bin", "minio"],
        &["bin", "mailpit"],
        &["bin", "node"],
        &["bin", "python"],
        &["bin", "python", "Scripts"],
        &["bin", "go", "bin"],
        &["bin", "java", "bin"],
        &["bin", "julia", "bin"],
        &["bin", "zig"],
        &["bin", "dart", "bin"],
        &["bin", "lua"],
        &["bin", "ruby", "bin"],
        &["bin", "rust", ".cargo", "bin"],
        &["bin", "kotlin", "bin"],
        &["bin", "haskell", "bin"],
        &["bin", "elixir", "bin"],
        &["bin", "crystal"],
        &["bin", "scala", "bin"],
        &["bin", "erlang", "bin"],
    ];

    let mut dirs = Vec::with_capacity(CANDIDATES.len() + 1);
    for parts in CANDIDATES {
        let mut dir = base_dir.to_path_buf();
        for part in parts {
            dir.push(part);
        }
        if dir.is_dir() {
            dirs.push(dir);
        }
    }

    if let Some(appdata) = appdata.filter(|value| !value.is_empty()) {
        let composer = Path::new(appdata)
            .join("Composer")
            .join("vendor")
            .join("bin");
        if composer.is_dir() {
            dirs.push(composer);
        }
    }

    dirs
}

/// The key two `PATH` entries are compared by: separators normalised to
/// backslashes, lower case, surrounding whitespace removed.
fn entry_key(part: &str) -> String {
    part.trim().replace('/', "\\").to_lowercase()
}

/// The key used for the prefix test, which - unlike [`entry_key`] - does *not*
/// trim. That asymmetry is the original's: a `PATH` entry written with a
/// leading space is not recognised as one of Lambo's own, so it is left in
/// place. Preserved rather than corrected.
fn prefix_key(part: &str) -> String {
    part.replace('/', "\\").to_lowercase()
}

/// A `PATH` entry, as a path this machine can look in.
///
/// [`entry_key`] normalises separators to backslashes, because `PATH` entries
/// are written the Windows way on the machine this was ported from. Comparing
/// keys is text work and stays as it was; *looking inside* a directory is a
/// filesystem question, and on Unix a backslash is an ordinary character in a
/// file name - `\tmp\bin` is not `/tmp/bin`. Translating back here is what makes
/// the shadow rule work on every platform instead of silently never firing
/// outside Windows. On Windows the two spellings are the same string, so
/// nothing changes there.
fn host_path(key: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(key)
    } else {
        PathBuf::from(key.replace('\\', "/"))
    }
}

/// Whether `dir` holds an executable that `other_dirs` also provides.
///
/// A directory that shadows `where.exe` or `sort.exe` would change the meaning
/// of commands the user already relies on, so it is left off `PATH` entirely
/// rather than put in front of Windows' own copy.
pub fn shadows_another_directory(dir: &Path, other_dirs: &BTreeSet<String>) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.to_lowercase().ends_with(".exe") {
            continue;
        }
        if entry.path().is_dir() {
            continue;
        }
        for other in other_dirs {
            if host_path(other).join(name.as_ref()).exists() {
                return true;
            }
        }
    }
    false
}

/// Adds Lambo's tool directories to the user's `PATH`.
///
/// Returns how many were added; `0` means there was nothing to do, and nothing
/// was written. Fails when no tool is installed, with the original's message -
/// the button that calls this is offered before anything is installed, and
/// "nothing was added" would be a worse answer than "there is nothing to add".
pub fn add_to_user_path(
    base_dir: &Path,
    appdata: Option<&str>,
    store: &dyn EnvironmentStore,
) -> Result<usize> {
    let dirs = tool_dirs(base_dir, appdata);
    if dirs.is_empty() {
        return Err(Error::InvalidInput(
            "no Lambo tools installed yet".to_owned(),
        ));
    }

    let current = store.read_user_path()?.unwrap_or_default();

    let mut existing: BTreeSet<String> = BTreeSet::new();
    for part in current.split(';') {
        if !part.trim().is_empty() {
            existing.insert(entry_key(part));
        }
    }

    let machine: BTreeSet<String> = store
        .machine_path()
        .map(|value| value.split(';').map(entry_key).collect())
        .unwrap_or_default();

    let mut parts: Vec<String> = current.split(';').map(str::to_owned).collect();
    let mut added = 0;
    for dir in &dirs {
        let key = entry_key(&dir.to_string_lossy());
        if existing.contains(&key) {
            continue;
        }
        if shadows_another_directory(dir, &machine) {
            continue;
        }
        parts.push(dir.to_string_lossy().into_owned());
        existing.insert(key);
        added += 1;
    }

    if added == 0 {
        return Ok(0);
    }

    // An entry that is empty or only whitespace does not survive the rewrite,
    // which is what the original did whenever it wrote the value back.
    let kept: Vec<&str> = parts
        .iter()
        .map(String::as_str)
        .filter(|part| !part.trim().is_empty())
        .collect();
    store.write_user_path(&kept.join(";"), kind_of(&current))?;
    store.broadcast_change();
    Ok(added)
}

/// Removes Lambo's tool directories from the user's `PATH`.
///
/// Everything under `<base_dir>\bin` goes, not just the directories
/// [`tool_dirs`] would return: uninstalling must not leave stale entries behind
/// for a tool that is no longer there. Returns how many entries were removed,
/// and writes nothing when there is nothing to remove.
pub fn remove_from_user_path(base_dir: &Path, store: &dyn EnvironmentStore) -> Result<usize> {
    let prefix = format!("{}\\bin", prefix_key(&base_dir.to_string_lossy()));

    let current = store.read_user_path()?.unwrap_or_default();

    let mut kept: Vec<&str> = Vec::new();
    let mut removed = 0;
    for part in current.split(';') {
        if part.trim().is_empty() {
            continue;
        }
        if prefix_key(part).starts_with(&prefix) {
            removed += 1;
            continue;
        }
        kept.push(part);
    }

    if removed == 0 {
        return Ok(0);
    }

    store.write_user_path(&kept.join(";"), kind_of(&current))?;
    store.broadcast_change();
    Ok(removed)
}

/// The value type a write has to keep.
fn kind_of(current: &str) -> PathKind {
    if current.contains('%') {
        PathKind::Expandable
    } else {
        PathKind::Plain
    }
}

/// Whether this process runs elevated.
///
/// Windows only; on any other platform there is no UAC prompt to have answered,
/// so the answer is `false` and the interface offers no elevation button.
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        lambo_process_windows::win32::is_elevated()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Starts this program again through the UAC prompt.
///
/// The error is the original's `UAC: <reason>`; the reason is the code
/// `ShellExecuteW` returned, formatted by the OS, so a dismissed prompt reads
/// as such instead of as a number.
pub fn relaunch_elevated() -> Result<()> {
    #[cfg(windows)]
    {
        let exe = std::env::current_exe()
            .map_err(|source| Error::InvalidInput(format!("executable path: {source}")))?;
        lambo_process_windows::win32::run_elevated(&exe)
            .map_err(|source| Error::InvalidInput(format!("UAC: {source}")))
    }
    #[cfg(not(windows))]
    {
        Err(Error::Unsupported(
            "restarting with administrator rights is a Windows feature",
        ))
    }
}

/// The store the product uses: the registry, on Windows.
#[cfg(windows)]
pub struct SystemEnvironment;

#[cfg(windows)]
impl EnvironmentStore for SystemEnvironment {
    fn read_user_path(&self) -> Result<Option<String>> {
        lambo_process_windows::win32::user_path()
            .map_err(|source| Error::InvalidInput(format!("open HKCU\\Environment: {source}")))
    }

    fn write_user_path(&self, value: &str, kind: PathKind) -> Result<()> {
        let expandable = kind == PathKind::Expandable;
        lambo_process_windows::win32::set_user_path(value, expandable)
            .map_err(|source| Error::InvalidInput(format!("write Path: {source}")))
    }

    fn machine_path(&self) -> Option<String> {
        lambo_process_windows::win32::machine_path()
    }

    fn broadcast_change(&self) {
        lambo_process_windows::win32::broadcast_environment_change();
    }
}

/// The refresh [`crate::installer::Installer`] runs after an install.
///
/// Windows only, because that is where the user `PATH` is something a program
/// maintains; an installer built on any other platform simply does not update
/// one, exactly as the original - which had no non-Windows build - did.
#[cfg(windows)]
pub fn refresher(base_dir: impl Into<PathBuf>) -> crate::installer::PathRefresher {
    let base_dir = base_dir.into();
    std::sync::Arc::new(move || {
        let appdata = std::env::var("APPDATA").ok();
        add_to_user_path(&base_dir, appdata.as_deref(), &SystemEnvironment)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::testutil::{TempDir, fixture};

    /// A store in memory, so the rules can be tested without a registry.
    #[derive(Default)]
    struct FakeStore {
        path: Mutex<Option<String>>,
        machine: Mutex<Option<String>>,
        writes: Mutex<Vec<(String, PathKind)>>,
        broadcasts: Mutex<usize>,
        fail_write: Mutex<Option<String>>,
    }

    impl FakeStore {
        fn with(path: Option<&str>) -> Self {
            Self {
                path: Mutex::new(path.map(str::to_owned)),
                ..Self::default()
            }
        }

        fn with_machine(self, machine: &str) -> Self {
            *self.machine.lock().unwrap() = Some(machine.to_owned());
            self
        }

        fn fail_writes(&self, reason: &str) {
            *self.fail_write.lock().unwrap() = Some(reason.to_owned());
        }

        fn written(&self) -> Option<(String, PathKind)> {
            self.writes.lock().unwrap().last().cloned()
        }

        fn broadcasts(&self) -> usize {
            *self.broadcasts.lock().unwrap()
        }
    }

    impl EnvironmentStore for FakeStore {
        fn read_user_path(&self) -> Result<Option<String>> {
            Ok(self.path.lock().unwrap().clone())
        }

        fn write_user_path(&self, value: &str, kind: PathKind) -> Result<()> {
            if let Some(reason) = self.fail_write.lock().unwrap().clone() {
                return Err(Error::InvalidInput(reason));
            }
            *self.path.lock().unwrap() = Some(value.to_owned());
            self.writes.lock().unwrap().push((value.to_owned(), kind));
            Ok(())
        }

        fn machine_path(&self) -> Option<String> {
            self.machine.lock().unwrap().clone()
        }

        fn broadcast_change(&self) {
            *self.broadcasts.lock().unwrap() += 1;
        }
    }

    /// An installation root with a PHP and a MariaDB client in it.
    fn installed(temp: &TempDir) -> PathBuf {
        fixture(temp.path(), "bin/php/php.exe", "MZ");
        fixture(temp.path(), "bin/mysql/bin/mariadb.exe", "MZ");
        temp.path().to_path_buf()
    }

    fn created(path: &Path) {
        std::fs::create_dir_all(path).expect("create fixture directory");
    }

    #[test]
    fn only_the_directories_that_exist_are_candidates() {
        let temp = TempDir::new();
        fixture(temp.path(), "bin/php/php.exe", "MZ");
        fixture(temp.path(), "bin/erlang/bin/erl.exe", "MZ");

        assert_eq!(
            tool_dirs(temp.path(), None),
            vec![temp.join("bin/php"), temp.join("bin/erlang/bin")],
            "the catalogue order is kept and missing tools are skipped"
        );
    }

    #[test]
    fn composer_is_on_the_list_only_when_appdata_says_so() {
        let temp = TempDir::new();
        let appdata = TempDir::new();
        let base = installed(&temp);

        assert_eq!(
            tool_dirs(&base, Some("")).len(),
            2,
            "an unset APPDATA adds nothing"
        );
        assert_eq!(
            tool_dirs(&base, None).len(),
            2,
            "and neither does no APPDATA at all"
        );

        created(&appdata.join("Composer/vendor/bin"));
        let global = appdata.join("Composer/vendor/bin");
        let appdata = appdata.path().to_string_lossy().into_owned();
        assert_eq!(
            tool_dirs(&base, Some(&appdata)).last(),
            Some(&global),
            "the Composer global directory comes last"
        );
    }

    #[test]
    fn nothing_to_add_is_an_error_rather_than_a_silent_no_op() {
        let temp = TempDir::new();
        let store = FakeStore::with(None);

        let error = add_to_user_path(temp.path(), None, &store).expect_err("no tools installed");

        assert!(error.to_string().contains("no Lambo tools installed yet"));
        assert!(store.written().is_none(), "nothing is written");
        assert_eq!(store.broadcasts(), 0, "and nothing is announced");
    }

    #[test]
    fn a_missing_path_value_is_created_from_the_candidates() {
        let temp = TempDir::new();
        let base = installed(&temp);
        let store = FakeStore::with(None);

        let added = add_to_user_path(&base, None, &store).expect("added");

        assert_eq!(added, 2, "php and the MariaDB client directory");
        let (value, kind) = store.written().expect("written");
        // Joined component by component, the way tool_dirs builds them: a
        // multi-component string pushed into a PathBuf keeps its separators
        // verbatim, which on Windows is not what the catalogue's order means.
        assert_eq!(
            value,
            format!(
                "{}{}{}",
                base.join("bin").join("php").display(),
                ";",
                base.join("bin").join("mysql").join("bin").display()
            ),
            "in the catalogue's order, with no leading separator"
        );
        assert_eq!(kind, PathKind::Plain, "no `%`, so a plain string");
        assert_eq!(store.broadcasts(), 1, "the shell is told once");
    }

    #[test]
    fn an_expandable_path_stays_expandable() {
        let temp = TempDir::new();
        let base = installed(&temp);
        let store = FakeStore::with(Some("%USERPROFILE%\\bin;C:\\tools"));

        add_to_user_path(&base, None, &store).expect("added");

        let (value, kind) = store.written().expect("written");
        assert!(
            value.starts_with("%USERPROFILE%\\bin;C:\\tools;"),
            "{value}"
        );
        assert_eq!(
            kind,
            PathKind::Expandable,
            "a `%` in the old value keeps it an expandable string"
        );
    }

    #[test]
    fn an_entry_that_is_already_there_is_not_added_again() {
        let temp = TempDir::new();
        let base = installed(&temp);

        // The same directory, spelled with a different case and separators.
        let existing = format!(
            "{}/BIN/PHP",
            base.to_string_lossy().replace('\\', "/").to_uppercase()
        );
        let store = FakeStore::with(Some(&existing));

        let added = add_to_user_path(&base, None, &store).expect("added");

        assert_eq!(added, 1, "only the MariaDB client directory is new");
        assert_eq!(
            store.written().expect("written").0,
            format!(
                "{existing};{}",
                base.join("bin").join("mysql").join("bin").display()
            ),
            "the entry that was there keeps its own spelling"
        );
    }

    #[test]
    fn nothing_to_add_writes_nothing_and_says_nothing() {
        let temp = TempDir::new();
        let base = installed(&temp);
        let existing = format!(
            "{};{}",
            base.join("bin/php").display(),
            base.join("bin/mysql/bin").display()
        );
        let store = FakeStore::with(Some(&existing));

        assert_eq!(
            add_to_user_path(&base, None, &store).expect("nothing to do"),
            0
        );
        assert!(
            store.written().is_none(),
            "an unchanged PATH is not rewritten"
        );
        assert_eq!(store.broadcasts(), 0, "and no shell is woken up");
    }

    #[test]
    fn blank_entries_do_not_survive_a_rewrite() {
        let temp = TempDir::new();
        let base = installed(&temp);
        let store = FakeStore::with(Some("C:\\keep;;   ;C:\\other"));

        add_to_user_path(&base, None, &store).expect("added");

        let (value, _) = store.written().expect("written");
        assert!(value.starts_with("C:\\keep;C:\\other;"), "{value}");
        assert!(
            !value.contains(";;"),
            "no empty entry is left behind: {value}"
        );
    }

    #[test]
    fn a_directory_that_shadows_a_windows_tool_is_left_off() {
        let temp = TempDir::new();
        let system = TempDir::new();
        fixture(temp.path(), "bin/php/sort.exe", "MZ");
        fixture(system.path(), "sort.exe", "MZ");
        let store = FakeStore::with(None).with_machine(&system.path().to_string_lossy());

        assert_eq!(
            add_to_user_path(temp.path(), None, &store).expect("nothing to add"),
            0,
            "php's directory would shadow `sort`"
        );
        assert!(store.written().is_none());
        assert_eq!(store.broadcasts(), 0);
    }

    #[test]
    fn a_directory_whose_executables_are_not_shadowed_is_added() {
        let temp = TempDir::new();
        let system = TempDir::new();
        fixture(temp.path(), "bin/php/php.exe", "MZ");
        fixture(system.path(), "sort.exe", "MZ");
        let store = FakeStore::with(None).with_machine(&system.path().to_string_lossy());

        assert_eq!(
            add_to_user_path(temp.path(), None, &store).expect("added"),
            1
        );
    }

    #[test]
    fn a_subdirectory_is_not_an_executable_to_shadow() {
        let temp = TempDir::new();
        let system = TempDir::new();
        // A directory called `sort.exe` is not the thing `PATH` would resolve.
        created(&temp.join("bin/php/sort.exe"));
        fixture(system.path(), "sort.exe", "MZ");
        let store = FakeStore::with(None).with_machine(&system.path().to_string_lossy());

        assert_eq!(
            add_to_user_path(temp.path(), None, &store).expect("added"),
            1
        );
    }

    #[test]
    fn the_machine_path_is_compared_without_case_or_separators() {
        // The comparison itself: separators are normalised to backslashes and
        // the case is dropped, so the same directory spelled two ways is one
        // entry.
        assert_eq!(
            entry_key("C:/Program Files/Git/CMD"),
            entry_key("c:\\program files\\git\\cmd")
        );
        assert_eq!(prefix_key("C:/Temp"), "c:\\temp");
        assert_ne!(entry_key("C:\\Temp"), entry_key("C:\\Temp2"));
        assert_eq!(entry_key("  C:\\Temp  "), "c:\\temp");

        // And through the rule: a directory the machine `PATH` spells with the
        // other separator still shadows.
        let temp = TempDir::new();
        let system = TempDir::new();
        fixture(temp.path(), "bin/php/sort.exe", "MZ");
        fixture(system.path(), "sort.exe", "MZ");
        // On Windows one directory has two spellings and the check has to see
        // through both; on a case-sensitive filesystem an upper-cased spelling
        // is a *different* directory, so only the separator varies there.
        let machine = system.path().to_string_lossy().replace('/', "\\");
        let machine = if cfg!(windows) {
            machine.to_uppercase()
        } else {
            machine
        };
        let store = FakeStore::with(None).with_machine(&machine);

        assert_eq!(
            add_to_user_path(temp.path(), None, &store).expect("nothing to add"),
            0
        );
    }

    #[test]
    fn a_machine_path_that_cannot_be_read_skips_nothing() {
        let temp = TempDir::new();
        fixture(temp.path(), "bin/php/php.exe", "MZ");
        let store = FakeStore::with(None);

        assert_eq!(
            add_to_user_path(temp.path(), None, &store).expect("added"),
            1
        );
    }

    #[test]
    fn a_failed_write_is_reported_and_nothing_is_announced() {
        let temp = TempDir::new();
        let base = installed(&temp);
        let store = FakeStore::with(None);
        store.fail_writes("write Path: access denied");

        let error = add_to_user_path(&base, None, &store).expect_err("the write fails");

        assert!(error.to_string().contains("write Path: access denied"));
        assert_eq!(store.broadcasts(), 0);
    }

    #[test]
    fn removing_takes_every_directory_under_the_installation_bin() {
        let temp = TempDir::new();
        let base = temp.path().to_path_buf();
        let existing = format!(
            "C:\\keep;{}\\bin\\php;{};C:\\other",
            base.display(),
            base.join("bin").join("gone-tool").display()
        );
        let store = FakeStore::with(Some(&existing));

        let removed = remove_from_user_path(&base, &store).expect("removed");

        assert_eq!(removed, 2, "the tool directory and the one that is gone");
        assert_eq!(store.written().expect("written").0, "C:\\keep;C:\\other");
        assert_eq!(store.broadcasts(), 1);
    }

    #[test]
    fn removing_matches_a_different_case_and_forward_slashes() {
        let temp = TempDir::new();
        let base = temp.path().to_path_buf();
        let existing = format!(
            "C:\\keep;{}/BIN/PHP",
            base.to_string_lossy().replace('\\', "/").to_uppercase()
        );
        let store = FakeStore::with(Some(&existing));

        assert_eq!(remove_from_user_path(&base, &store).expect("removed"), 1);
        assert_eq!(store.written().expect("written").0, "C:\\keep");
    }

    #[test]
    fn nothing_to_remove_writes_nothing() {
        let temp = TempDir::new();
        let store = FakeStore::with(Some("C:\\keep;C:\\other"));

        assert_eq!(
            remove_from_user_path(temp.path(), &store).expect("nothing to remove"),
            0
        );
        assert!(store.written().is_none());
        assert_eq!(store.broadcasts(), 0);
    }

    #[test]
    fn an_entry_with_a_leading_space_is_not_recognised_when_removing() {
        // The original trimmed when adding but not when removing, so an entry
        // written with a leading space survived a removal. Preserved.
        let temp = TempDir::new();
        let store = FakeStore::with(Some(" C:\\keep;C:\\other"));

        assert_eq!(
            remove_from_user_path(temp.path(), &store).expect("nothing to remove"),
            0
        );
        assert!(store.written().is_none());
    }

    #[test]
    fn the_value_stays_expandable_when_removing_too() {
        let temp = TempDir::new();
        let base = temp.path().to_path_buf();
        let store = FakeStore::with(Some(&format!(
            "%USERPROFILE%\\bin;{}\\bin\\php",
            base.display()
        )));

        assert_eq!(remove_from_user_path(&base, &store).expect("removed"), 1);
        let (value, kind) = store.written().expect("written");
        assert_eq!(value, "%USERPROFILE%\\bin");
        assert_eq!(kind, PathKind::Expandable);
    }

    #[test]
    fn a_failed_write_when_removing_is_reported_too() {
        let temp = TempDir::new();
        let base = temp.path().to_path_buf();
        let store = FakeStore::with(Some(&format!("{}\\bin\\php", base.display())));
        store.fail_writes("write Path: access denied");

        let error = remove_from_user_path(&base, &store).expect_err("the write fails");

        assert!(error.to_string().contains("write Path: access denied"));
        assert_eq!(store.broadcasts(), 0);
    }

    #[test]
    fn elevation_is_a_windows_question() {
        // On Windows this asks the process token; elsewhere it is `false` and
        // the restart is refused, so no interface offers a button that cannot
        // work.
        if cfg!(windows) {
            let _ = is_elevated();
        } else {
            assert!(!is_elevated());
            let error = relaunch_elevated().expect_err("there is no UAC elsewhere");
            assert!(error.to_string().contains("Windows"));
        }
    }

    #[test]
    fn the_separator_normalising_key_is_the_one_paths_are_compared_by() {
        assert_eq!(entry_key(" C:/Tools/php "), "c:\\tools\\php");
        assert_eq!(prefix_key("C:/Lambo/bin"), "c:\\lambo\\bin");
        assert_eq!(
            prefix_key(" C:/Lambo/bin"),
            " c:\\lambo\\bin",
            "the prefix test does not trim - the original did not"
        );
    }
}
