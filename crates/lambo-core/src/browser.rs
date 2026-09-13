//! Opening a URL in the user's own browser.
//!
//! `lambo up` (unless `--no-browser`), `lambo open` and `lambo db open` all
//! end by handing a URL to the desktop.
//! Two things matter here:
//!
//! - **No shell.** On Windows the obvious one-liner is `cmd /C start <url>`,
//!   which hands the URL to a command interpreter - and a query string such as
//!   `?server=127.0.0.1:3306&db=shop` then contains the `&` that cmd uses to
//!   chain commands. Lambo instead invokes
//!   `rundll32 url.dll,FileProtocolHandler <url>`, a plain process start with
//!   the URL as a single argument, which is the same mechanism Python's own
//!   `webbrowser` module falls back to. `explorer.exe <url>` is the second
//!   choice, also without a shell. A URL can never become a command.
//! - **The URL is validated anyway.** Defence in depth: only `http://` and
//!   `https://` URLs with none of the characters that a shell, a terminal or a
//!   browser extension could reinterpret are accepted, so even a future change
//!   that reintroduces a shell cannot be turned into an injection.
//!
//! Opening a browser can legitimately fail - a headless CI runner, an SSH
//! session, a container. That is not an error worth failing a `lambo up` over,
//! so [`open`] returns the outcome and callers report the URL for the user to
//! open by hand.

use crate::error::{Error, Result};
use crate::platform::Os;
use crate::process::{self, Output, ProcessSpec};

/// Longest URL Lambo will hand to a browser.
pub const MAX_URL_LENGTH: usize = 2048;

/// Characters that never appear in a URL Lambo generates, and that a shell or
/// terminal would give special meaning to.
///
/// `&` is *not* here: it is a normal query-string separator and Lambo's own
/// database-manager URLs use it. It is safe because the URL is always a single
/// argument to a single program - never part of a command line that a shell
/// re-parses.
const REJECTED: &[char] = &['"', '\'', '`', '<', '>', '|', '\\', '^', '$'];

/// Whether a URL is safe to hand to the operating system.
pub fn is_safe_url(url: &str) -> bool {
    if url.is_empty() || url.len() > MAX_URL_LENGTH {
        return false;
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return false;
    }
    if url
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return false;
    }
    !url.chars().any(|character| REJECTED.contains(&character))
}

/// The command that opens a URL, with its fallback.
///
/// Returns `(primary, fallback)`; the fallback is `None` where the platform has
/// a single mechanism.
pub fn opener(os: Os, url: &str) -> (ProcessSpec, Option<ProcessSpec>) {
    match os {
        Os::Windows => {
            let primary = ProcessSpec::new("rundll32.exe", "browser")
                .args(["url.dll,FileProtocolHandler", url])
                .stdout(Output::Null)
                .stderr(Output::Null);
            // `explorer.exe` also hands a URL to the default browser, and like
            // rundll32 it takes it as one argument rather than a command line.
            let fallback = ProcessSpec::new("explorer.exe", "browser")
                .arg(url)
                .stdout(Output::Null)
                .stderr(Output::Null);
            (primary, Some(fallback))
        }
        Os::MacOs => {
            let primary = ProcessSpec::new("open", "browser")
                .arg(url)
                .stdout(Output::Null)
                .stderr(Output::Null);
            (primary, None)
        }
        // Linux and the BSDs: `xdg-open` is the freedesktop standard, `wslview`
        // covers WSL where there is no browser inside the distribution.
        _ => {
            let primary = ProcessSpec::new("xdg-open", "browser")
                .arg(url)
                .stdout(Output::Null)
                .stderr(Output::Null);
            let fallback = ProcessSpec::new("wslview", "browser")
                .arg(url)
                .stdout(Output::Null)
                .stderr(Output::Null);
            (primary, Some(fallback))
        }
    }
}

/// Opens a URL in the default browser.
///
/// The child is left running on purpose: the browser is the user's process, not
/// Lambo's, and waiting for it would make `lambo up` hang until the browser
/// exits.
pub fn open(url: &str, os: Os) -> Result<()> {
    if !is_safe_url(url) {
        return Err(Error::InvalidInput(format!(
            "`{url}` is not a URL Lambo will open: only http:// and https:// addresses are supported"
        )));
    }

    let (primary, fallback) = opener(os, url);
    if process::spawn(&primary, os).is_ok() {
        return Ok(());
    }

    match fallback {
        Some(fallback) if process::spawn(&fallback, os).is_ok() => Ok(()),
        Some(fallback) => Err(Error::ServiceFailed {
            service: "the browser".to_owned(),
            reason: format!(
                "no way to open a browser was found on this machine; open {url} manually"
            ),
            causes: vec![
                format!("{} could not be started", primary.program.display()),
                format!("{} could not be started", fallback.program.display()),
                "there may be no graphical session (SSH, container, CI)".to_owned(),
            ],
            hint: Some(format!("open {url} manually")),
        }),
        None => Err(Error::ServiceFailed {
            service: "the browser".to_owned(),
            reason: format!(
                "{} could not be started; open {url} manually",
                primary.program.display()
            ),
            causes: vec!["there may be no graphical session (SSH, container, CI)".to_owned()],
            hint: Some(format!("open {url} manually")),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_http_urls_are_opened() {
        for good in [
            "http://localhost:8080",
            "https://localhost:8443/",
            "http://127.0.0.1:8081/?server=127.0.0.1:3306&username=root&db=shop",
            "https://example.com/path?query=value#fragment",
        ] {
            assert!(is_safe_url(good), "`{good}` should be accepted");
        }

        for bad in [
            "",
            "localhost:8080",
            "file:///etc/passwd",
            "ftp://example.com/x",
            "javascript:alert(1)",
            "http://localhost:8080/\" & calc.exe",
            "http://localhost:8080/`whoami`",
            "http://localhost:8080/\n",
            "http://localhost:8080/ path",
            "C:\\Lambo\\index.html",
            "http://localhost:8080/$HOME",
        ] {
            assert!(!is_safe_url(bad), "`{bad}` must be refused");
        }
    }

    #[test]
    fn a_ridiculously_long_url_is_refused() {
        let long = format!("http://localhost:8080/{}", "a".repeat(MAX_URL_LENGTH));
        assert!(!is_safe_url(&long));
        assert!(is_safe_url(&format!(
            "http://localhost:8080/{}",
            "a".repeat(64)
        )));
    }

    #[test]
    fn windows_opens_without_a_command_interpreter() {
        let url = "http://127.0.0.1:8081/?server=127.0.0.1:3306&username=root&db=shop";
        assert!(
            is_safe_url(url),
            "Lambo's own database-manager URL must be openable"
        );

        let (primary, fallback) = opener(Os::Windows, url);
        let rendered = primary.render();

        assert_eq!(primary.program.display().to_string(), "rundll32.exe");
        assert!(
            rendered.contains("url.dll,FileProtocolHandler"),
            "{rendered}"
        );
        // The URL is exactly one argument, so the `&` in the query string
        // cannot split it into two commands.
        assert_eq!(primary.args, ["url.dll,FileProtocolHandler", url]);

        let fallback = fallback.expect("Windows keeps a second opener");
        assert_eq!(fallback.program.display().to_string(), "explorer.exe");
        assert_eq!(fallback.args, [url]);
        for spec in [primary, fallback] {
            assert!(
                !spec.program.display().to_string().contains("cmd"),
                "no command interpreter"
            );
        }
    }

    #[test]
    fn each_platform_uses_its_own_opener() {
        assert_eq!(
            opener(Os::MacOs, "http://x/")
                .0
                .program
                .display()
                .to_string(),
            "open"
        );
        assert!(opener(Os::MacOs, "http://x/").1.is_none());
        assert_eq!(
            opener(Os::Linux, "http://x/")
                .0
                .program
                .display()
                .to_string(),
            "xdg-open"
        );
        let (primary, fallback) = opener(Os::Linux, "http://x/");
        assert_eq!(primary.program.display().to_string(), "xdg-open");
        assert_eq!(fallback.unwrap().program.display().to_string(), "wslview");
    }

    #[test]
    fn an_unsafe_url_never_reaches_the_operating_system() {
        // On a machine with no browser at all this still fails on validation,
        // not on a missing program: the URL is checked first.
        let error = open("file:///etc/passwd", Os::host()).unwrap_err();
        assert!(matches!(error, Error::InvalidInput(_)), "{error:?}");
        assert!(
            error.to_string().contains("not a URL Lambo will open"),
            "{error}"
        );
    }

    #[test]
    fn a_missing_browser_is_reported_with_the_url_to_open() {
        // `open` on a headless Linux runner cannot start a browser. Whatever the
        // outcome, the message must name the URL so the user is not stuck.
        let url = "http://localhost:8080";
        let result = open(url, Os::host());
        if let Err(error) = result {
            assert!(error.to_string().contains(url), "{error}");
        }
    }
}
