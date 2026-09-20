//! The tray icon, its menu, and starting with Windows.
//!
//! Ported from the original implementation's `tray.go`. Two things live here,
//! and they have
//! different shapes:
//!
//! * the **auto-start** setting, which is a string value in the user's `Run`
//!   key - `"<exe>" --tray` - that Windows runs at login. The rules around it
//!   (which value name this product owns, that the previous implementation's
//!   name counts as *this* setting, that disabling removes both) are here,
//!   behind [`RunKeyStore`], and are tested with a store in memory. Only the
//!   registry calls are platform work: see [`SystemRunKey`] and
//!   `lambo_process_windows::win32`.
//! * the **menu**, which is a list of command ids, labels and check marks. The
//!   window that shows it owns the Win32 side (`Shell_NotifyIcon`, `LoadImage`,
//!   `TrackPopupMenu`); what to show, and under which id, is here so that the
//!   GUI cannot drift from it.
//!
//! # The previous implementation's entry
//!
//! The previous implementation wrote its own product name as the value; this
//! product writes `Lambo PHP` (see the constants below). A
//! machine that had the old auto-start on would otherwise start two copies, and
//! the Settings page and the tray check mark would both read "off" while a
//! login launch still happened. So the old name counts as this setting when
//! read, and is removed whenever the setting is written or cleared.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::logs::LogFn;

/// The flag a login launch passes, so the window starts hidden in the tray.
///
/// The same spelling the original used, and the same one its `Run` value
/// carried.
pub const TRAY_FLAG: &str = "--tray";

/// The value name this product's auto-start entry uses.
pub const AUTOSTART_VALUE: &str = "Lambo PHP";

/// The value name the previous implementation's auto-start entry used.
pub const LEGACY_AUTOSTART_VALUE: &str = "GoAMPP";

/// The tray icon's file name, next to the executable.
///
/// The original refused to start the tray without it, rather than showing a
/// nameless, iconless entry that a user cannot find again.
pub const TRAY_ICON_FILE: &str = "logo.ico";

/// What the icon's tooltip says.
pub const TRAY_TOOLTIP: &str = "Lambo PHP — Local Web Stack";

/// The icon's id within its window.
pub const TRAY_ICON_UID: u32 = 1;

/// The application-defined message the shell calls the window back with.
///
/// `WM_APP + 1`, the first message a program may define for itself.
pub const TRAY_CALLBACK_MESSAGE: u32 = 0x8001;

/// The key Windows starts programs from at login.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The value name the Settings page and the tray read and write.
///
/// The `Run` key is the platform's, not this product's, so the store takes the
/// value name rather than assuming it.
pub trait RunKeyStore {
    /// The value, or `None` when it is not set.
    ///
    /// Errors keep whatever the platform says, which is what the original
    /// logged behind `auto-start: `.
    fn read(&self, value: &str) -> Result<Option<String>>;

    /// Sets the value to `command`.
    fn write(&self, value: &str, command: &str) -> Result<()>;

    /// Removes the value, reporting whether it was there.
    fn delete(&self, value: &str) -> Result<bool>;
}

/// A command in the tray menu, with the id the original gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    /// Bring the window back.
    Show,
    /// Start the services the stack needs.
    Start,
    /// Stop everything that is running.
    Stop,
    /// Turn the "start with Windows" setting on or off.
    ToggleAutoStart,
    /// Stop everything and exit.
    Quit,
}

impl TrayCommand {
    /// The command id the window receives.
    ///
    /// The original's numbers, kept because a menu is built and dispatched by
    /// number and there is nothing to gain from renumbering them.
    pub const fn id(self) -> u16 {
        match self {
            Self::Show => 40001,
            Self::Start => 40002,
            Self::Stop => 40003,
            Self::ToggleAutoStart => 40004,
            Self::Quit => 40005,
        }
    }

    /// The label, with the original's keyboard mnemonics and, where the
    /// original named the product, this one's name.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Show => "&Show Lambo PHP",
            Self::Start => "Start &Stack",
            Self::Stop => "Stop &All",
            Self::ToggleAutoStart => "&Auto-start with Windows",
            Self::Quit => "&Quit",
        }
    }
}

/// One line of the tray menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayItem {
    /// A divider.
    Separator,
    /// A command, with the check mark the settings give it.
    Command {
        /// What the line does.
        command: TrayCommand,
        /// Whether the line is checked, which only the auto-start line ever is.
        checked: bool,
    },
}

/// The menu, in the original's order: show, the two stack commands, the
/// auto-start toggle with its check mark, and quit.
pub fn menu(auto_start_enabled: bool) -> [TrayItem; 8] {
    let command = |command| TrayItem::Command {
        command,
        checked: false,
    };
    [
        command(TrayCommand::Show),
        TrayItem::Separator,
        command(TrayCommand::Start),
        command(TrayCommand::Stop),
        TrayItem::Separator,
        TrayItem::Command {
            command: TrayCommand::ToggleAutoStart,
            checked: auto_start_enabled,
        },
        TrayItem::Separator,
        command(TrayCommand::Quit),
    ]
}

/// The icon next to the executable, or the error the original reported.
///
/// `base_dir` is the directory the executable runs from, which is where the
/// installer puts `logo.ico`.
pub fn icon_path(base_dir: &Path) -> Result<PathBuf> {
    let icon = base_dir.join(TRAY_ICON_FILE);
    match std::fs::metadata(&icon) {
        Ok(_) => Ok(icon),
        Err(error) => Err(Error::InvalidInput(format!(
            "{TRAY_ICON_FILE} not found next to exe: {}: {error}",
            icon.display()
        ))),
    }
}

/// The auto-start setting, over a `Run` key.
///
/// The path to the executable is captured once: what a login launch runs is the
/// program that is running now, not wherever one happens to be later.
pub struct AutoStart<S> {
    store: S,
    exe: PathBuf,
}

impl<S: RunKeyStore> AutoStart<S> {
    /// The setting for `exe`, read and written through `store`.
    pub fn new(store: S, exe: impl Into<PathBuf>) -> Self {
        Self {
            store,
            exe: exe.into(),
        }
    }

    /// The store this setting writes through.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Whether a login launch is set up.
    ///
    /// Either value name counts: see the module documentation.
    pub fn is_enabled(&self) -> bool {
        matches!(self.store.read(AUTOSTART_VALUE), Ok(Some(_)))
            || matches!(self.store.read(LEGACY_AUTOSTART_VALUE), Ok(Some(_)))
    }

    /// What a login launch runs: the executable, quoted, and the tray flag.
    ///
    /// Quoted because the original quoted it, and because a `Run` value is a
    /// command line: an installation under `C:\Program Files\..` has a space in
    /// it.
    pub fn command_line(&self) -> String {
        format!("\"{}\" {TRAY_FLAG}", self.exe.display())
    }

    /// Turns the setting on or off.
    ///
    /// On: this product's value is written and the previous implementation's is
    /// removed, so exactly one entry starts at login. Off: both are removed. A
    /// value that was never there is not an error - the setting is off either
    /// way, and the original treated the missing value as one.
    pub fn set(&self, enabled: bool) -> Result<()> {
        if enabled {
            self.store.write(AUTOSTART_VALUE, &self.command_line())?;
            self.store.delete(LEGACY_AUTOSTART_VALUE)?;
            return Ok(());
        }

        self.store.delete(AUTOSTART_VALUE)?;
        self.store.delete(LEGACY_AUTOSTART_VALUE)?;
        Ok(())
    }

    /// The opposite of the current setting, applied and reported.
    ///
    /// A failure is logged behind `auto-start: ` and swallowed, which is what
    /// the original did: the tray has nowhere to put a dialog, and the check
    /// mark stays where it was because the setting did not change.
    pub fn toggle(&self, log: &LogFn) {
        let next = !self.is_enabled();
        match self.set(next) {
            Ok(()) if next => {
                log("auto-start enabled — Lambo PHP will launch into the tray on login")
            }
            Ok(()) => log("auto-start disabled"),
            Err(error) => log(&format!("auto-start: {error}")),
        }
    }

    /// What the Settings page shows for this setting.
    pub fn status_text(&self) -> &'static str {
        if self.is_enabled() {
            AUTO_START_ON
        } else {
            AUTO_START_OFF
        }
    }
}

/// The Settings row's text when a login launch is set up.
pub const AUTO_START_ON: &str = "on — launches into tray on login";

/// The Settings row's text when it is not.
pub const AUTO_START_OFF: &str = "off";

/// The `Run` key of this user, through the platform.
///
/// Windows only, because that is where a login launch is a registry value; on
/// any other platform the tray and its setting do not exist, exactly as the
/// original - which had no non-Windows build - did not have them.
#[cfg(windows)]
pub struct SystemRunKey;

#[cfg(windows)]
impl RunKeyStore for SystemRunKey {
    fn read(&self, value: &str) -> Result<Option<String>> {
        lambo_process_windows::win32::run_value(value)
            .map_err(|source| Error::InvalidInput(format!("open HKCU\\{RUN_KEY}: {source}")))
    }

    fn write(&self, value: &str, command: &str) -> Result<()> {
        lambo_process_windows::win32::set_run_value(value, command)
            .map_err(|source| Error::InvalidInput(format!("write {value}: {source}")))
    }

    fn delete(&self, value: &str) -> Result<bool> {
        lambo_process_windows::win32::delete_run_value(value)
            .map_err(|source| Error::InvalidInput(format!("delete {value}: {source}")))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// The `Run` key in memory, so the rules can be tested without a registry.
    #[derive(Default)]
    struct FakeRunKey {
        values: Mutex<BTreeMap<String, String>>,
        /// When set, every call fails with this message.
        failure: Option<&'static str>,
    }

    impl FakeRunKey {
        fn with(value: &str, command: &str) -> Self {
            let key = Self::default();
            key.values
                .lock()
                .expect("key lock")
                .insert(value.to_owned(), command.to_owned());
            key
        }

        fn failing(message: &'static str) -> Self {
            Self {
                values: Mutex::new(BTreeMap::new()),
                failure: Some(message),
            }
        }

        fn get(&self, value: &str) -> Option<String> {
            self.values.lock().expect("key lock").get(value).cloned()
        }

        fn check(&self) -> Result<()> {
            match self.failure {
                Some(message) => Err(Error::InvalidInput(message.to_owned())),
                None => Ok(()),
            }
        }
    }

    impl RunKeyStore for FakeRunKey {
        fn read(&self, value: &str) -> Result<Option<String>> {
            self.check()?;
            Ok(self.get(value))
        }

        fn write(&self, value: &str, command: &str) -> Result<()> {
            self.check()?;
            self.values
                .lock()
                .expect("key lock")
                .insert(value.to_owned(), command.to_owned());
            Ok(())
        }

        fn delete(&self, value: &str) -> Result<bool> {
            self.check()?;
            Ok(self
                .values
                .lock()
                .expect("key lock")
                .remove(value)
                .is_some())
        }
    }

    fn auto_start(store: FakeRunKey) -> AutoStart<FakeRunKey> {
        AutoStart::new(store, r"C:\lambo\lambo.exe")
    }

    fn recorder() -> (LogFn, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: LogFn = Arc::new(move |line: &str| {
            sink.lock().expect("log lock").push(line.to_owned());
        });
        (log, lines)
    }

    #[test]
    fn a_login_launch_runs_the_executable_with_the_tray_flag() {
        let setting = auto_start(FakeRunKey::default());
        setting.set(true).expect("the setting is written");

        // Quoted, and with the flag the window reads to start hidden.
        assert_eq!(
            setting.store().get(AUTOSTART_VALUE),
            Some(format!(r#""C:\lambo\lambo.exe" {TRAY_FLAG}"#))
        );
    }

    #[test]
    fn enabling_a_setting_that_is_already_on_writes_the_same_value() {
        let key = FakeRunKey::default();
        let setting = auto_start(key);
        setting.set(true).expect("the setting is written");
        setting.set(true).expect("writing it twice is not an error");

        assert!(setting.is_enabled());
        assert_eq!(
            setting.command_line(),
            format!(r#""{}" {TRAY_FLAG}"#, r"C:\lambo\lambo.exe")
        );
    }

    #[test]
    fn the_previous_implementations_entry_counts_as_this_setting() {
        // A machine that was set up by the original: the value name is its own,
        // and the check mark has to reflect that a launch is still going to
        // happen.
        let key = FakeRunKey::with(LEGACY_AUTOSTART_VALUE, r#""C:\goampp\goampp.exe" --tray"#);

        assert!(auto_start(key).is_enabled());
    }

    #[test]
    fn enabling_replaces_the_previous_implementations_entry() {
        let setting = auto_start(FakeRunKey::with(
            LEGACY_AUTOSTART_VALUE,
            r#""C:\goampp\goampp.exe" --tray"#,
        ));
        setting.set(true).expect("the setting is written");

        // Exactly one entry starts at login, and it is this product's.
        assert!(setting.store().get(LEGACY_AUTOSTART_VALUE).is_none());
        assert_eq!(
            setting.store().get(AUTOSTART_VALUE),
            Some(format!(r#""C:\lambo\lambo.exe" {TRAY_FLAG}"#))
        );
        assert!(setting.is_enabled());
    }

    #[test]
    fn disabling_removes_both_names() {
        let setting = auto_start(FakeRunKey::with(
            LEGACY_AUTOSTART_VALUE,
            r#""C:\goampp\goampp.exe" --tray"#,
        ));
        setting.set(true).expect("the setting is written");
        setting.set(false).expect("the setting is removed");

        assert!(setting.store().get(AUTOSTART_VALUE).is_none());
        assert!(setting.store().get(LEGACY_AUTOSTART_VALUE).is_none());
        assert!(!setting.is_enabled());
    }

    #[test]
    fn disabling_something_that_was_never_set_is_not_an_error() {
        let key = FakeRunKey::default();
        let setting = auto_start(key);
        setting
            .set(false)
            .expect("a missing value is not a failure");
        assert!(!setting.is_enabled());
    }

    #[test]
    fn a_store_that_fails_leaves_the_setting_alone() {
        let key = FakeRunKey::failing("access is denied");
        let setting = auto_start(key);

        let error = setting.set(true).expect_err("the failure is reported");
        assert!(error.to_string().contains("access is denied"), "{error}");
        assert!(!setting.is_enabled());
    }

    #[test]
    fn the_status_row_says_off_or_on() {
        let off = auto_start(FakeRunKey::default());
        assert_eq!(off.status_text(), "off");

        let on = auto_start(FakeRunKey::default());
        on.set(true).expect("the setting is written");
        assert_eq!(on.status_text(), "on — launches into tray on login");
    }

    #[test]
    fn toggling_logs_the_state_it_reached() {
        let (log, lines) = recorder();
        let setting = auto_start(FakeRunKey::default());

        setting.toggle(&log);
        assert_eq!(
            *lines.lock().expect("log lock"),
            vec!["auto-start enabled — Lambo PHP will launch into the tray on login".to_owned()]
        );

        setting.toggle(&log);
        let lines = lines.lock().expect("log lock");
        assert_eq!(
            lines.last().map(String::as_str),
            Some("auto-start disabled")
        );
    }

    #[test]
    fn a_toggle_that_fails_is_logged_and_not_reported_further() {
        let (log, lines) = recorder();
        let setting = auto_start(FakeRunKey::failing("access is denied"));

        setting.toggle(&log);
        let lines = lines.lock().expect("log lock");
        assert_eq!(
            lines.first().map(String::as_str),
            Some("auto-start: access is denied")
        );
        // The setting did not change, which is what the tray's check mark
        // reflects on its next open.
        assert!(!setting.is_enabled());
    }

    #[test]
    fn the_menu_is_the_originals_commands_in_order() {
        let items = menu(false);
        let commands: Vec<TrayCommand> = items
            .iter()
            .filter_map(|item| match item {
                TrayItem::Command { command, .. } => Some(*command),
                TrayItem::Separator => None,
            })
            .collect();

        assert_eq!(
            commands,
            vec![
                TrayCommand::Show,
                TrayCommand::Start,
                TrayCommand::Stop,
                TrayCommand::ToggleAutoStart,
                TrayCommand::Quit,
            ]
        );
        assert_eq!(
            items
                .iter()
                .filter(|item| **item == TrayItem::Separator)
                .count(),
            3
        );
        assert_eq!(TrayCommand::Show.id(), 40001);
        assert_eq!(TrayCommand::Quit.id(), 40005);
        assert_eq!(TrayCommand::Show.label(), "&Show Lambo PHP");
        assert_eq!(TrayCommand::Start.label(), "Start &Stack");
        assert_eq!(TrayCommand::Stop.label(), "Stop &All");
        assert_eq!(
            TrayCommand::ToggleAutoStart.label(),
            "&Auto-start with Windows"
        );
        assert_eq!(TrayCommand::Quit.label(), "&Quit");
    }

    #[test]
    fn only_the_auto_start_line_carries_a_check_mark() {
        let checked: Vec<TrayCommand> = menu(true)
            .iter()
            .filter_map(|item| match item {
                TrayItem::Command {
                    command,
                    checked: true,
                } => Some(*command),
                _ => None,
            })
            .collect();
        assert_eq!(checked, vec![TrayCommand::ToggleAutoStart]);

        assert_eq!(
            menu(false),
            menu(true).map(|item| match item {
                TrayItem::Command { command, .. } => TrayItem::Command {
                    command,
                    checked: false,
                },
                TrayItem::Separator => TrayItem::Separator,
            })
        );
    }

    #[test]
    fn the_icon_has_to_be_next_to_the_executable() {
        let temp = crate::testutil::TempDir::new();

        let error = icon_path(temp.path()).expect_err("a missing icon is an error");
        assert!(
            error
                .to_string()
                .starts_with("logo.ico not found next to exe: "),
            "{error}"
        );

        std::fs::write(temp.join(TRAY_ICON_FILE), b"icon").expect("failed to write the fixture");
        assert_eq!(
            icon_path(temp.path()).expect("the icon is there"),
            temp.join("logo.ico")
        );
    }
}
