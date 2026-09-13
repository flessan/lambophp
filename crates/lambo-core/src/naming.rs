//! Naming helpers: directory names become slugs, database names and URLs.
//!
//! `acme/My Shop!` -> `my-shop` -> `my_shop` -> `http://localhost:8080`.
//! Pure functions, no I/O - which is why they are exhaustively testable and
//! behave identically on every platform.

/// Converts an arbitrary project or directory name into a URL-safe slug.
///
/// Lowercases, replaces every non-alphanumeric run with a single dash, and
/// trims leading/trailing dashes. Falls back to `app` when nothing usable
/// remains so callers never have to handle the empty case.
pub fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut last_was_dash = true; // suppresses leading dashes
    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "app".to_owned()
    } else {
        slug
    }
}

/// Default local development domain for a project (`my-app.test`).
///
/// `.test` is reserved by RFC 2606 and can never collide with a real TLD.
/// Lambo's default workflow uses `localhost` and a port instead of per-project
/// domains (no hosts-file editing, no administrator rights), so this is only
/// used when a project explicitly asks for a domain.
pub fn default_domain(project_name: &str) -> String {
    format!("{}.test", slugify(project_name))
}

/// Derives a MySQL/MariaDB database name from a project name.
///
/// MySQL identifiers may not start with a digit and read best with
/// underscores, so `My Shop 2` becomes `my_shop_2`. The result is at most 64
/// characters (MySQL's identifier limit) and never empty.
pub fn database_name(project_name: &str) -> String {
    let slug = slugify(project_name).replace('-', "_");
    let trimmed: String = slug.chars().take(64).collect();
    let trimmed = trimmed.trim_end_matches('_');
    let candidate = if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
        format!("app_{trimmed}")
    } else {
        trimmed.to_owned()
    };
    if candidate.is_empty() {
        "app".to_owned()
    } else {
        candidate
    }
}

/// Whether `name` is usable as a MySQL/MariaDB database identifier.
///
/// Deliberately strict: Lambo quotes identifiers itself, so it never has to
/// reason about escaping, and a project file that names a database
/// `my db` is rejected at validation time rather than at `lambo db create`.
pub fn is_valid_database_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() && first != '_' {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Builds the local URL of a project.
///
/// Uses `localhost` and a port rather than a custom domain so that nothing
/// has to be written to the hosts file - on Windows that would need
/// administrator rights, which Lambo's default workflow must not require.
pub fn local_url(port: u16) -> String {
    match port {
        80 => "http://localhost".to_owned(),
        443 => "https://localhost".to_owned(),
        port => format!("http://localhost:{port}"),
    }
}

/// The `127.0.0.1:port` form used for database connections.
///
/// `127.0.0.1` rather than `localhost`: on Windows `localhost` can resolve to
/// `::1` first, and a MySQL server bound to IPv4 only then appears down.
pub fn database_host_and_port(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_handles_real_world_names() {
        assert_eq!(slugify("My Shop!"), "my-shop");
        assert_eq!(slugify("  Leading and trailing  "), "leading-and-trailing");
        assert_eq!(slugify("already-good"), "already-good");
        assert_eq!(slugify("Version_2.0"), "version-2-0");
        // Non-ASCII letters are separators, not letters: slugs stay pure
        // ASCII so they are always valid as domains, database names and
        // directory names on every platform.
        assert_eq!(slugify("Ünïcode Nàme"), "n-code-n-me");
        assert_eq!(slugify("café"), "caf");
    }

    #[test]
    fn slugify_never_returns_empty() {
        assert_eq!(slugify(""), "app");
        assert_eq!(slugify("!!!"), "app");
    }

    #[test]
    fn default_domain_appends_test_tld() {
        assert_eq!(default_domain("My Shop"), "my-shop.test");
    }

    #[test]
    fn database_names_are_mysql_safe() {
        assert_eq!(database_name("My Shop"), "my_shop");
        assert_eq!(database_name("my-project"), "my_project");
        assert_eq!(database_name("2fast"), "app_2fast");
        assert_eq!(database_name("!!!"), "app");
        assert_eq!(database_name(""), "app");
        assert!(is_valid_database_name(&database_name("My Shop 2")));
    }

    #[test]
    fn database_names_respect_the_identifier_limit() {
        let long = "a".repeat(100);
        assert_eq!(database_name(&long).len(), 64);
    }

    #[test]
    fn database_name_validation_is_strict() {
        for name in ["shop", "shop_dev", "_private", "a1"] {
            assert!(is_valid_database_name(name), "{name} must be accepted");
        }
        for name in [
            "",
            "Shop",
            "1shop",
            "my db",
            "my-db",
            "shop;drop",
            &"a".repeat(65),
        ] {
            assert!(!is_valid_database_name(name), "{name} must be rejected");
        }
    }

    #[test]
    fn local_urls_omit_the_default_port() {
        assert_eq!(local_url(8080), "http://localhost:8080");
        assert_eq!(local_url(80), "http://localhost");
        assert_eq!(local_url(443), "https://localhost");
    }

    #[test]
    fn database_connections_use_the_ipv4_literal() {
        assert_eq!(database_host_and_port(3306), "127.0.0.1:3306");
    }
}
