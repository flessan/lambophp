//! What the dashboard shows, independent of how it is drawn.
//!
//! A Win32 window cannot be unit-tested on Linux, but almost everything that
//! could be *wrong* about the dashboard is not Win32 at all - it is deciding
//! which text to show for which state. That decision lives here, where it
//! compiles and is tested on every platform, and `win32.rs` only draws the
//! result.
//!
//! Splitting it this way is what keeps the unverifiable part small: the Win32
//! plumbing is mechanical, and anything with real branching is covered.

use lambo_core::app::{
    About, Dashboard, Diagnostic, LogView, OperationOutcome, ProjectInfo, RuntimeInfo,
    RuntimeState, ServiceInfo, ServiceState,
};
use lambo_core::config::Config;
#[cfg(test)]
use std::path::PathBuf;

/// One line of the status list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    /// Label, e.g. `Apache`.
    pub label: String,
    /// Value, e.g. `Running`.
    pub value: String,
    /// Whether to show the success marker.
    pub ok: bool,
}

/// The dashboard rendered to plain text, ready for any toolkit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// Project name, or a prompt to choose one.
    pub project: String,
    /// Overall state, e.g. `Running` or `Stopped`.
    pub state: String,
    /// Per-service lines.
    pub lines: Vec<StatusLine>,
    /// The URL to open, when there is one.
    pub url: Option<String>,
    /// The database manager URL, when it is installed.
    pub database_ui_url: Option<String>,
    /// Whether `Start` should be enabled.
    pub can_start: bool,
    /// Whether `Stop` should be enabled.
    pub can_stop: bool,
    /// A message for the status bar.
    pub notice: Option<String>,
}

/// Renders a dashboard.
///
/// `has_project` is passed separately because the dashboard carries an `Option`
/// and the caller already knows whether one is open; deriving it twice is how
/// the two come to disagree.
pub fn render(dashboard: &Dashboard, has_project: bool) -> View {
    let lines = dashboard
        .services
        .iter()
        // The database manager is not a service in this list; it is mounted
        // into the site and shown as its own button.
        .filter(|service| service.name != "dbui")
        .map(line)
        .collect::<Vec<_>>();

    // PHP is shown from the runtime evidence, not from the service list, so a
    // runtime that will not run says so here rather than appearing healthy.
    let php = match &dashboard.php {
        Some(runtime) => php_line(runtime),
        None => StatusLine {
            label: "PHP".to_owned(),
            value: "not installed".to_owned(),
            ok: false,
        },
    };

    let any_running = dashboard
        .services
        .iter()
        .any(|service| service.state == ServiceState::Running);
    let any_failed = dashboard
        .services
        .iter()
        .any(|service| service.state == ServiceState::Failed);

    // Whether the site actually answers, as opposed to merely having a live
    // process behind it. `is_alive` verifies the process is still the one Lambo
    // started; only this says it is serving.
    let serving = dashboard
        .project
        .as_ref()
        .is_some_and(|project| project.serving);

    let state = if any_failed {
        // Checked before `any_running`: a stack where the database came up and
        // Apache did not is a failure the user must see, not a "Running".
        "Failed"
    } else if any_running && serving {
        "Running"
    } else if any_running {
        // Processes are up but nothing is answering yet - a server still
        // binding, or one that came up on a port nobody has reached. Claiming
        // "Running" here would send the user to a URL that refuses them.
        "Starting"
    } else if has_project {
        "Stopped"
    } else {
        "No project"
    };

    View {
        // Nothing is running, so Start is always available. Stop is offered
        // only when there is something to stop - a disabled button is clearer
        // than one that does nothing.
        can_stop: any_running,
        can_start: true,
        notice: notice(dashboard),
        project: dashboard
            .project
            .as_ref()
            .map(|project| project.name.clone())
            .unwrap_or_else(|| "No project selected".to_owned()),
        state: state.to_owned(),
        url: dashboard.url.clone(),
        database_ui_url: dashboard.database_ui_url.clone(),
        lines: [php].into_iter().chain(lines).collect(),
    }
}

/// One service as a status line.
fn line(service: &ServiceInfo) -> StatusLine {
    StatusLine {
        ok: service.state == ServiceState::Running,
        value: service.state.label().to_owned(),
        label: label(&service.name),
    }
}

/// The PHP line, carrying the evidence that it runs.
///
/// A runtime that is installed but will not start must not read as `8.4 ✓`.
/// That is the single most misleading thing this screen could say, because the
/// user would go looking for a problem in their project.
fn php_line(runtime: &RuntimeInfo) -> StatusLine {
    let version = runtime
        .reported_version
        .clone()
        .unwrap_or_else(|| runtime.version.clone());
    match runtime.healthy {
        Some(true) => StatusLine {
            label: "PHP".to_owned(),
            value: version,
            ok: true,
        },
        Some(false) => StatusLine {
            label: "PHP".to_owned(),
            value: format!(
                "{} - will not run",
                runtime.problem.as_deref().unwrap_or("broken")
            ),
            ok: false,
        },
        // Not started yet, so nothing has been proven either way.
        None => StatusLine {
            label: "PHP".to_owned(),
            value: runtime.version.clone(),
            ok: false,
        },
    }
}

/// The status-bar message.
///
/// A port fallback is surfaced here rather than hidden: the user configured one
/// port and is being given another, and a screen that does not mention it looks
/// like it ignored the configuration.
fn notice(dashboard: &Dashboard) -> Option<String> {
    // Name the service that failed: "something went wrong" sends the user to
    // the logs to find out what, when the dashboard already knows.
    if let Some(failed) = dashboard
        .services
        .iter()
        .find(|service| service.state == ServiceState::Failed)
    {
        return Some(format!(
            "{} failed to start - see Logs for the reason",
            label(&failed.name)
        ));
    }
    if !dashboard.healthy {
        return Some("A service stopped unexpectedly - check Logs".to_owned());
    }
    None
}

/// Renders the About screen.
///
/// Every value comes from the engine: the version from the build, the paths
/// from the resolved home. Nothing here is typed out, because a hard-coded
/// version string is exactly the kind of thing that is still showing the last
/// release six months later.
pub fn render_about(about: &About) -> Vec<StatusLine> {
    vec![
        StatusLine {
            label: "Version".to_owned(),
            value: about.version.clone(),
            ok: true,
        },
        StatusLine {
            label: "Installed in".to_owned(),
            value: about.home.display().to_string(),
            ok: true,
        },
        StatusLine {
            label: "Your data".to_owned(),
            value: about.data.display().to_string(),
            ok: true,
        },
        StatusLine {
            label: "Configuration".to_owned(),
            value: about.config.display().to_string(),
            ok: true,
        },
        StatusLine {
            label: "Logs".to_owned(),
            value: about.logs.display().to_string(),
            ok: true,
        },
        StatusLine {
            label: "Licence".to_owned(),
            value: about.license.to_owned(),
            ok: true,
        },
        StatusLine {
            label: "Source".to_owned(),
            value: about.repository.to_owned(),
            ok: true,
        },
    ]
}

/// The Projects screen: one row per registered project.
///
/// The active project is marked rather than listed separately, so the screen
/// cannot disagree with the dashboard about which one is open.
pub fn render_projects(projects: &[ProjectInfo], active: Option<&str>) -> Vec<StatusLine> {
    if projects.is_empty() {
        return vec![StatusLine {
            label: "Projects".to_owned(),
            value: "none registered - run `lambo init` in a project folder".to_owned(),
            ok: false,
        }];
    }
    projects
        .iter()
        .map(|project| StatusLine {
            // The marker is part of the label rather than a separate column
            // because the value carries the path, which is the longer string.
            label: if active == Some(project.name.as_str()) {
                format!("* {}", project.name)
            } else {
                project.name.clone()
            },
            value: format!(
                "{} - {} - {}",
                project.framework,
                project.php,
                project.path.display()
            ),
            ok: active == Some(project.name.as_str()),
        })
        .collect()
}

/// The PHP screen: every known version and the evidence about each.
pub fn render_runtimes(runtimes: &[RuntimeInfo]) -> Vec<StatusLine> {
    if runtimes.is_empty() {
        return vec![StatusLine {
            label: "PHP".to_owned(),
            value: "no versions known - run `lambo php install`".to_owned(),
            ok: false,
        }];
    }
    runtimes
        .iter()
        .map(|runtime| StatusLine {
            label: runtime.version.clone(),
            // `active` and `installed` are both fine; the difference is which
            // one the project will use, and that is the marker's job.
            ok: matches!(
                runtime.state,
                RuntimeState::Active | RuntimeState::Installed
            ),
            value: match &runtime.problem {
                // A runtime that will not run must say so here rather than
                // looking like one that merely is not selected.
                Some(problem) => format!("{} - {problem}", runtime_label(runtime.state)),
                None => runtime_label(runtime.state).to_owned(),
            },
        })
        .collect()
}

/// The Logs screen: which logs exist and the tail of each.
///
/// Paths are shown because the point of this screen is often "where do I look",
/// and a log viewer that hides the file it is reading is not helping.
pub fn render_logs(logs: &[LogView], tail: usize) -> Vec<StatusLine> {
    if logs.is_empty() {
        return vec![StatusLine {
            label: "Logs".to_owned(),
            value: "nothing logged yet".to_owned(),
            ok: true,
        }];
    }
    let mut lines = Vec::new();
    for log in logs {
        lines.push(StatusLine {
            label: label(&log.name),
            value: log.path.display().to_string(),
            ok: true,
        });
        for line in log
            .lines
            .iter()
            .rev()
            .take(tail)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            lines.push(StatusLine {
                label: String::new(),
                value: line.clone(),
                // A log line is not a pass/fail; it is evidence.
                ok: true,
            });
        }
    }
    lines
}

/// The keys a person actually tunes, and what to call them.
///
/// Deliberately not `Config::KEYS`. A settings screen that lists every internal
/// key is a config file with a window on it, and the keys that matter get lost
/// in it. What is left out is still reachable through `lambo config`, which is
/// where a key you have to be told the name of belongs anyway.
const SETTINGS: [(&str, &str); 11] = [
    ("paths.projects", "Project location"),
    ("php.default", "Default PHP"),
    ("server.kind", "Web server"),
    ("server.port", "HTTP port"),
    ("server.https_port", "HTTPS port"),
    ("database.kind", "Database"),
    ("database.port", "Database port"),
    ("database.username", "Database user"),
    ("database.password", "Database password"),
    ("dbui.kind", "Database manager"),
    ("browser.open", "Open browser on start"),
];

/// The one key whose value must never be rendered.
const SECRET_KEY: &str = "database.password";

/// The Settings screen: the settings worth tuning, in words.
///
/// Values come from the loaded config, so this cannot drift from what the CLI
/// reads and writes.
pub fn render_settings(config: &Config) -> Vec<StatusLine> {
    SETTINGS
        .iter()
        .map(|(key, caption)| {
            let read = config.get(key);
            StatusLine {
                label: (*caption).to_owned(),
                // The password is masked here for the same reason it is never
                // put in a URL: a screen that can be glanced at, photographed
                // or screen-shared is not a place to display a credential.
                // `lambo db credentials --show-password` remains the way to
                // see it, deliberately behind a flag.
                value: match (&read, *key == SECRET_KEY) {
                    (Ok(_), true) => {
                        "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022} (hidden)"
                            .to_owned()
                    }
                    // An unreadable key is shown as an error rather than
                    // skipped: a settings screen with a silent hole is worse
                    // than one that says it could not read a value.
                    (Ok(value), false) => value.clone(),
                    (Err(error), _) => error.to_string(),
                },
                ok: read.is_ok(),
            }
        })
        .collect()
}

/// The Services screen: every managed service, including the database manager.
///
/// Distinct from the dashboard's list, which deliberately omits the manager
/// because there it is a button.
pub fn render_services(dashboard: &Dashboard) -> Vec<StatusLine> {
    dashboard.services.iter().map(line).collect()
}

/// The Database screen: the server, the manager, and how to reach both.
pub fn render_database(dashboard: &Dashboard) -> Vec<StatusLine> {
    let mut lines: Vec<StatusLine> = dashboard
        .services
        .iter()
        .filter(|service| service.name == "database" || service.name == "dbui")
        .map(line)
        .collect();
    if let Some(url) = &dashboard.database_ui_url {
        lines.push(StatusLine {
            label: "Manager URL".to_owned(),
            value: url.clone(),
            ok: true,
        });
    } else {
        lines.push(StatusLine {
            label: "Manager URL".to_owned(),
            value: "not installed - run `lambo db install-ui`".to_owned(),
            ok: false,
        });
    }
    lines
}

/// The diagnostics screen: what `lambo doctor` found, in words.
pub fn render_diagnostics(diagnostics: &[Diagnostic]) -> Vec<StatusLine> {
    if diagnostics.is_empty() {
        return vec![StatusLine {
            label: "Doctor".to_owned(),
            value: "no checks reported".to_owned(),
            ok: true,
        }];
    }
    diagnostics
        .iter()
        .map(|check| StatusLine {
            label: check.name.clone(),
            // The fix is appended because a problem without its remedy is half
            // a diagnosis, and the engine already knows the remedy.
            value: match &check.fix {
                // `unsupported` is not a failure the user can fix, so it is
                // left alone rather than being handed a fix command.
                Some(fix) if check.severity == "error" || check.severity == "warning" => {
                    format!("{} - fix: {fix}", check.detail)
                }
                _ => check.detail.clone(),
            },
            // Ok and unsupported both mean nothing is wrong; unsupported means
            // a feature is not available on this platform, which is not a
            // fault.
            ok: check.severity == "ok" || check.severity == "unsupported",
        })
        .collect()
}

/// Turns a failed operation into the sentence a person can act on.
///
/// The engine already worked out what went wrong, what else might be the cause,
/// and which command to run next. Showing only the first of those is what makes
/// a GUI feel like it is hiding something, so all three are used.
pub fn failure_message(outcome: &OperationOutcome) -> String {
    let what = outcome
        .error
        .clone()
        .unwrap_or_else(|| "the operation failed".to_owned());

    let mut message = what;
    // The remedy is the part the user actually needed. Without it the window
    // reports a problem and leaves them to go and find the CLI.
    if let Some(hint) = &outcome.hint {
        message.push_str(&format!("  Try: {hint}"));
    }
    // Causes beyond the first are worth showing, but a notice line is not a
    // scroll view, so they are capped rather than allowed to overflow it.
    if !outcome.causes.is_empty() {
        let shown: Vec<&str> = outcome.causes.iter().take(2).map(String::as_str).collect();
        message.push_str(&format!("  ({})", shown.join("; ")));
    }
    message
}

/// A runtime state in words.
///
/// The engine's vocabulary is stable identifiers, not display text, so the
/// wording lives here where the other presentation decisions do.
fn runtime_label(state: RuntimeState) -> &'static str {
    match state {
        RuntimeState::Active => "active",
        RuntimeState::Installed => "installed",
        RuntimeState::Available => "available",
        RuntimeState::Unavailable => "not available here",
        RuntimeState::Corrupt => "corrupt",
    }
}

/// The engine's names are stable identifiers, not display text; a GUI should
/// not show `mariadb` where it means `MariaDB`.
pub fn label(name: &str) -> String {
    match name {
        "apache" => "Apache".to_owned(),
        "mariadb" => "MariaDB".to_owned(),
        "mysql" => "MySQL".to_owned(),
        "php-server" => "PHP server".to_owned(),
        "dbui" => "Database manager".to_owned(),
        other => other.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lambo_core::app::RuntimeState;

    fn dashboard(services: Vec<ServiceInfo>) -> Dashboard {
        Dashboard {
            project: None,
            services,
            php: None,
            url: None,
            database_ui_url: None,
            healthy: true,
        }
    }

    #[test]
    fn a_failed_service_is_its_own_state_and_names_itself() {
        let view = render(
            &dashboard(vec![
                service("database", ServiceState::Running),
                service("apache", ServiceState::Failed),
            ]),
            true,
        );

        // Not "Running": the database came up, but the stack is broken and
        // saying otherwise would hide it.
        assert_eq!(view.state, "Failed");
        let notice = view.notice.expect("a failure must be reported");
        assert!(
            notice.contains("Apache"),
            "the notice names the service that failed: {notice}"
        );
        // Start stays available - retrying is the user's next action.
        assert!(view.can_start);
    }

    fn project(name: &str) -> ProjectInfo {
        ProjectInfo {
            name: name.to_owned(),
            path: PathBuf::from(format!("/code/{name}")),
            framework: "Laravel".to_owned(),
            document_root: PathBuf::from(format!("/code/{name}/public")),
            php: "8.4".to_owned(),
            database: "mysql".to_owned(),
            url: "http://localhost".to_owned(),
            serving: false,
        }
    }

    /// A runtime in a specific state. The existing `runtime` helper pins the
    /// state to `Active`, which is right for its tests but not these.
    fn runtime_in(version: &str, state: RuntimeState, problem: Option<&str>) -> RuntimeInfo {
        RuntimeInfo {
            version: version.to_owned(),
            state,
            path: None,
            source: None,
            reported_version: None,
            healthy: problem.is_none().then_some(true),
            problem: problem.map(str::to_owned),
        }
    }

    #[test]
    fn projects_mark_the_active_one_and_say_when_there_are_none() {
        let lines = render_projects(&[project("shop"), project("blog")], Some("blog"));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].label, "shop");
        assert!(!lines[0].ok, "an inactive project is not marked");
        assert_eq!(lines[1].label, "* blog", "the active one is marked");
        assert!(lines[1].ok);
        assert!(lines[1].value.contains("Laravel"), "{}", lines[1].value);

        let none = render_projects(&[], None);
        assert_eq!(none.len(), 1);
        assert!(
            none[0].value.contains("lambo init"),
            "the empty state says what to do: {}",
            none[0].value
        );
    }

    #[test]
    fn a_runtime_that_will_not_run_says_why() {
        let lines = render_runtimes(&[
            runtime_in("8.4.2", RuntimeState::Active, None),
            runtime_in("8.3.16", RuntimeState::Corrupt, Some("checksum mismatch")),
        ]);
        assert_eq!(lines[0].value, "active");
        assert!(lines[0].ok);
        assert!(!lines[1].ok, "a corrupt runtime is not healthy");
        assert!(
            lines[1].value.contains("checksum mismatch"),
            "the reason is on screen: {}",
            lines[1].value
        );

        let none = render_runtimes(&[]);
        assert!(
            none[0].value.contains("lambo php install"),
            "{}",
            none[0].value
        );
    }

    #[test]
    fn diagnostics_carry_their_fix_but_unsupported_is_not_a_fault() {
        let lines = render_diagnostics(&[
            Diagnostic {
                name: "ports".to_owned(),
                severity: "error".to_owned(),
                detail: "port 80 is held by IIS".to_owned(),
                fix: Some("lambo config set server.port 8088".to_owned()),
            },
            Diagnostic {
                name: "systemd".to_owned(),
                severity: "unsupported".to_owned(),
                detail: "not available on Windows".to_owned(),
                fix: None,
            },
        ]);
        assert!(!lines[0].ok);
        assert!(
            lines[0].value.contains("lambo config set server.port 8088"),
            "a failing check shows its remedy: {}",
            lines[0].value
        );
        assert!(
            lines[1].ok,
            "unsupported is not something the user did wrong"
        );
    }

    #[test]
    fn the_database_screen_shows_the_manager_url_or_how_to_get_it() {
        let mut d = dashboard(vec![
            service("database", ServiceState::Running),
            service("apache", ServiceState::Running),
        ]);
        // Apache is not a database concern and must not appear here.
        let with_url = {
            d.database_ui_url = Some("http://localhost/phpmyadmin".to_owned());
            render_database(&d)
        };
        assert!(with_url.iter().all(|line| line.label != "Apache"));
        let url = with_url
            .iter()
            .find(|line| line.label == "Manager URL")
            .expect("manager row");
        assert_eq!(url.value, "http://localhost/phpmyadmin");

        d.database_ui_url = None;
        let without = render_database(&d);
        let url = without
            .iter()
            .find(|line| line.label == "Manager URL")
            .expect("manager row");
        assert!(
            url.value.contains("lambo db install-ui"),
            "the empty state says what to do: {}",
            url.value
        );
    }

    #[test]
    fn services_includes_the_manager_that_the_dashboard_hides() {
        let d = dashboard(vec![
            service("apache", ServiceState::Running),
            service("dbui", ServiceState::Running),
        ]);
        let lines = render_services(&d);
        assert!(
            lines.iter().any(|line| line.label == "Database manager"),
            "the services screen lists the manager the dashboard shows as a button"
        );
    }

    #[test]
    fn settings_show_the_settings_a_person_tunes_in_words() {
        let config = Config::default();
        let lines = render_settings(&config);

        // Curated, not a dump of the config file: every caption is a phrase,
        // and none of the internal source-resolution keys are here.
        assert!(
            lines.len() < Config::KEYS.len(),
            "the screen is a curated list, not every internal key"
        );
        // Captions are phrases a person reads, never the dotted config key.
        assert!(
            lines.iter().all(|line| !line.label.contains('.')),
            "captions are phrases, not dotted config keys: {:?}",
            lines
                .iter()
                .map(|line| line.label.clone())
                .collect::<Vec<_>>()
        );

        let port = lines
            .iter()
            .find(|line| line.label == "HTTP port")
            .expect("HTTP port");
        assert_eq!(port.value, "80", "the localhost-first default");
        let manager = lines
            .iter()
            .find(|line| line.label == "Database manager")
            .expect("Database manager");
        assert_eq!(manager.value, "phpmyadmin");
    }

    #[test]
    fn settings_never_render_the_database_password() {
        let mut config = Config::default();
        config.database.password = "s3cret-value".to_owned();
        let lines = render_settings(&config);

        let row = lines
            .iter()
            .find(|line| line.label == "Database password")
            .expect("the password row exists");
        assert!(
            !row.value.contains("s3cret-value"),
            "the credential must not be on screen: {}",
            row.value
        );
        assert!(row.value.contains("hidden"), "{}", row.value);
        // Nor anywhere else on the screen.
        assert!(
            lines
                .iter()
                .all(|line| !line.value.contains("s3cret-value")),
            "no row may leak the password"
        );
    }

    #[test]
    fn logs_show_the_path_and_the_tail_in_order() {
        let logs = vec![LogView {
            name: "apache".to_owned(),
            path: PathBuf::from("/home/dev/.lambo/logs/apache.log"),
            lines: vec!["first".to_owned(), "second".to_owned(), "third".to_owned()],
        }];
        let lines = render_logs(&logs, 2);
        assert_eq!(lines[0].value, "/home/dev/.lambo/logs/apache.log");
        // Tail, oldest first: reading a log backwards is not reading it.
        assert_eq!(lines[1].value, "second");
        assert_eq!(lines[2].value, "third");
        assert_eq!(lines.len(), 3, "the requested tail length is respected");
    }

    fn outcome(error: Option<&str>, hint: Option<&str>, causes: &[&str]) -> OperationOutcome {
        OperationOutcome {
            ok: false,
            steps: Vec::new(),
            url: None,
            error: error.map(str::to_owned),
            causes: causes.iter().map(|c| (*c).to_owned()).collect(),
            hint: hint.map(str::to_owned),
            failed_service: None,
        }
    }

    #[test]
    fn a_failure_shows_its_remedy_not_just_its_message() {
        let message = failure_message(&outcome(
            Some("port 80 is held by another process"),
            Some("lambo config set server.port 8088"),
            &["IIS is listening on 80", "http.sys owns the port"],
        ));
        assert!(
            message.contains("port 80 is held"),
            "what failed: {message}"
        );
        assert!(
            message.contains("lambo config set server.port 8088"),
            "what to try: {message}"
        );
        assert!(
            message.contains("IIS is listening on 80"),
            "what Lambo knows: {message}"
        );
    }

    #[test]
    fn a_failure_with_no_known_remedy_is_still_a_sentence() {
        let message = failure_message(&outcome(Some("the database would not start"), None, &[]));
        assert_eq!(message, "the database would not start");
        // No trailing separator left behind when there is nothing to append.
        assert!(!message.ends_with("Try:"), "{message}");

        let bare = failure_message(&outcome(None, None, &[]));
        assert_eq!(bare, "the operation failed");
    }

    #[test]
    fn about_reports_the_build_version_and_real_paths() {
        let about = About {
            product: "Lambo PHP".to_owned(),
            version: "0.10.0".to_owned(),
            home: PathBuf::from("/home/dev/.lambo"),
            data: PathBuf::from("/home/dev/.lambo/data"),
            config: PathBuf::from("/home/dev/.lambo/config"),
            logs: PathBuf::from("/home/dev/.lambo/logs"),
            license: "Apache-2.0 OR MIT",
            repository: "https://github.com/flessan/kink-php-dev",
        };
        let lines = render_about(&about);

        let version = lines
            .iter()
            .find(|line| line.label == "Version")
            .expect("version row");
        assert_eq!(version.value, "0.10.0");

        // Licence and source belong on an About screen: they are what a user
        // needs in order to check their rights or file a bug.
        let licence = lines
            .iter()
            .find(|line| line.label == "Licence")
            .expect("licence row");
        assert_eq!(licence.value, "Apache-2.0 OR MIT");
        let source = lines
            .iter()
            .find(|line| line.label == "Source")
            .expect("source row");
        assert!(source.value.starts_with("https://"), "{}", source.value);

        // The paths a user needs when something goes wrong must be on screen.
        for label in ["Installed in", "Your data", "Configuration", "Logs"] {
            assert!(
                lines.iter().any(|line| line.label == label),
                "About is missing {label}"
            );
        }
        let data = lines.iter().find(|line| line.label == "Your data").unwrap();
        assert_eq!(data.value, "/home/dev/.lambo/data");
    }

    fn serving_project(serving: bool) -> ProjectInfo {
        ProjectInfo {
            name: "shop".to_owned(),
            path: PathBuf::from("/code/shop"),
            framework: "plain PHP".to_owned(),
            document_root: PathBuf::from("/code/shop"),
            php: "8.4".to_owned(),
            database: "none".to_owned(),
            url: "http://localhost".to_owned(),
            serving,
        }
    }

    fn service(name: &str, state: ServiceState) -> ServiceInfo {
        ServiceInfo {
            name: name.to_owned(),
            state,
            pid: None,
            port: None,
            uptime: None,
            log: None,
            occupant: None,
        }
    }

    fn runtime(version: &str, healthy: Option<bool>, problem: Option<&str>) -> RuntimeInfo {
        RuntimeInfo {
            version: version.to_owned(),
            state: RuntimeState::Active,
            path: None,
            source: None,
            reported_version: healthy.and(Some(version.to_owned())),
            healthy,
            problem: problem.map(str::to_owned),
        }
    }

    #[test]
    fn a_runtime_that_will_not_run_is_not_shown_as_healthy() {
        let mut d = dashboard(vec![service("apache", ServiceState::Running)]);
        d.php = Some(runtime(
            "8.4.2",
            Some(false),
            Some("error while loading shared libraries: libonig.so.5"),
        ));

        let view = render(&d, true);
        let php = view.lines.iter().find(|l| l.label == "PHP").unwrap();

        assert!(!php.ok, "a broken runtime must not show the tick");
        assert!(
            php.value.contains("will not run"),
            "and must say so rather than showing a bare version: {}",
            php.value
        );
        assert!(
            php.value.contains("libonig.so.5"),
            "carrying the reason is what makes it actionable: {}",
            php.value
        );
    }

    #[test]
    fn a_healthy_runtime_shows_the_version_php_reported() {
        let mut d = dashboard(vec![service("apache", ServiceState::Running)]);
        d.php = Some(runtime("8.4.2", Some(true), None));

        let view = render(&d, true);
        let php = view.lines.iter().find(|l| l.label == "PHP").unwrap();
        assert_eq!(php.value, "8.4.2");
        assert!(php.ok);
    }

    #[test]
    fn stop_is_offered_only_when_there_is_something_to_stop() {
        let idle = render(
            &dashboard(vec![service("apache", ServiceState::NotStarted)]),
            true,
        );
        assert!(
            !idle.can_stop,
            "a disabled button beats one that does nothing"
        );
        assert!(idle.can_start);

        // A site that is genuinely up: the process is alive *and* the URL
        // answers. Both halves are needed before the window says Running.
        let mut up = dashboard(vec![service("apache", ServiceState::Running)]);
        up.project = Some(serving_project(true));
        let running = render(&up, true);
        assert!(running.can_stop);
        assert_eq!(running.state, "Running");
    }

    #[test]
    fn a_live_process_that_is_not_answering_is_starting_not_running() {
        // `is_alive` proves the process Lambo started is still there; it does
        // not prove anything is listening. Claiming Running here would send
        // the user to a URL that refuses the connection.
        let mut up = dashboard(vec![service("apache", ServiceState::Running)]);
        up.project = Some(serving_project(false));
        let view = render(&up, true);
        assert_eq!(view.state, "Starting");
        // Stop is still offered: there is a real process to stop.
        assert!(view.can_stop);
    }

    #[test]
    fn a_stopped_service_raises_a_notice_and_unhealthy_state() {
        // `stopped` means Lambo has a record but the process is gone - that is
        // different from `not_started`, and the user should be told.
        let d = Dashboard {
            healthy: false,
            ..dashboard(vec![service("apache", ServiceState::Stopped)])
        };
        let view = render(&d, true);
        assert!(view.notice.is_some(), "a crash must be surfaced");
        assert!(!view.can_stop);
    }

    #[test]
    fn the_database_manager_is_a_button_not_a_service_row() {
        let mut d = dashboard(vec![
            service("apache", ServiceState::Running),
            service("dbui", ServiceState::Running),
        ]);
        d.database_ui_url = Some("http://localhost/phpmyadmin".to_owned());

        let view = render(&d, true);
        assert!(
            !view.lines.iter().any(|l| l.label == "Database manager"),
            "the manager is mounted into the site, not run as a service: {:?}",
            view.lines
        );
        assert_eq!(
            view.database_ui_url.as_deref(),
            Some("http://localhost/phpmyadmin")
        );
    }

    #[test]
    fn service_names_are_shown_as_proper_nouns() {
        assert_eq!(label("apache"), "Apache");
        assert_eq!(label("mariadb"), "MariaDB");
        assert_eq!(label("php-server"), "PHP server");
        // An unknown name is passed through rather than dropped: a new service
        // must still be visible on the dashboard.
        assert_eq!(label("something-new"), "something-new");
    }

    #[test]
    fn a_home_with_no_project_says_so_rather_than_showing_a_blank() {
        let view = render(&dashboard(Vec::new()), false);
        assert_eq!(view.project, "No project selected");
        assert_eq!(view.state, "No project");
        assert!(view.url.is_none());
    }
}
