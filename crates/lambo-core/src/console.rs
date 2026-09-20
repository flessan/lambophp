//! The two consoles the panel opens.
//!
//! The original panel's services page puts a `⌨ Term` button on every language
//! runtime and
//! its settings page an `Open psql Console` button. Both start a console the
//! user types into, and both are decided the same way whether the panel opens
//! them or a command does: which terminal, what goes on `PATH`, what the window
//! is called, and what happens when the tool is not installed.
//!
//! The terminal is the original's: ConEmu when the installation carries one
//! (`{base}/installer/ConEmu64.exe`, which its bundled installer drops there),
//! and `cmd.exe` otherwise. Either way the child is a console program started
//! from a windowed process, so it is given a console window of its own - the
//! window *is* the feature, and the helper-console suppression every other
//! spawned process gets would hide it.
//!
//! Nothing here is a shell string except the one `cmd` is asked to run for
//! PostgreSQL, which is the operating system's own command line and not a Lambo
//! one: the outer invocation is an argument vector, as everywhere else.

use std::path::{Path, PathBuf};

use crate::process::ProcessSpec;

/// The environment variable a console finds the installation through.
///
/// The previous implementation exported its own base-directory variable; this
/// is the rebranded name, and the legacy variable is *not* also set: a terminal
/// is the user's own shell, not a compatibility surface, and two names for one
/// path would be worse than one.
pub const BASE_ENV: &str = "LAMBO_BASE";

/// Where ConEmu lives inside an installation, when it was installed with one.
pub const CONEMU_PATH: &str = "installer/ConEmu64.exe";

/// The console program, which is what every terminal ends up being.
pub const CONSOLE_PROGRAM: &str = "cmd.exe";

/// What the settings page says when PostgreSQL is not there.
pub const PSQL_MISSING: &str =
    "psql: PostgreSQL not installed — install it from the Services tab first";

/// What the PostgreSQL console greets the user with.
pub const PSQL_PROMPT: &str = "PostgreSQL console — try: psql -U postgres";

/// Where PostgreSQL's client programs live inside an installation.
pub const PSQL_BIN_DIR: &str = "bin/pgsql/bin";

/// The directories a language's terminal puts at the front of `PATH`, relative
/// to the installation root.
///
/// The original's `langBinDirs`, entry for entry, including the languages that
/// have none (`Swift`) and the ones with two (`Python`, `Elixir`).
pub fn language_bin_dirs(name: &str) -> &'static [&'static str] {
    match name {
        "Node.js" => &["bin/node"],
        "Python" => &["bin/python", "bin/python/Scripts"],
        "Go" => &["bin/go/bin"],
        "Java" => &["bin/java/bin"],
        "Julia" => &["bin/julia/bin"],
        "Zig" => &["bin/zig"],
        "Dart" => &["bin/dart/bin"],
        "Lua" => &["bin/lua"],
        "Ruby" => &["bin/ruby/bin"],
        "Rust" => &["bin/rust/.cargo/bin"],
        "Kotlin" => &["bin/kotlin/bin"],
        "Haskell" => &["bin/haskell/bin"],
        "Elixir" => &["bin/elixir/bin", "bin/erlang/bin"],
        "Crystal" => &["bin/crystal"],
        "Scala" => &["bin/scala/bin"],
        "Erlang" => &["bin/erlang/bin"],
        _ => &[],
    }
}

/// The title of a service's terminal window: `Lambo PHP — Node.js`.
///
/// `Swift` is in the original's table with no directories, which is why it still
/// gets a terminal: the card's button is there for the runtime the user built
/// projects with, not for the paths.
pub fn terminal_title(name: &str) -> String {
    format!("{} — {}", crate::PRODUCT, name)
}

/// The `PATH` a language's terminal starts with: the language's own directories
/// first, then whatever the panel inherited.
///
/// Joined with `;`, which is the separator the console this serves reads, on the
/// platform it serves it on.
pub fn terminal_path(base_dir: &Path, name: &str, inherited: &str) -> String {
    let mut entries: Vec<String> = language_bin_dirs(name)
        .iter()
        .map(|dir| base_dir.join(dir).display().to_string())
        .collect();
    entries.push(inherited.to_owned());
    entries.join(";")
}

/// The ConEmu the installation carries, when it carries one.
pub fn conemu(base_dir: &Path) -> Option<PathBuf> {
    let path = base_dir.join(CONEMU_PATH);
    path.is_file().then_some(path)
}

/// A language's terminal, ready to start.
///
/// `inherited_path` is the `PATH` the panel is running with - a process read,
/// which is the interface's to make and the decision here.
pub fn terminal(base_dir: &Path, name: &str, inherited_path: &str) -> ProcessSpec {
    let spec = match conemu(base_dir) {
        Some(conemu) => ProcessSpec::new(conemu, "terminal").args([
            "/Title",
            &terminal_title(name),
            "/cmd",
            CONSOLE_PROGRAM,
        ]),
        None => ProcessSpec::new(CONSOLE_PROGRAM, "terminal"),
    };

    // The user's console: its own window, and its own standard handles rather
    // than the null ones a captured helper gets.
    spec.console()
        .interactive()
        .env("PATH", terminal_path(base_dir, name, inherited_path))
        .env(BASE_ENV, base_dir.display().to_string())
}

/// Whether PostgreSQL's client programs are installed.
pub fn psql_installed(base_dir: &Path) -> bool {
    base_dir.join(PSQL_BIN_DIR).join("psql.exe").is_file()
}

/// What the settings page's console button found.
#[derive(Debug, Clone)]
pub enum PsqlConsole {
    /// PostgreSQL is not installed: the original logged one line and stopped.
    NotInstalled,
    /// The console to open.
    Ready(Box<ProcessSpec>),
}

/// PostgreSQL's console, as the original opened it: `cmd` starts a second `cmd`
/// that keeps reading the user's input, with the client programs on `PATH` and
/// the installation as the working directory.
pub fn psql_console(base_dir: &Path) -> PsqlConsole {
    if !psql_installed(base_dir) {
        return PsqlConsole::NotInstalled;
    }

    let bin = base_dir.join(PSQL_BIN_DIR);
    let command = format!(
        "set PATH={};%PATH% && cd /d {} && echo {}",
        bin.display(),
        base_dir.display(),
        PSQL_PROMPT
    );

    PsqlConsole::Ready(Box::new(
        ProcessSpec::new("cmd", "psql console")
            .args(["/c", "start", "", "cmd", "/K", &command])
            .interactive()
            .console(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    // `Output` is what the tests assert about a console's handles; the module
    // itself never names it, which is why it is imported here.
    use crate::process::Output;
    use crate::testutil::TempDir;

    fn spec_env(spec: &ProcessSpec, key: &str) -> String {
        spec.env.get(key).cloned().unwrap_or_default()
    }

    #[test]
    fn a_terminal_carries_the_languages_directories_and_the_installation() {
        let temp = TempDir::new();
        let base = temp.path();
        let spec = terminal(base, "Node.js", "C:\\Windows");

        assert_eq!(spec.program, PathBuf::from(CONSOLE_PROGRAM));
        assert_eq!(spec.args, Vec::<String>::new());
        assert!(
            spec.new_console,
            "a terminal is a console the user asked for"
        );
        assert_eq!(
            spec_env(&spec, "PATH"),
            format!("{};C:\\Windows", base.join("bin/node").display()),
            "the language's directory comes first"
        );
        assert_eq!(
            spec_env(&spec, BASE_ENV),
            base.display().to_string(),
            "the console can find the installation"
        );
        assert_eq!(
            (spec.stdin.clone(), spec.stdout.clone(), spec.stderr.clone()),
            (Output::Inherit, Output::Inherit, Output::Inherit),
            "the user's console owns its own handles"
        );
    }

    #[test]
    fn a_two_directory_language_prepends_both_of_them() {
        let temp = TempDir::new();
        let base = temp.path();
        let spec = terminal(base, "Python", "C:\\Windows");

        assert_eq!(
            spec_env(&spec, "PATH"),
            format!(
                "{};{};C:\\Windows",
                base.join("bin/python").display(),
                base.join("bin/python/Scripts").display()
            )
        );
        // Elixir keeps its own directory *and* Erlang's, in that order.
        assert_eq!(
            language_bin_dirs("Elixir"),
            &["bin/elixir/bin", "bin/erlang/bin"]
        );
    }

    #[test]
    fn a_language_with_no_directories_still_gets_a_terminal() {
        let temp = TempDir::new();
        let base = temp.path();

        // Swift is in the original's table with an empty list; a language the
        // table does not know behaves the same way.
        for name in ["Swift", "PHP"] {
            assert_eq!(language_bin_dirs(name), &[] as &[&str], "{name}");
            let spec = terminal(base, name, "C:\\Windows");
            assert_eq!(spec_env(&spec, "PATH"), "C:\\Windows", "{name}");
        }
    }

    #[test]
    fn the_terminal_prefers_the_conemu_the_installation_carries() {
        let temp = TempDir::new();
        let base = temp.path();
        let conemu = base.join(CONEMU_PATH);
        std::fs::create_dir_all(conemu.parent().expect("it has one")).expect("the installer dir");
        std::fs::write(&conemu, b"MZ").expect("a ConEmu");

        let spec = terminal(base, "Go", "C:\\Windows");

        assert_eq!(spec.program, conemu);
        assert_eq!(
            spec.args,
            vec![
                "/Title".to_owned(),
                terminal_title("Go"),
                "/cmd".to_owned(),
                CONSOLE_PROGRAM.to_owned(),
            ]
        );
        assert_eq!(
            terminal_title("Go"),
            format!("{} — Go", crate::PRODUCT),
            "the window is this product's"
        );
    }

    #[test]
    fn the_psql_console_says_so_when_postgresql_is_not_installed() {
        let temp = TempDir::new();
        assert!(matches!(
            psql_console(temp.path()),
            PsqlConsole::NotInstalled
        ));
        assert!(!psql_installed(temp.path()));
        assert!(PSQL_MISSING.starts_with("psql: "));
    }

    #[test]
    fn the_psql_console_runs_the_installed_client() {
        let temp = TempDir::new();
        let base = temp.path();
        let bin = base.join(PSQL_BIN_DIR);
        std::fs::create_dir_all(&bin).expect("the client directory");
        std::fs::write(bin.join("psql.exe"), b"MZ").expect("psql");

        let PsqlConsole::Ready(spec) = psql_console(base) else {
            panic!("psql is installed");
        };

        assert_eq!(spec.program, PathBuf::from("cmd"));
        assert_eq!(
            spec.args,
            vec![
                "/c".to_owned(),
                "start".to_owned(),
                String::new(),
                "cmd".to_owned(),
                "/K".to_owned(),
                format!(
                    "set PATH={};%PATH% && cd /d {} && echo {}",
                    bin.display(),
                    base.display(),
                    PSQL_PROMPT
                ),
            ],
            "the original's own command line, with the installation's paths"
        );
        assert!(spec.new_console, "the console is the point of the button");
    }
}
