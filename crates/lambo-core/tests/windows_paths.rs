//! Windows path handling, asserted on every platform.
//!
//! `Os` is an argument rather than a compile-time switch, so Windows path
//! shapes, generated configuration and command lines can be - and are -
//! verified while running on Linux. A Windows-only regression therefore fails
//! CI on every runner instead of waiting for somebody to try it on a PC.
//!
//! The cases here are the ones that break in practice: spaces in a user's
//! project directory, a Lambo home installed somewhere with a space in it, and
//! runtime paths built from either. None of this goes through a shell, which is
//! asserted rather than assumed, because a shell would silently re-split every
//! one of these paths.
//!
//! Physical Windows execution - real `httpd.exe`, real service behaviour, a real
//! desktop browser - is still a manual smoke test; see `docs/windows.md`.

use std::path::{Path, PathBuf};

use lambo_core::apache::{self, Alias, Apache, PhpIntegration, Plan};
use lambo_core::config::Config;
use lambo_core::naming;
use lambo_core::paths::Paths;
use lambo_core::platform::Os;
use lambo_core::process::{self, Output, ProcessSpec};
use lambo_core::runtime::{InstalledRuntime, RuntimeKind};

/// A temporary directory that removes itself.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let unique = format!(
            "lambo-winpath-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).expect("temporary directory");
        Self { path }
    }

    fn join(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

/// Renders a path the way generated configuration writes it.
///
/// Apache's parser treats a backslash as an escape, so Lambo emits forward
/// slashes; that conversion is what these assertions check.
fn configured(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

// ---------------------------------------------------------------------------
// The Lambo home
// ---------------------------------------------------------------------------

#[test]
fn a_lambo_home_with_a_space_in_it_keeps_every_subdirectory_intact() {
    let paths = Paths::from_root(r"C:\Program Files\Lambo");

    // Each directory is the root plus one component; a space in the root must
    // not shift anything or introduce quoting.
    assert_eq!(configured(paths.root()), "C:/Program Files/Lambo");
    assert_eq!(configured(&paths.php_dir()), "C:/Program Files/Lambo/php");
    assert_eq!(
        configured(&paths.config_file()),
        "C:/Program Files/Lambo/config/lambo.yml"
    );
    assert_eq!(
        configured(&paths.state_file()),
        "C:/Program Files/Lambo/data/services.yml"
    );
    assert_eq!(
        configured(&paths.runtime_version_dir(RuntimeKind::Php, "8.4.2")),
        "C:/Program Files/Lambo/php/8.4.2"
    );

    // The executable name gains the platform extension.
    assert_eq!(
        Os::Windows.executable_name("php"),
        "php.exe",
        "Windows executables carry .exe"
    );
}

#[test]
fn the_default_data_home_stays_under_the_users_own_profile() {
    // Two different directories, and the distinction matters: the installer
    // puts the *program* under %LOCALAPPDATA%\Programs\LamboPHP, while this is
    // where Lambo keeps its *data*. Both are per-user, so neither needs
    // administrator rights and neither collides with a machine-wide install.
    use lambo_core::platform::default_data_dir;

    let windows = default_data_dir(Os::Windows, r"C:\Users\dev").expect("windows home");
    assert_eq!(configured(&windows), "C:/Users/dev/Lambo");

    // The Unix equivalent is hidden, the Windows one is not: a dot-prefixed
    // directory is conventional on Unix and merely awkward on Windows.
    let unix = default_data_dir(Os::Linux, "/home/dev").expect("unix home");
    assert_eq!(unix, Path::new("/home/dev/.lambo"));

    // No profile at all is an error, not a silent fallback to the filesystem
    // root, which would scatter runtimes somewhere the user cannot find.
    assert!(default_data_dir(Os::Windows, "").is_err());
}

// ---------------------------------------------------------------------------
// Generated Apache configuration
// ---------------------------------------------------------------------------

/// A Windows Apache installation under a directory with a space in it.
fn spaced_apache() -> Apache {
    let root = PathBuf::from(r"C:\Program Files\Lambo\apache\2.4.62");
    Apache {
        runtime: None,
        executable: root.join("bin").join("httpd.exe"),
        server_root: root,
    }
}

#[test]
fn a_project_directory_with_spaces_survives_into_the_apache_configuration() {
    let paths = Paths::from_root(r"C:\Program Files\Lambo");
    let document_root = PathBuf::from(r"C:\Projects\my project");

    let plan = Plan {
        apache: spaced_apache(),
        port: 8080,
        document_root: document_root.clone(),
        project_name: "my project".to_owned(),
        php: Some(PhpIntegration {
            version: "8.4.2".to_owned(),
            module: PathBuf::from(r"C:\Program Files\Lambo\php\8.4.2\php8apache2_4.dll"),
            ini_dir: PathBuf::from(r"C:\Program Files\Lambo\php\8.4.2"),
        }),
        allow_override: true,
        directory_index: vec!["index.php".to_owned(), "index.html".to_owned()],
        // A mounted application whose path contains a space, which is the case
        // that breaks an unquoted Alias directive.
        aliases: vec![Alias {
            path: "/phpmyadmin".to_owned(),
            directory: PathBuf::from(r"C:\Users\Jane Doe\Lambo\dbui\phpmyadmin"),
        }],
    };

    let written = apache::write_config(&paths, &plan, Os::Windows).expect("config written");
    let config = std::fs::read_to_string(&written).expect("config readable");

    // The whole point: the spaced document root appears as one intact path,
    // quoted, with the spaces preserved.
    assert!(
        config.contains("\"C:/Projects/my project\""),
        "the document root lost its spaces or its quoting: {config}"
    );
    assert!(
        config.contains("C:/Program Files/Lambo/php/8.4.2/php8apache2_4.dll"),
        "the module path with a space is wrong: {config}"
    );
    assert!(
        config.contains("\"C:/Program Files/Lambo/php/8.4.2\""),
        "PHPIniDir lost its quoting: {config}"
    );

    // Apache treats a backslash as an escape, so no path may keep one. The only
    // legitimate backslash is the `\.php$` regex inside <FilesMatch>.
    for line in config.lines().filter(|line| !line.contains("FilesMatch")) {
        assert!(
            !line.contains('\\'),
            "a backslash would be read as an escape by Apache: {line}"
        );
    }

    std::fs::remove_file(&written).ok();
}

// ---------------------------------------------------------------------------
// Command lines: never through a shell
// ---------------------------------------------------------------------------

#[test]
fn a_runtime_and_project_with_spaces_are_passed_as_single_arguments() {
    // Real directories, because `serve_spec` refuses a runtime whose executable
    // is not there - a refusal that is itself worth keeping. The concern under
    // test is spaces surviving intact, which is the same on every platform.
    let temp = TempDir::new();
    let root = temp.join("My Projects/shop");
    std::fs::create_dir_all(&root).expect("project directory");
    std::fs::write(root.join("index.php"), "<?php echo 'hi';\n").unwrap();

    let runtime_dir = temp.join("Lambo Home/php/8.4.2");
    std::fs::create_dir_all(&runtime_dir).expect("runtime directory");
    let os = Os::host();
    let executable = runtime_dir.join(os.executable_name("php"));
    std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
    make_executable(&executable);

    let runtime = InstalledRuntime {
        kind: RuntimeKind::Php,
        name: "8.4.2".to_owned(),
        version: "8.4.2".parse().expect("version"),
        path: runtime_dir.clone(),
    };
    let log = temp.join("Lambo Home/logs/php/server.log");

    let spec = lambo_core::php::serve_spec(&runtime, 8080, &root, &log, os).expect("spec");

    // The executable is the full path, not a bare name resolved via PATH and
    // not a string handed to a shell.
    assert_eq!(spec.program, executable);

    // The document root is exactly one argument, spaces and all. If it ever
    // became two, `-t` would receive only the first word and the server would
    // quietly serve the wrong directory.
    let docroot_index = spec
        .args
        .iter()
        .position(|arg| arg == "-t")
        .expect("serve_spec passes -t");
    assert_eq!(spec.args[docroot_index + 1], root.display().to_string());
    assert!(
        spec.args[docroot_index + 1].contains(' '),
        "the test is only meaningful with a space in the path"
    );

    // Output goes to a file, not to a shell redirection.
    assert_eq!(spec.stdout, Output::File(log.clone()));
    assert_eq!(spec.stderr, Output::File(log));
}

#[test]
fn shell_metacharacters_in_a_path_stay_inside_their_argument() {
    // A directory named with shell syntax is unusual but legal on Windows. The
    // argument list must carry it verbatim as one item; the moment anything
    // joins these into a string for `cmd.exe`, this becomes code execution.
    let hostile = r"C:\Projects\my project; rm -rf C:\ & echo pwned";
    let spec = ProcessSpec::new(r"C:\Lambo\php\php.exe", "php")
        .arg("-t")
        .arg(hostile);

    assert_eq!(spec.args.len(), 2, "the argument was split");
    assert_eq!(spec.args[1], hostile, "the argument was rewritten");

    // Arguments are handed to the operating system as a list, which is why the
    // metacharacters are inert. This is the property the whole module rests on.
    assert!(
        process::is_safe_argument(hostile),
        "a path with shell syntax is still a legitimate single argument"
    );
    assert!(
        !process::is_safe_argument("ok\nrm -rf C:\\"),
        "a newline must be rejected: it is the one thing that can smuggle a second command"
    );
}

#[test]
fn the_rendered_command_line_quotes_everything_a_human_would_need_quoted() {
    // `render` is only for logs and error messages, but it is what a user
    // copies and pastes, so it has to be a command line that actually works.
    let spec = ProcessSpec::new(
        r"C:\Program Files\Lambo\apache\2.4.62\bin\httpd.exe",
        "apache",
    )
    .arg("-f")
    .arg(r"C:\Program Files\Lambo\config\httpd.conf")
    .arg("-k")
    .arg("start");

    assert_eq!(
        spec.render(),
        r#""C:\Program Files\Lambo\apache\2.4.62\bin\httpd.exe" -f "C:\Program Files\Lambo\config\httpd.conf" -k start"#
    );
}

// ---------------------------------------------------------------------------
// URLs and naming
// ---------------------------------------------------------------------------

#[test]
fn the_local_url_never_carries_a_windows_path() {
    // Project configuration must stay portable, so nothing platform-absolute
    // may leak into what a user commits.
    assert_eq!(naming::local_url(8080), "http://localhost:8080");
    // The default port is what makes the promise `http://localhost` rather
    // than a URL the user has to remember. Windows binds 80 without elevation.
    assert_eq!(Config::default().server.port, 80);
    assert_eq!(
        naming::local_url(Config::default().server.port),
        "http://localhost",
        "the default configuration must produce a bare localhost URL"
    );
}
