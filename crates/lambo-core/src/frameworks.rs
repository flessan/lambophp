//! The framework catalogue and the project scaffolder.
//!
//! This is the port of the original implementation's `frameworks.go`: seventeen
//! frameworks, the
//! tools each one needs, the command steps that scaffold it, the files written
//! afterwards, the port its dev server answers on, and `create_project`, which
//! scaffolds a framework into `{base}/www/<name>` and registers the project and
//! its virtual host in the installation document.
//!
//! Three properties of the original drive the shape of this module:
//!
//! 1. **The catalogue is data, and the data is the behaviour.** The table, the
//!    command steps, the proxy ports and the post-files are transcribed
//!    verbatim, down to the argv vectors and the starter files, because the GUI
//!    and the CLI both present them and both must present the same thing.
//! 2. **The scaffolding runs the tool, not a shell.** Every step is an argument
//!    vector handed to [`process::spawn_piped`], so nothing a project name
//!    contains can turn into a second command.
//! 3. **Registration is not this module's business to persist.** `create_project`
//!    knows *what* to record - a [`PanelProject`] and a [`Vhost`] - and asks a
//!    [`ProjectSink`] to write them. The real sink loads and saves the panel
//!    document (`session.rs`, `PanelDocument`); tests use an in-memory one, which
//!    is how the rules on top of the document are verified without a JSON file.
//!
//! Branding: the scaffolded files and the log lines say Lambo PHP where the
//! original named its predecessor product. Nothing else about them changed.

use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::download::{http_download, nop_progress};
use crate::error::{Error, Result};
use crate::logs::LogFn;
use crate::panel::{PanelProject, Vhost};
use crate::platform::Os;
use crate::process::{self, ProcessSpec};
use crate::{archive, download_cache, vhost};

/// What kind of scaffold a framework uses.
///
/// The original stored this as a string and dispatched on it, with an
/// `unknown framework kind` error for anything else. The type is an enum here so
/// the dispatch cannot be forgotten, and [`Kind::from_key`] still reports the
/// original's error for a key that is not one of the four.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `composer create-project`.
    Composer,
    /// A release archive, extracted with the wrapper directory stripped.
    Download,
    /// One or more commands, optionally followed by a starter file.
    Cmd,
    /// A single boilerplate `index.html`.
    Static,
}

impl Kind {
    /// The key used in the catalogue document.
    pub fn key(self) -> &'static str {
        match self {
            Self::Composer => "composer",
            Self::Download => "download",
            Self::Cmd => "cmd",
            Self::Static => "static",
        }
    }

    /// Parses a catalogue key, or reports the original's error.
    pub fn from_key(key: &str) -> Result<Self> {
        match key {
            "composer" => Ok(Self::Composer),
            "download" => Ok(Self::Download),
            "cmd" => Ok(Self::Cmd),
            "static" => Ok(Self::Static),
            other => Err(Error::InvalidInput(format!(
                "unknown framework kind: {other}"
            ))),
        }
    }
}

/// One scaffoldable framework.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Framework {
    /// Display name; also the catalogue key of its command steps and post-file.
    pub name: &'static str,
    /// Icon file name under `assets/icons/`, or empty.
    pub icon_file: &'static str,
    /// Runtime family: `php`, `node`, `python`, `go`, `java` or `static`.
    pub runtime: &'static str,
    /// One-line description shown next to the name.
    pub description: &'static str,
    /// Tools that must be on `PATH` (or in the installation) to scaffold it.
    pub required_tools: &'static [&'static str],
    /// How it is scaffolded.
    pub kind: Kind,
    /// Composer package for [`Kind::Composer`].
    pub composer_package: &'static str,
    /// Release URL for [`Kind::Download`].
    pub download_url: &'static str,
    /// Wrapper directory to strip when extracting a [`Kind::Download`] archive.
    pub strip_top: &'static str,
    /// Document root below the project directory; empty means the directory
    /// itself.
    pub doc_root: &'static str,
}

/// One command of a [`Kind::Cmd`] scaffold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmdStep {
    /// What the step is called in the log and in an error message.
    pub desc: &'static str,
    /// The argument vector, `argv[0]` first.
    pub args: &'static [&'static str],
}

/// A file written after a [`Kind::Cmd`] scaffold has run its steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PostFile {
    /// Path relative to the project directory.
    pub path: &'static str,
    /// The file's contents.
    pub content: &'static str,
}

/// Every framework, in the order both interfaces list them.
///
/// The README claimed seventeen scaffolders - Laravel *and* Laravel + Livewire
/// count separately - so seventeen is what the table has.
pub const FRAMEWORKS: [Framework; 17] = [
    Framework {
        name: "Laravel",
        icon_file: "laravel.ico",
        runtime: "php",
        description: "PHP web framework \u{2014} composer create-project laravel/laravel",
        required_tools: &["php", "composer"],
        kind: Kind::Composer,
        composer_package: "laravel/laravel",
        download_url: "",
        strip_top: "",
        doc_root: "public",
    },
    Framework {
        name: "Laravel + Livewire",
        icon_file: "livewire.ico",
        runtime: "php",
        description: "Laravel app with Livewire full-stack pre-wired",
        required_tools: &["php", "composer"],
        kind: Kind::Composer,
        composer_package: "laravel/laravel",
        download_url: "",
        strip_top: "",
        doc_root: "public",
    },
    Framework {
        name: "Symfony",
        icon_file: "php.ico",
        runtime: "php",
        description: "PHP enterprise framework \u{2014} symfony/skeleton via composer",
        required_tools: &["php", "composer"],
        kind: Kind::Composer,
        composer_package: "symfony/skeleton",
        download_url: "",
        strip_top: "",
        doc_root: "public",
    },
    Framework {
        name: "CodeIgniter 4",
        icon_file: "php.ico",
        runtime: "php",
        description: "Lightweight PHP MVC \u{2014} codeigniter4/appstarter",
        required_tools: &["php", "composer"],
        kind: Kind::Composer,
        composer_package: "codeigniter4/appstarter",
        download_url: "",
        strip_top: "",
        doc_root: "public",
    },
    Framework {
        name: "WordPress",
        icon_file: "wordpress.ico",
        runtime: "php",
        description: "WordPress CMS \u{2014} latest release from wordpress.org",
        required_tools: &["php"],
        kind: Kind::Download,
        composer_package: "",
        download_url: "https://wordpress.org/latest.zip",
        strip_top: "wordpress/",
        doc_root: "",
    },
    Framework {
        name: "Next.js",
        icon_file: "nodejs.ico",
        runtime: "node",
        description: "React framework \u{2014} npx create-next-app",
        required_tools: &["node", "npm"],
        kind: Kind::Cmd,
        composer_package: "",
        download_url: "",
        strip_top: "",
        doc_root: ".next",
    },
    Framework {
        name: "Vite + React",
        icon_file: "nodejs.ico",
        runtime: "node",
        description: "Vite React starter \u{2014} npm create vite@latest",
        required_tools: &["node", "npm"],
        kind: Kind::Cmd,
        composer_package: "",
        download_url: "",
        strip_top: "",
        doc_root: "dist",
    },
    Framework {
        name: "Express",
        icon_file: "nodejs.ico",
        runtime: "node",
        description: "Minimal Node.js web framework \u{2014} npm + a hello-world server",
        required_tools: &["node", "npm"],
        kind: Kind::Cmd,
        composer_package: "",
        download_url: "",
        strip_top: "",
        doc_root: "",
    },
    Framework {
        name: "NestJS",
        icon_file: "nodejs.ico",
        runtime: "node",
        description: "Node.js TypeScript framework \u{2014} npm i -g @nestjs/cli && nest new",
        required_tools: &["node", "npm"],
        kind: Kind::Cmd,
        composer_package: "",
        download_url: "",
        strip_top: "",
        doc_root: "dist",
    },
    Framework {
        name: "AdonisJS",
        icon_file: "adonisjs.ico",
        runtime: "node",
        description: "Node.js full-stack framework \u{2014} npm init adonis-ts-app",
        required_tools: &["node", "npm"],
        kind: Kind::Cmd,
        composer_package: "",
        download_url: "",
        strip_top: "",
        doc_root: "build",
    },
    Framework {
        name: "Flask",
        icon_file: "python.ico",
        runtime: "python",
        description: "Python micro web framework \u{2014} pip install flask + starter app.py",
        required_tools: &["python", "pip"],
        kind: Kind::Cmd,
        composer_package: "",
        download_url: "",
        strip_top: "",
        doc_root: "",
    },
    Framework {
        name: "Django",
        icon_file: "python.ico",
        runtime: "python",
        description: "Python full-stack framework \u{2014} pip install django + django-admin startproject",
        required_tools: &["python", "pip"],
        kind: Kind::Cmd,
        composer_package: "",
        download_url: "",
        strip_top: "",
        doc_root: "",
    },
    Framework {
        name: "FastAPI",
        icon_file: "python.ico",
        runtime: "python",
        description: "Modern async Python API framework \u{2014} pip install fastapi uvicorn",
        required_tools: &["python", "pip"],
        kind: Kind::Cmd,
        composer_package: "",
        download_url: "",
        strip_top: "",
        doc_root: "",
    },
    Framework {
        name: "Go HTTP server",
        icon_file: "go.ico",
        runtime: "go",
        description: "Standard library net/http hello-world",
        required_tools: &["go"],
        kind: Kind::Cmd,
        composer_package: "",
        download_url: "",
        strip_top: "",
        doc_root: "",
    },
    Framework {
        name: "Gin (Go)",
        icon_file: "go.ico",
        runtime: "go",
        description: "Go web framework \u{2014} go mod + gin-gonic/gin",
        required_tools: &["go"],
        kind: Kind::Cmd,
        composer_package: "",
        download_url: "",
        strip_top: "",
        doc_root: "",
    },
    Framework {
        name: "Spring Boot",
        icon_file: "java.ico",
        runtime: "java",
        description: "Spring Boot starter \u{2014} fetched from start.spring.io",
        required_tools: &["java"],
        kind: Kind::Download,
        composer_package: "",
        download_url: "https://start.spring.io/starter.zip?type=maven-project&language=java&dependencies=web,devtools&packageName=com.example.demo&name=demo",
        strip_top: "demo/",
        doc_root: "",
    },
    Framework {
        name: "Static HTML",
        icon_file: "",
        runtime: "static",
        description: "Plain HTML5 boilerplate \u{2014} no build step",
        required_tools: &[],
        kind: Kind::Static,
        composer_package: "",
        download_url: "",
        strip_top: "",
        doc_root: "",
    },
];

/// The domain extensions the projects and vhost pages offer.
///
/// `.test` is first because it is the default, and because it is reserved by
/// RFC 2606 and so can never resolve on the public internet.
pub const DOMAIN_EXTENSIONS: [&str; 6] =
    [".test", ".local", ".localhost", ".lan", ".home", ".site"];

/// Every framework, in display order.
pub fn frameworks() -> &'static [Framework] {
    &FRAMEWORKS
}

/// Looks a framework up by its exact display name.
pub fn framework_by_name(name: &str) -> Option<&'static Framework> {
    FRAMEWORKS.iter().find(|framework| framework.name == name)
}

/// The command steps of a framework, in order.
///
/// Empty means "no steps registered", which [`scaffold_cmd`] reports as an
/// error rather than scaffolding nothing.
pub fn cmd_steps(name: &str) -> &'static [CmdStep] {
    match name {
        "Next.js" => &[CmdStep {
            desc: "create-next-app",
            args: &[
                "npx",
                "--yes",
                "create-next-app@latest",
                ".",
                "--js",
                "--no-tailwind",
                "--no-src-dir",
                "--no-app",
                "--no-eslint",
                "--no-import-alias",
                "--use-npm",
            ],
        }],
        "Vite + React" => &[
            CmdStep {
                desc: "create vite",
                args: &[
                    "npm",
                    "create",
                    "vite@latest",
                    ".",
                    "--",
                    "--template",
                    "react",
                ],
            },
            CmdStep {
                desc: "npm install",
                args: &["npm", "install"],
            },
        ],
        "Express" => &[
            CmdStep {
                desc: "npm init",
                args: &["npm", "init", "-y"],
            },
            CmdStep {
                desc: "install express",
                args: &["npm", "install", "express"],
            },
        ],
        "NestJS" => &[CmdStep {
            desc: "nest new",
            args: &[
                "npx",
                "--yes",
                "@nestjs/cli",
                "new",
                ".",
                "--package-manager",
                "npm",
                "--skip-git",
            ],
        }],
        "AdonisJS" => &[
            CmdStep {
                desc: "adonis create",
                args: &[
                    "npm",
                    "init",
                    "adonisjs@latest",
                    ".",
                    "--",
                    "--kit=web",
                    "--no-install",
                ],
            },
            CmdStep {
                desc: "npm install",
                args: &["npm", "install"],
            },
        ],
        "Flask" => &[CmdStep {
            desc: "pip install flask",
            args: &["pip", "install", "flask"],
        }],
        "Django" => &[
            CmdStep {
                desc: "pip install django",
                args: &["pip", "install", "django"],
            },
            CmdStep {
                desc: "django-admin startproject",
                args: &["python", "-m", "django", "startproject", "site_app", "."],
            },
        ],
        "FastAPI" => &[CmdStep {
            desc: "pip install fastapi",
            args: &["pip", "install", "fastapi", "uvicorn[standard]"],
        }],
        "Go HTTP server" => &[CmdStep {
            desc: "go mod init",
            args: &["go", "mod", "init", "lambo.local/app"],
        }],
        "Gin (Go)" => &[
            CmdStep {
                desc: "go mod init",
                args: &["go", "mod", "init", "lambo.local/gin-app"],
            },
            CmdStep {
                desc: "go get gin",
                args: &["go", "get", "github.com/gin-gonic/gin"],
            },
        ],
        _ => &[],
    }
}

/// The loopback port a framework's own dev server answers on, or `0`.
///
/// A framework with a port becomes a reverse-proxy vhost: Apache answers on
/// port 80 for the domain and forwards to this port, which is how a Next.js or
/// Flask project is reachable at `http://my-app.test` without touching its
/// start command.
pub fn proxy_port(name: &str) -> u16 {
    match name {
        "Next.js" => 3000,
        "Vite + React" => 5173,
        "Express" => 3000,
        "NestJS" => 3000,
        "AdonisJS" => 3333,
        "Flask" => 5000,
        "Django" => 8000,
        "FastAPI" => 8000,
        "Go HTTP server" => 8080,
        "Gin (Go)" => 8080,
        _ => 0,
    }
}

/// The starter file written after a framework's command steps, if it has one.
pub fn post_file(name: &str) -> Option<PostFile> {
    Some(match name {
        "Express" => PostFile {
            path: "index.js",
            content: EXPRESS_INDEX_JS,
        },
        "Flask" => PostFile {
            path: "app.py",
            content: FLASK_APP_PY,
        },
        "FastAPI" => PostFile {
            path: "main.py",
            content: FASTAPI_MAIN_PY,
        },
        "Go HTTP server" => PostFile {
            path: "main.go",
            content: GO_HTTP_MAIN_GO,
        },
        "Gin (Go)" => PostFile {
            path: "main.go",
            content: GIN_MAIN_GO,
        },
        _ => return None,
    })
}

// The starter files the original wrote, byte for byte except for the product
// name they greet the user with.

const EXPRESS_INDEX_JS: &str = r#"const express = require('express');
const app = express();
const port = 3000;

app.get('/', (req, res) => {
  res.send('Hello from Lambo PHP + Express!');
});

app.listen(port, () => {
  console.log(`Express listening on http://localhost:${port}`);
});
"#;

const FLASK_APP_PY: &str = r#"from flask import Flask

app = Flask(__name__)

@app.route('/')
def hello():
    return 'Hello from Lambo PHP + Flask!'

if __name__ == '__main__':
    app.run(host='127.0.0.1', port=5000)
"#;

const FASTAPI_MAIN_PY: &str = r#"from fastapi import FastAPI

app = FastAPI()

@app.get('/')
def root():
    return {'message': 'Hello from Lambo PHP + FastAPI!'}

# Run with: uvicorn main:app --reload
"#;

const GO_HTTP_MAIN_GO: &str = r#"package main

import (
	"fmt"
	"log"
	"net/http"
)

func main() {
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintln(w, "Hello from Lambo PHP + Go net/http!")
	})
	log.Println("listening on http://localhost:8080")
	log.Fatal(http.ListenAndServe(":8080", nil))
}
"#;

const GIN_MAIN_GO: &str = r#"package main

import "github.com/gin-gonic/gin"

func main() {
	r := gin.Default()
	r.GET("/", func(c *gin.Context) {
		c.JSON(200, gin.H{"message": "Hello from Lambo PHP + Gin!"})
	})
	r.Run(":8080") // listen on http://localhost:8080
}
"#;

/// The boilerplate [`scaffold_static`] writes. `{title}` and `{directory}` are
/// replaced with the project directory's name and path.
const STATIC_INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{title}</title>
  <style>
    body { font-family: system-ui, sans-serif; max-width: 40rem; margin: 4rem auto; padding: 0 1rem; }
    h1   { color: #3a7aef; }
    code { background: #f0f2f5; padding: .15rem .4rem; border-radius: .25rem; }
  </style>
</head>
<body>
  <h1>It works! 🎉</h1>
  <p>This site is served by Lambo PHP from <code>{directory}</code>.</p>
  <p>Edit <code>index.html</code> to get started.</p>
</body>
</html>
"#;

// ------------------------------------------------------------------- tools

/// The path a tool is expected at inside the installation, if it is one the
/// installation manages.
///
/// PHP, Composer, Node, Python, Go and the JDK all install under `{base}/bin`
/// in a known place. Anything else - and anything the user installed globally -
/// is found on `PATH` instead.
pub fn known_tool_path(base_dir: &Path, name: &str) -> Option<PathBuf> {
    let under_bin = |parts: &[&str]| {
        let mut path = base_dir.join("bin");
        for part in parts {
            path.push(part);
        }
        path
    };
    Some(match name {
        "php" => under_bin(&["php", "php.exe"]),
        "composer" => under_bin(&["php", "composer.phar"]),
        "node" => under_bin(&["node", "node.exe"]),
        "npm" => under_bin(&["node", "npm.cmd"]),
        "npx" => under_bin(&["node", "npx.cmd"]),
        "python" | "python3" => under_bin(&["python", "python.exe"]),
        "pip" | "pip3" => under_bin(&["python", "Scripts", "pip.exe"]),
        "go" => under_bin(&["go", "bin", "go.exe"]),
        "java" => under_bin(&["java", "bin", "java.exe"]),
        "javac" => under_bin(&["java", "bin", "javac.exe"]),
        "jar" => under_bin(&["java", "bin", "jar.exe"]),
        _ => return None,
    })
}

/// Whether a tool can be run at all: on `PATH`, or where the installation keeps
/// it.
///
/// The original asked `exec.LookPath` first and then `knownToolPath`, which is
/// the order here. `PATH` is searched with [`process::find_program`], which also
/// knows the platform's conventional install directories - a superset of the
/// original's lookup, so a tool that was found before is still found.
pub fn has_tool(base_dir: &Path, name: &str) -> bool {
    if process::find_program(name, Os::host()).is_some() {
        return true;
    }
    known_tool_path(base_dir, name).is_some_and(|path| path.exists())
}

/// The command to run for a tool: its path inside the installation when that
/// exists, otherwise the bare name for `PATH` to resolve.
pub fn resolve_tool(base_dir: &Path, name: &str) -> OsString {
    match known_tool_path(base_dir, name) {
        Some(path) if path.exists() => path.into_os_string(),
        _ => OsString::from(name),
    }
}

/// The `Runtime status:` line of the projects page.
///
/// A filled circle is a tool that can be run, an open one is a tool that cannot
/// be, and the report is one line so the page does not reflow as tools appear.
pub fn runtime_status_text(base_dir: &Path) -> String {
    let checks: [(&str, &str); 6] = [
        ("PHP", "php"),
        ("Composer", "composer"),
        ("Node.js", "node"),
        ("Python", "python"),
        ("Java", "java"),
        ("Go", "go"),
    ];
    let parts: Vec<String> = checks
        .iter()
        .map(|(label, tool)| {
            let mark = if has_tool(base_dir, tool) {
                '\u{2713}'
            } else {
                '\u{25cb}'
            };
            format!("{mark} {label}")
        })
        .collect();
    parts.join("   ")
}

// ---------------------------------------------------------------- composer

/// Where Composer is downloaded from when the installation does not have it.
pub const COMPOSER_URL: &str = "https://getcomposer.org/composer.phar";

/// `{base}/bin/php/composer.phar`, the file the PHP service expects.
pub fn composer_path(base_dir: &Path) -> PathBuf {
    base_dir.join("bin").join("php").join("composer.phar")
}

/// Returns the installation's `composer.phar`, downloading it when missing.
pub fn ensure_composer(base_dir: &Path, log: &LogFn) -> Result<PathBuf> {
    ensure_composer_from(base_dir, log, COMPOSER_URL)
}

/// [`ensure_composer`] against an explicit URL, which is how the download path
/// is tested without reaching getcomposer.org.
pub fn ensure_composer_from(base_dir: &Path, log: &LogFn, url: &str) -> Result<PathBuf> {
    let target = composer_path(base_dir);
    if target.exists() {
        return Ok(target);
    }

    log("composer.phar not found \u{2014} downloading from getcomposer.org");
    if let Some(parent) = target.parent() {
        // 0o755 in the original; on Windows the mode is ignored, and on Unix
        // the umask decides the rest.
        crate::fsx::ensure_dir(parent)?;
    }

    // Deliberately not the download cache: the original wrote Composer straight
    // into the installation, so a cache entry would be a second copy of a file
    // that is itself already a download.
    let progress = |_done: i64, _total: i64| {};
    http_download(url, &target, log, &progress)
        .map_err(|error| Error::InvalidInput(format!("download composer: {error}")))?;
    Ok(target)
}

// -------------------------------------------------------------- scaffolding

/// What a scaffold produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaffoldResult {
    /// Absolute document root: the project directory, or `doc_root` below it.
    pub doc_root: PathBuf,
    /// Something that went wrong without being fatal.
    ///
    /// The original declared this field and never set it; the one candidate it
    /// clearly had in mind is the Livewire step of `Laravel + Livewire`, which
    /// is logged as non-fatal and which this port also reports here, so the GUI
    /// can show it beside the project it just created.
    pub warning: Option<String>,
}

/// Runs a command in a directory and streams its output into the log.
///
/// `argv[0]` is resolved through [`resolve_tool`] first, so a bare `php` picks
/// up the installation's own `bin/php/php.exe` when it exists. The console
/// window is suppressed ([`process::spawn_piped`]); both pipes are read on their
/// own threads, because a command that fills a pipe buffer while its other
/// stream is being read would otherwise deadlock.
pub fn run_in_dir(base_dir: &Path, dir: &Path, log: &LogFn, argv: &[&str]) -> Result<()> {
    if argv.is_empty() {
        return Err(Error::InvalidInput("empty command".to_owned()));
    }

    let program = resolve_tool(base_dir, argv[0]);
    let spec = ProcessSpec::new(PathBuf::from(&program), argv[0])
        .args(argv[1..].iter().copied())
        .cwd(dir);
    let mut child = process::spawn_piped(&spec, Os::host())?;

    let stdout = child.stdout.take().map(BufReader::new);
    let stderr = child.stderr.take().map(BufReader::new);

    let status = std::thread::scope(|scope| -> Result<std::process::ExitStatus> {
        let mut readers = Vec::new();
        if let Some(pipe) = stdout {
            let log = Arc::clone(log);
            readers.push(scope.spawn(move || stream_to_log(pipe, &log)));
        }
        if let Some(pipe) = stderr {
            let log = Arc::clone(log);
            readers.push(scope.spawn(move || stream_to_log(pipe, &log)));
        }

        let status = child.wait().map_err(|source| {
            Error::service_failed(
                argv[0],
                format!("could not wait: {source}"),
                std::iter::empty::<String>(),
            )
        })?;
        for reader in readers {
            let _ = reader.join();
        }
        Ok(status)
    })?;

    if status.success() {
        Ok(())
    } else {
        Err(Error::InvalidInput(exit_status_message(status)))
    }
}

/// The message `exec.Cmd.Wait` reports for a failed command: `exit status 1`.
fn exit_status_message(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status {code}"),
        None => "process was terminated".to_owned(),
    }
}

/// Copies a child's output into the log, line by line.
///
/// This is the original's `streamToLog`: a trailing carriage return is dropped
/// (Windows tools end lines with `\r\n`), empty lines are skipped, and a line
/// longer than a megabyte is skipped too - the original's `bufio.Scanner`
/// refused it rather than growing without bound.
pub fn stream_to_log(mut reader: impl BufRead, log: &LogFn) {
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) => break,
            Ok(_) => {
                if buffer.len() > LOG_LINE_LIMIT {
                    continue;
                }
                let text = String::from_utf8_lossy(&buffer);
                let text = text.strip_suffix('\n').unwrap_or(&text);
                let text = text.trim_end_matches('\r');
                if !text.is_empty() {
                    log(text);
                }
            }
            Err(_) => break,
        }
    }
}

/// The longest line [`stream_to_log`] will pass on, matching the original's
/// `bufio.Scanner` buffer.
const LOG_LINE_LIMIT: usize = 1 << 20;

/// Scaffolds a framework into a directory that must be empty or absent.
pub fn scaffold_framework(
    base_dir: &Path,
    framework: &Framework,
    project_dir: &Path,
    log: &LogFn,
) -> Result<ScaffoldResult> {
    if let Ok(metadata) = fs::metadata(project_dir) {
        if metadata.is_dir() {
            // A directory with anything in it is somebody's work; a directory
            // with nothing in it is the leftovers of a failed run.
            let entries = fs::read_dir(project_dir)
                .map(|entries| entries.count())
                .unwrap_or(0);
            if entries > 0 {
                return Err(Error::InvalidInput(format!(
                    "{} already exists and is not empty",
                    project_dir.display()
                )));
            }
        }
    }
    crate::fsx::ensure_dir(project_dir)
        .map_err(|error| Error::InvalidInput(format!("create project dir: {error}")))?;

    for tool in framework.required_tools {
        // Composer is not a tool to check for: it is downloaded by
        // `scaffold_composer`, so requiring it here would refuse to scaffold on
        // the installations that most need it.
        if *tool == "composer" {
            continue;
        }
        if !has_tool(base_dir, tool) {
            return Err(Error::InvalidInput(format!(
                "missing {tool}: install it first (Settings \u{2192} runtimes)"
            )));
        }
    }

    match framework.kind {
        Kind::Composer => scaffold_composer(base_dir, framework, project_dir, log),
        Kind::Download => scaffold_download(base_dir, framework, project_dir, log),
        Kind::Static => scaffold_static(project_dir, log),
        Kind::Cmd => scaffold_cmd(base_dir, framework, project_dir, log),
    }
}

/// Runs a framework's command steps, then writes its starter file.
pub fn scaffold_cmd(
    base_dir: &Path,
    framework: &Framework,
    project_dir: &Path,
    log: &LogFn,
) -> Result<ScaffoldResult> {
    scaffold_cmd_steps(
        base_dir,
        framework,
        project_dir,
        log,
        cmd_steps(framework.name),
    )
}

/// [`scaffold_cmd`] with the steps supplied, which is how the runner is tested
/// without npm, pip or go.
fn scaffold_cmd_steps(
    base_dir: &Path,
    framework: &Framework,
    project_dir: &Path,
    log: &LogFn,
    steps: &[CmdStep],
) -> Result<ScaffoldResult> {
    if steps.is_empty() {
        return Err(Error::InvalidInput(format!(
            "no command steps registered for {}",
            framework.name
        )));
    }

    for (index, step) in steps.iter().enumerate() {
        log(&format!(
            "[{}/{}] {} \u{2014} {}",
            index + 1,
            steps.len(),
            step.desc,
            step.args.join(" ")
        ));
        run_in_dir(base_dir, project_dir, log, step.args)
            .map_err(|error| Error::InvalidInput(format!("{}: {error}", step.desc)))?;
    }

    if let Some(post) = post_file(framework.name) {
        let full = project_dir.join(post.path);
        fs::write(&full, post.content)
            .map_err(|error| Error::InvalidInput(format!("write starter file: {error}")))?;
        log(&format!("  wrote {}", post.path));
    }

    Ok(ScaffoldResult {
        doc_root: document_root(project_dir, framework.doc_root),
        warning: None,
    })
}

/// Scaffolds a Composer project, adding Livewire for the Livewire flavour.
pub fn scaffold_composer(
    base_dir: &Path,
    framework: &Framework,
    project_dir: &Path,
    log: &LogFn,
) -> Result<ScaffoldResult> {
    let composer = ensure_composer(base_dir, log)?;
    let php = known_tool_path(base_dir, "php");
    let php_ref = php.as_deref().filter(|path| path.exists());
    let Some(php_exe) = php_ref else {
        return Err(Error::InvalidInput(format!(
            "php.exe not found at {} \u{2014} install the PHP service first",
            php.as_deref().unwrap_or(Path::new("")).display()
        )));
    };

    log(&format!(
        "scaffolding {} into {} (this may take a few minutes)...",
        framework.name,
        project_dir.display()
    ));
    let php_exe = php_exe.to_string_lossy().into_owned();
    let composer = composer.to_string_lossy().into_owned();
    run_in_dir(
        base_dir,
        project_dir,
        log,
        &[
            php_exe.as_str(),
            composer.as_str(),
            "create-project",
            "--no-interaction",
            framework.composer_package,
            ".",
        ],
    )
    .map_err(|error| Error::InvalidInput(format!("composer create-project: {error}")))?;

    let mut warning = None;
    if framework.name == "Laravel + Livewire" {
        log("adding Livewire via composer require...");
        if let Err(error) = run_in_dir(
            base_dir,
            project_dir,
            log,
            &[
                php_exe.as_str(),
                composer.as_str(),
                "require",
                "livewire/livewire",
                "--no-interaction",
            ],
        ) {
            // Deliberately non-fatal: the Laravel app is scaffolded and usable,
            // and refetching it because one package failed would be worse.
            let message = format!("livewire install failed (non-fatal): {error}");
            log(&message);
            warning = Some(message);
        }
    }

    Ok(ScaffoldResult {
        doc_root: document_root(project_dir, framework.doc_root),
        warning,
    })
}

/// Scaffolds from a release archive, through the download cache.
pub fn scaffold_download(
    base_dir: &Path,
    framework: &Framework,
    project_dir: &Path,
    log: &LogFn,
) -> Result<ScaffoldResult> {
    // Fetch logs the cache hit, and the download logs the GET on a miss.
    let mut cache = download_cache::DownloadCache::new(
        base_dir,
        Arc::clone(log),
        Box::new(crate::download::PanelDownloader),
    );
    let name = format!("{}.zip", vhost::safe_file_name(framework.name));
    let downloaded = cache
        .fetch(&name, framework.download_url, &nop_progress())
        .map_err(|error| Error::InvalidInput(format!("download: {error}")))?;

    log(&format!("extracting into {}", project_dir.display()));
    let strip_top = if framework.strip_top.is_empty() {
        None
    } else {
        Some(framework.strip_top)
    };
    archive::extract_zip_with(&downloaded, project_dir, strip_top, None)
        .map_err(|error| Error::InvalidInput(format!("extract: {error}")))?;

    Ok(ScaffoldResult {
        doc_root: document_root(project_dir, framework.doc_root),
        warning: None,
    })
}

/// Writes the one-page HTML boilerplate for a static project.
pub fn scaffold_static(project_dir: &Path, log: &LogFn) -> Result<ScaffoldResult> {
    let title = project_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let html = STATIC_INDEX_HTML
        .replace("{title}", &title)
        .replace("{directory}", &project_dir.to_string_lossy());

    let index = project_dir.join("index.html");
    fs::write(&index, html).map_err(|error| Error::io(&index, error))?;
    log("created index.html boilerplate");
    Ok(ScaffoldResult {
        doc_root: project_dir.to_path_buf(),
        warning: None,
    })
}

/// The document root of a scaffolded project: `{project}/doc_root`, or the
/// project directory when the framework has no sub-directory of its own.
pub fn document_root(project_dir: &Path, doc_root: &str) -> PathBuf {
    if doc_root.is_empty() {
        project_dir.to_path_buf()
    } else {
        project_dir.join(doc_root)
    }
}

// ----------------------------------------------------------- registration

/// Where a scaffolded project is recorded.
///
/// The installation document is the panel configuration, whose reader and
/// writer are JSON and therefore not available to every caller. The rules about
/// *what* to record live in [`create_project`]; this trait is only about
/// writing it down.
pub trait ProjectSink {
    /// The projects the document holds, in the order they were created.
    fn projects(&self) -> &[PanelProject];

    /// Appends the project and its virtual host and persists the document.
    fn record(&mut self, project: &PanelProject, host: &Vhost) -> Result<()>;

    /// Removes a project and every virtual host on its domain, and persists the
    /// document. Returns whether the project was there.
    fn remove(&mut self, name: &str) -> Result<bool>;

    /// Rewrites the hosts file and the server configurations from the document.
    fn apply(&mut self) -> Result<()>;
}

/// The directory a project named `name` is scaffolded into.
pub fn project_directory(base_dir: &Path, name: &str) -> PathBuf {
    base_dir.join("www").join(name)
}

/// The project name as the original's `slugify` normalised it.
///
/// Note the difference from [`crate::naming::slugify`]: an input with nothing usable in
/// it becomes the empty string here, and `app` there. That is the original's
/// behaviour, and the empty result is what makes `create_project` reject the
/// name instead of scaffolding a project called `app`.
pub fn project_slug(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut previous_was_dash = false;
    for ch in name.trim().to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            slug.push(ch);
            previous_was_dash = false;
        } else if !previous_was_dash && !slug.is_empty() {
            slug.push('-');
            previous_was_dash = true;
        }
    }
    slug.trim_end_matches('-').to_owned()
}

/// Splits a domain into its name and its extension, as the vhost form does.
///
/// A domain with no dot is returned whole with `.test`, because that is the
/// extension the form would have selected for it.
pub fn split_domain(domain: &str) -> (String, String) {
    let domain = domain.trim();
    match domain.rfind('.') {
        Some(index) => (domain[..index].to_owned(), domain[index..].to_owned()),
        None => (domain.to_owned(), ".test".to_owned()),
    }
}

/// How the vhost document refers to a document root inside the installation:
/// `{base}/www/name/public`.
///
/// A path outside the installation - which a hand-edited document can contain -
/// keeps its absolute form, with the separators normalised, because `{base}`
/// does not describe it.
pub fn document_root_reference(base_dir: &Path, doc_root: &Path) -> String {
    let document = doc_root.to_string_lossy().replace('\\', "/");
    let base = base_dir.to_string_lossy().replace('\\', "/");
    let base = base.trim_end_matches('/');
    match document.strip_prefix(&format!("{base}/")) {
        Some(relative) => format!("{{base}}/{relative}"),
        None => document,
    }
}

/// The project and virtual host records [`create_project`] appends.
///
/// A framework with a dev-server port also gets a reverse-proxy vhost, and the
/// project records the same port so the projects page can show where it will
/// answer.
pub fn project_records(
    base_dir: &Path,
    framework: &Framework,
    name: &str,
    domain: &str,
    doc_root: &Path,
) -> (PanelProject, Vhost) {
    let port = proxy_port(framework.name);
    let project = PanelProject {
        name: name.to_owned(),
        framework: framework.name.to_owned(),
        domain: domain.to_owned(),
        docroot: doc_root.to_string_lossy().into_owned(),
        port,
    };
    let host = Vhost {
        domain: domain.to_owned(),
        docroot: document_root_reference(base_dir, doc_root),
        port: 80,
        server_type: "apache".to_owned(),
        enabled: true,
        proxy_port: port,
    };
    (project, host)
}

/// A project that was created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedProject {
    /// Directory name under `www/`.
    pub name: String,
    /// Framework display name.
    pub framework: &'static str,
    /// Domain the project answers on.
    pub domain: String,
    /// Absolute document root.
    pub doc_root: PathBuf,
    /// Reverse-proxy port, zero when the framework has no dev server.
    pub proxy_port: u16,
    /// A non-fatal problem, as [`ScaffoldResult::warning`].
    pub warning: Option<String>,
}

/// Scaffolds a framework and registers the project and its virtual host.
///
/// The order is the original's, and it matters: the project directory is
/// scaffolded first (so a failed scaffold registers nothing), the document is
/// saved before the hosts file is touched, and a failure to apply the vhosts is
/// reported *and* returned - the project exists, but the user has to know that
/// the domain does not resolve yet, and why.
pub fn create_project(
    sink: &mut dyn ProjectSink,
    base_dir: &Path,
    framework: &Framework,
    name: &str,
    domain: &str,
    log: &LogFn,
) -> Result<CreatedProject> {
    if name.is_empty() {
        return Err(Error::InvalidInput("project name required".to_owned()));
    }
    let domain = if domain.is_empty() {
        format!("{name}.test")
    } else {
        domain.to_owned()
    };
    let project_dir = project_directory(base_dir, name);

    let result = scaffold_framework(base_dir, framework, &project_dir, log)?;
    let (project, host) = project_records(base_dir, framework, name, &domain, &result.doc_root);

    sink.record(&project, &host)
        .map_err(|error| Error::InvalidInput(format!("save config: {error}")))?;

    if let Err(error) = sink.apply() {
        log(&format!("apply vhosts: {error}"));
        log("  \u{2192} run Lambo PHP as administrator for hosts-file writes to work");
        return Err(Error::InvalidInput(format!("apply vhosts: {error}")));
    }

    log(&format!(
        "project '{name}' created at {} \u{2192} http://{domain}",
        result.doc_root.display()
    ));
    log("NOTE: Apache needs a restart to pick up the new vhost \u{2014} click 'Restart Stack'");

    Ok(CreatedProject {
        name: name.to_owned(),
        framework: framework.name,
        domain,
        doc_root: result.doc_root,
        proxy_port: proxy_port(framework.name),
        warning: result.warning,
    })
}

/// A project an existing folder was loaded as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptedProject {
    /// The project's name, slugged from the folder's.
    pub name: String,
    /// What the detection concluded the project is.
    pub framework: String,
    /// The domain the project answers on.
    pub domain: String,
    /// The absolute document root that is served.
    pub doc_root: PathBuf,
    /// The detection's one-line summary, evidence included.
    pub summary: String,
    /// Whether the project needs a database to run.
    pub needs_database: bool,
    /// The PHP constraint read from `composer.json`, when there is one.
    pub php_requirement: Option<String>,
}

/// Loads an existing folder as a project and registers it with a virtual host.
///
/// This is the projects page's `Open Project`: nothing is scaffolded, because
/// the project already exists. The folder is probed the way `lambo init`
/// probes - the framework and the document root are read from its files - and
/// the records [`create_project`] would append are appended for it, so the
/// stack serves the folder on a domain of its own.
pub fn adopt_project(
    sink: &mut dyn ProjectSink,
    base_dir: &Path,
    folder: &Path,
    log: &LogFn,
) -> Result<AdoptedProject> {
    if !folder.is_dir() {
        return Err(Error::InvalidInput(format!(
            "projects: {} is not a folder",
            folder.display()
        )));
    }
    let file_name = folder
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = project_slug(&file_name);
    if name.is_empty() {
        return Err(Error::InvalidInput(
            "projects: the folder needs a name".to_owned(),
        ));
    }
    if sink.projects().iter().any(|project| project.name == name) {
        return Err(Error::InvalidInput(format!(
            "projects: a project named {name} is already registered"
        )));
    }

    let detection = crate::detect::detect(folder);
    let domain = format!("{name}.test");
    let doc_root = folder.join(detection.document_root);
    let project = PanelProject {
        name: name.clone(),
        framework: detection.framework.display_name().to_owned(),
        domain: domain.clone(),
        docroot: doc_root.to_string_lossy().into_owned(),
        port: 0,
    };
    let host = Vhost {
        domain: domain.clone(),
        docroot: document_root_reference(base_dir, &doc_root),
        port: 80,
        server_type: "apache".to_owned(),
        enabled: true,
        proxy_port: 0,
    };

    sink.record(&project, &host)
        .map_err(|error| Error::InvalidInput(format!("save config: {error}")))?;

    if let Err(error) = sink.apply() {
        log(&format!("apply vhosts: {error}"));
        log("  \u{2192} run Lambo PHP as administrator for hosts-file writes to work");
        return Err(Error::InvalidInput(format!("apply vhosts: {error}")));
    }

    log(&format!(
        "project '{name}' loaded from {} - {}",
        folder.display(),
        detection.summary(),
    ));
    log(&format!(
        "  serving {} \u{2192} http://{domain}",
        doc_root.display()
    ));
    if detection.needs_database {
        log("  needs a database - start MariaDB from the services page");
    }
    if let Some(php) = &detection.php_requirement {
        log(&format!("  asks for PHP {php}"));
    }
    log("NOTE: Apache needs a restart to pick up the new vhost \u{2014} click 'Restart Stack'");

    Ok(AdoptedProject {
        name,
        framework: detection.framework.display_name().to_owned(),
        domain,
        doc_root,
        summary: detection.summary(),
        needs_database: detection.needs_database,
        php_requirement: detection.php_requirement.clone(),
    })
}

/// Deletes a project: its directory, its record, and its domains.
///
/// Nothing here fails visibly, because the original's `deleteProject` did not:
/// a directory that cannot be removed is logged and the registration is dropped
/// anyway (a locked file on Windows must not leave a row pointing at a project
/// the user just deleted), a document that cannot be saved is logged, and a
/// hosts file that cannot be rewritten is logged with the reason. Returns
/// whether there was a project to delete.
pub fn delete_project(
    sink: &mut dyn ProjectSink,
    base_dir: &Path,
    name: &str,
    log: &LogFn,
) -> bool {
    let Some(project) = sink
        .projects()
        .iter()
        .find(|project| project.name == name)
        .cloned()
    else {
        return false;
    };

    log(&format!(
        "projects: deleting '{}' ({})...",
        project.name, project.domain
    ));

    let project_dir = project_directory(base_dir, &project.name);
    match fs::remove_dir_all(&project_dir) {
        Ok(()) => {}
        // `RemoveAll` answered nil for a path that was not there, so a project
        // whose files were deleted by hand is not a failure.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => log(&format!("projects delete: {error}")),
    }

    if let Err(error) = sink.remove(&project.name) {
        log(&format!("projects delete: {error}"));
    }

    if let Err(error) = sink.apply() {
        log(&format!("projects delete: apply vhosts: {error}"));
    }

    true
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::naming;
    use crate::testutil::{TempDir, fixture, write_zip};

    /// A log sink that keeps every line, so the tests can assert on the text
    /// the user would see.
    fn recorder() -> (LogFn, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: LogFn =
            Arc::new(move |line: &str| sink.lock().expect("log poisoned").push(line.to_owned()));
        (log, lines)
    }

    fn logged(lines: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        lines.lock().expect("log poisoned").clone()
    }

    /// A framework whose scaffold needs nothing installed, for the tests that
    /// exercise the dispatch and the guards rather than a real toolchain.
    const PORTABLE: Framework = Framework {
        name: "Express",
        icon_file: "",
        runtime: "node",
        description: "",
        required_tools: &[],
        kind: Kind::Cmd,
        composer_package: "",
        download_url: "",
        strip_top: "",
        doc_root: "",
    };

    /// A recording sink: it keeps what `create_project` would have written and
    /// can be told to fail the hosts-file step.
    #[derive(Default)]
    struct Sink {
        projects: Vec<PanelProject>,
        hosts: Vec<Vhost>,
        saved: usize,
        removed: usize,
        applied: usize,
        fail_apply: bool,
    }

    impl ProjectSink for Sink {
        fn projects(&self) -> &[PanelProject] {
            &self.projects
        }

        fn record(&mut self, project: &PanelProject, host: &Vhost) -> Result<()> {
            self.projects.push(project.clone());
            self.hosts.push(host.clone());
            self.saved += 1;
            Ok(())
        }

        fn remove(&mut self, name: &str) -> Result<bool> {
            let Some(index) = self
                .projects
                .iter()
                .position(|project| project.name == name)
            else {
                return Ok(false);
            };
            let domain = self.projects[index].domain.clone();
            self.projects.remove(index);
            // Every vhost on the domain, not just the one the project
            // registered: a hand-added vhost for the same domain is the same
            // site, and leaving it behind would serve a directory that is gone.
            self.hosts.retain(|host| host.domain != domain);
            self.removed += 1;
            Ok(true)
        }

        fn apply(&mut self) -> Result<()> {
            self.applied += 1;
            if self.fail_apply {
                return Err(Error::InvalidInput(
                    "failed to access the hosts file".to_owned(),
                ));
            }
            Ok(())
        }
    }

    /// A command step that succeeds on the platform the test is running on.
    ///
    /// The fixture spells out the shell only because a portable "do nothing,
    /// successfully" command has to; nothing in the module under test runs
    /// through a shell.
    fn portable_step(desc: &'static str) -> CmdStep {
        #[cfg(windows)]
        return CmdStep {
            desc,
            args: &["cmd", "/C", "exit 0"],
        };
        #[cfg(not(windows))]
        return CmdStep {
            desc,
            args: &["sh", "-c", "exit 0"],
        };
    }

    /// The same, for a command that fails with a known status.
    fn failing_step(desc: &'static str, code: &'static str) -> CmdStep {
        #[cfg(windows)]
        let args: &'static [&'static str] = match code {
            "3" => &["cmd", "/C", "exit 3"],
            _ => &["cmd", "/C", "exit 1"],
        };
        #[cfg(not(windows))]
        let args: &'static [&'static str] = match code {
            "3" => &["sh", "-c", "exit 3"],
            _ => &["sh", "-c", "exit 1"],
        };
        CmdStep { desc, args }
    }

    // ------------------------------------------------------------ the table

    #[test]
    fn the_catalogue_is_the_original_seventeen() {
        // One row of the shipped catalogue.
        type Row = (
            &'static str,
            &'static str,
            &'static str,
            Kind,
            &'static [&'static str],
            &'static str,
        );

        // name, icon, runtime, kind, tools, doc root, package/url/strip
        let expected: [Row; 17] = [
            (
                "Laravel",
                "laravel.ico",
                "php",
                Kind::Composer,
                &["php", "composer"],
                "public",
            ),
            (
                "Laravel + Livewire",
                "livewire.ico",
                "php",
                Kind::Composer,
                &["php", "composer"],
                "public",
            ),
            (
                "Symfony",
                "php.ico",
                "php",
                Kind::Composer,
                &["php", "composer"],
                "public",
            ),
            (
                "CodeIgniter 4",
                "php.ico",
                "php",
                Kind::Composer,
                &["php", "composer"],
                "public",
            ),
            (
                "WordPress",
                "wordpress.ico",
                "php",
                Kind::Download,
                &["php"],
                "",
            ),
            (
                "Next.js",
                "nodejs.ico",
                "node",
                Kind::Cmd,
                &["node", "npm"],
                ".next",
            ),
            (
                "Vite + React",
                "nodejs.ico",
                "node",
                Kind::Cmd,
                &["node", "npm"],
                "dist",
            ),
            (
                "Express",
                "nodejs.ico",
                "node",
                Kind::Cmd,
                &["node", "npm"],
                "",
            ),
            (
                "NestJS",
                "nodejs.ico",
                "node",
                Kind::Cmd,
                &["node", "npm"],
                "dist",
            ),
            (
                "AdonisJS",
                "adonisjs.ico",
                "node",
                Kind::Cmd,
                &["node", "npm"],
                "build",
            ),
            (
                "Flask",
                "python.ico",
                "python",
                Kind::Cmd,
                &["python", "pip"],
                "",
            ),
            (
                "Django",
                "python.ico",
                "python",
                Kind::Cmd,
                &["python", "pip"],
                "",
            ),
            (
                "FastAPI",
                "python.ico",
                "python",
                Kind::Cmd,
                &["python", "pip"],
                "",
            ),
            ("Go HTTP server", "go.ico", "go", Kind::Cmd, &["go"], ""),
            ("Gin (Go)", "go.ico", "go", Kind::Cmd, &["go"], ""),
            (
                "Spring Boot",
                "java.ico",
                "java",
                Kind::Download,
                &["java"],
                "",
            ),
            ("Static HTML", "", "static", Kind::Static, &[], ""),
        ];

        assert_eq!(FRAMEWORKS.len(), expected.len());
        for (framework, (name, icon, runtime, kind, tools, doc_root)) in
            FRAMEWORKS.iter().zip(expected)
        {
            assert_eq!(framework.name, name);
            assert_eq!(framework.icon_file, icon);
            assert_eq!(framework.runtime, runtime);
            assert_eq!(framework.kind, kind);
            assert_eq!(framework.required_tools, tools, "{name} tools");
            assert_eq!(framework.doc_root, doc_root, "{name} doc root");
            assert!(!framework.description.is_empty(), "{name} description");
        }
    }

    #[test]
    fn the_composer_and_archive_frameworks_are_the_originals() {
        let laravel = framework_by_name("Laravel").expect("Laravel");
        assert_eq!(laravel.composer_package, "laravel/laravel");
        assert_eq!(
            framework_by_name("Symfony")
                .expect("Symfony")
                .composer_package,
            "symfony/skeleton"
        );
        assert_eq!(
            framework_by_name("CodeIgniter 4")
                .expect("CI4")
                .composer_package,
            "codeigniter4/appstarter"
        );
        assert_eq!(
            framework_by_name("Laravel + Livewire")
                .expect("Livewire")
                .composer_package,
            "laravel/laravel"
        );

        let wordpress = framework_by_name("WordPress").expect("WordPress");
        assert_eq!(wordpress.download_url, "https://wordpress.org/latest.zip");
        assert_eq!(wordpress.strip_top, "wordpress/");

        let spring = framework_by_name("Spring Boot").expect("Spring Boot");
        assert_eq!(spring.strip_top, "demo/");
        assert!(
            spring
                .download_url
                .starts_with("https://start.spring.io/starter.zip?"),
            "{}",
            spring.download_url
        );
        assert!(spring.download_url.contains("dependencies=web,devtools"));

        // A framework that is neither has neither.
        for name in ["Next.js", "Static HTML"] {
            let framework = framework_by_name(name).expect(name);
            assert!(framework.composer_package.is_empty(), "{name}");
            assert!(framework.download_url.is_empty(), "{name}");
            assert!(framework.strip_top.is_empty(), "{name}");
        }
    }

    #[test]
    fn framework_lookup_is_exact() {
        for framework in frameworks() {
            assert_eq!(
                framework_by_name(framework.name).map(|found| found.name),
                Some(framework.name)
            );
        }
        assert!(framework_by_name("laravel").is_none(), "case matters");
        assert!(framework_by_name("").is_none());
        assert!(framework_by_name("Laravel 11").is_none());
    }

    #[test]
    fn the_command_steps_are_the_originals() {
        let steps = |name: &str| -> Vec<String> {
            cmd_steps(name)
                .iter()
                .map(|step| step.args.join(" "))
                .collect()
        };

        assert_eq!(
            steps("Next.js"),
            vec![
                "npx --yes create-next-app@latest . --js --no-tailwind --no-src-dir --no-app \
                 --no-eslint --no-import-alias --use-npm"
            ]
        );
        assert_eq!(
            steps("Vite + React"),
            vec![
                "npm create vite@latest . -- --template react",
                "npm install"
            ]
        );
        assert_eq!(steps("Express"), vec!["npm init -y", "npm install express"]);
        assert_eq!(
            steps("NestJS"),
            vec!["npx --yes @nestjs/cli new . --package-manager npm --skip-git"]
        );
        assert_eq!(
            steps("AdonisJS"),
            vec![
                "npm init adonisjs@latest . -- --kit=web --no-install",
                "npm install"
            ]
        );
        assert_eq!(steps("Flask"), vec!["pip install flask"]);
        assert_eq!(
            steps("Django"),
            vec![
                "pip install django",
                "python -m django startproject site_app ."
            ]
        );
        assert_eq!(
            steps("FastAPI"),
            vec!["pip install fastapi uvicorn[standard]"]
        );
        assert_eq!(steps("Go HTTP server"), vec!["go mod init lambo.local/app"]);
        assert_eq!(
            steps("Gin (Go)"),
            vec![
                "go mod init lambo.local/gin-app",
                "go get github.com/gin-gonic/gin"
            ]
        );

        // The step descriptions are what the log shows for each step.
        let descriptions: Vec<&str> = cmd_steps("Vite + React")
            .iter()
            .map(|step| step.desc)
            .collect();
        assert_eq!(descriptions, vec!["create vite", "npm install"]);
    }

    #[test]
    fn the_strategies_without_commands_have_no_steps() {
        for framework in frameworks() {
            match framework.kind {
                Kind::Cmd => assert!(
                    !cmd_steps(framework.name).is_empty(),
                    "{} has no steps",
                    framework.name
                ),
                _ => assert!(
                    cmd_steps(framework.name).is_empty(),
                    "{} should have no steps",
                    framework.name
                ),
            }
        }
    }

    #[test]
    fn the_proxy_ports_are_the_originals() {
        let expected = [
            ("Next.js", 3000),
            ("Vite + React", 5173),
            ("Express", 3000),
            ("NestJS", 3000),
            ("AdonisJS", 3333),
            ("Flask", 5000),
            ("Django", 8000),
            ("FastAPI", 8000),
            ("Go HTTP server", 8080),
            ("Gin (Go)", 8080),
        ];
        for (name, port) in expected {
            assert_eq!(proxy_port(name), port, "{name}");
        }
        // Everything else is a document-root vhost, not a proxy.
        for framework in frameworks() {
            if !expected.iter().any(|(name, _)| *name == framework.name) {
                assert_eq!(proxy_port(framework.name), 0, "{}", framework.name);
            }
        }
    }

    #[test]
    fn the_post_files_are_the_originals() {
        let express = post_file("Express").expect("Express post-file");
        assert_eq!(express.path, "index.js");
        assert!(
            express
                .content
                .starts_with("const express = require('express');")
        );
        assert!(express.content.contains("const port = 3000;"));
        assert!(express.content.contains("Hello from Lambo PHP + Express!"));
        assert!(
            express
                .content
                .contains("Express listening on http://localhost:${port}")
        );
        assert!(express.content.ends_with("});\n"));

        let flask = post_file("Flask").expect("Flask post-file");
        assert_eq!(flask.path, "app.py");
        assert!(flask.content.contains("from flask import Flask"));
        assert!(
            flask
                .content
                .contains("app.run(host='127.0.0.1', port=5000)")
        );

        let fastapi = post_file("FastAPI").expect("FastAPI post-file");
        assert_eq!(fastapi.path, "main.py");
        assert!(fastapi.content.contains("app = FastAPI()"));
        assert!(fastapi.content.contains("uvicorn main:app --reload"));

        let go = post_file("Go HTTP server").expect("Go post-file");
        assert_eq!(go.path, "main.go");
        assert!(go.content.contains("package main"));
        assert!(go.content.contains("\"net/http\""));
        assert!(go.content.contains("http.ListenAndServe(\":8080\", nil)"));

        let gin = post_file("Gin (Go)").expect("Gin post-file");
        assert_eq!(gin.path, "main.go");
        assert!(gin.content.contains("import \"github.com/gin-gonic/gin\""));
        assert!(gin.content.contains("r.Run(\":8080\")"));

        // Only the command frameworks have one.
        for name in [
            "Laravel",
            "WordPress",
            "Next.js",
            "Vite + React",
            "Django",
            "Spring Boot",
            "Static HTML",
        ] {
            assert!(post_file(name).is_none(), "{name}");
        }
    }

    #[test]
    fn none_of_the_generated_files_or_lines_carry_the_old_brand() {
        let mut texts: Vec<String> = Vec::new();
        for framework in frameworks() {
            texts.push(framework.description.to_owned());
        }
        for name in ["Express", "Flask", "FastAPI", "Go HTTP server", "Gin (Go)"] {
            texts.push(post_file(name).expect("post-file").content.to_owned());
        }
        texts.push(STATIC_INDEX_HTML.to_owned());
        texts.push(cmd_steps("Go HTTP server")[0].args.join(" "));
        texts.push(cmd_steps("Gin (Go)")[0].args.join(" "));

        for text in texts {
            let lowered = text.to_lowercase();
            assert!(!lowered.contains("goampp"), "old brand in `{text}`");
        }
    }

    #[test]
    fn unknown_keys_are_rejected_the_way_the_original_rejected_them() {
        assert_eq!(
            Kind::from_key("composer").expect("composer"),
            Kind::Composer
        );
        assert_eq!(
            Kind::from_key("download").expect("download"),
            Kind::Download
        );
        assert_eq!(Kind::from_key("cmd").expect("cmd"), Kind::Cmd);
        assert_eq!(Kind::from_key("static").expect("static"), Kind::Static);
        assert_eq!(Kind::Cmd.key(), "cmd");

        let error = Kind::from_key("docker").expect_err("unknown kind");
        assert_eq!(
            error.to_string(),
            "unknown framework kind: docker",
            "the original's message"
        );
        assert!(Kind::from_key("").is_err());
    }

    // -------------------------------------------------------------- tools

    #[test]
    fn the_known_tool_paths_are_the_originals() {
        let base = Path::new("/lambo");
        // Components, not strings: the assertion is about the parts and their
        // order, and a PathBuf joined one component at a time spells them with
        // the platform's own separator.
        let expected = [
            ("php", "bin/php/php.exe"),
            ("composer", "bin/php/composer.phar"),
            ("node", "bin/node/node.exe"),
            ("npm", "bin/node/npm.cmd"),
            ("npx", "bin/node/npx.cmd"),
            ("python", "bin/python/python.exe"),
            ("python3", "bin/python/python.exe"),
            ("pip", "bin/python/Scripts/pip.exe"),
            ("pip3", "bin/python/Scripts/pip.exe"),
            ("go", "bin/go/bin/go.exe"),
            ("java", "bin/java/bin/java.exe"),
            ("javac", "bin/java/bin/javac.exe"),
            ("jar", "bin/java/bin/jar.exe"),
        ];
        for (name, relative) in expected {
            let mut joined = base.to_path_buf();
            for part in relative.split('/') {
                joined.push(part);
            }
            assert_eq!(known_tool_path(base, name), Some(joined), "{name}");
        }
        assert_eq!(known_tool_path(base, "ruby"), None);
        assert_eq!(known_tool_path(base, ""), None);
    }

    #[test]
    fn a_tool_in_the_installation_counts_even_though_it_is_not_on_path() {
        let temp = TempDir::new();
        let base = temp.path();
        // A name no `PATH` lookup can resolve, at a known path.
        fixture(base, "bin/go/bin/go.exe", "");

        assert!(has_tool(base, "go"), "the installation's own copy is found");
        assert!(
            process::find_program("go", Os::host()).is_none() || has_tool(base, "go"),
            "PATH or the installation, either is enough"
        );
        assert!(!has_tool(base, "definitely-not-a-real-tool-xyz"));
    }

    #[test]
    fn a_tool_resolves_to_the_installation_only_when_it_exists() {
        let temp = TempDir::new();
        let base = temp.path();

        assert_eq!(resolve_tool(base, "php"), OsString::from("php"));
        fixture(base, "bin/php/php.exe", "");
        assert_eq!(
            resolve_tool(base, "php"),
            super::composer_path(base)
                .parent()
                .expect("bin/php")
                .join("php.exe")
                .into_os_string()
        );
        // A name with no known path stays a bare name for `PATH`.
        assert_eq!(resolve_tool(base, "ruby"), OsString::from("ruby"));
    }

    #[test]
    fn the_runtime_status_line_names_every_runtime() {
        let temp = TempDir::new();
        let text = runtime_status_text(temp.path());
        for label in ["PHP", "Composer", "Node.js", "Python", "Java", "Go"] {
            assert!(text.contains(label), "{label} missing from `{text}`");
        }
        assert_eq!(
            text.matches('\u{2713}').count() + text.matches('\u{25cb}').count(),
            6
        );
        assert_eq!(text.matches("   ").count(), 5, "one separator per gap");
    }

    // ----------------------------------------------------------- composer

    #[test]
    fn an_existing_composer_is_reused_without_downloading() {
        let temp = TempDir::new();
        let base = temp.path();
        fixture(base, "bin/php/composer.phar", "<?php // composer");
        let (log, lines) = recorder();

        let path = ensure_composer(base, &log).expect("existing composer");
        assert_eq!(path, composer_path(base));
        assert!(
            logged(&lines).is_empty(),
            "nothing to download, nothing to say"
        );
    }

    #[test]
    fn composer_lands_beside_php_and_reports_an_unusable_directory() {
        let temp = TempDir::new();
        let base = temp.path();
        // `bin/php` is a file, so the directory Composer needs cannot be made.
        fixture(base, "bin/php", "not a directory");
        let (log, lines) = recorder();

        let error = ensure_composer(base, &log).expect_err("no directory for composer");
        assert!(
            matches!(error, Error::Io { .. }),
            "the original returned the MkdirAll error: {error}"
        );
        assert_eq!(
            logged(&lines),
            vec!["composer.phar not found \u{2014} downloading from getcomposer.org"]
        );
    }

    // ------------------------------------------------------------ runner

    #[test]
    fn an_empty_command_is_an_error() {
        let temp = TempDir::new();
        let (log, _) = recorder();
        let error = run_in_dir(temp.path(), temp.path(), &log, &[]).expect_err("empty");
        assert_eq!(error.to_string(), "empty command");
    }

    #[test]
    fn a_command_that_cannot_start_reports_the_program() {
        let temp = TempDir::new();
        let (log, _) = recorder();
        let error = run_in_dir(
            temp.path(),
            temp.path(),
            &log,
            &["lambo-no-such-program-xyz", "--version"],
        )
        .expect_err("no such program");
        let text = error.to_string();
        assert!(text.contains("lambo-no-such-program-xyz"), "{text}");
    }

    #[test]
    fn a_failing_command_reports_the_original_s_exit_status() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let step = failing_step("doomed", "3");

        let error = run_in_dir(temp.path(), temp.path(), &log, step.args).expect_err("exit 3");
        assert_eq!(error.to_string(), "exit status 3");
        // And a step failure is attributed to the step.
        let mapped = scaffold_cmd_steps(temp.path(), &PORTABLE, temp.path(), &log, &[step])
            .expect_err("step failed");
        assert_eq!(mapped.to_string(), "doomed: exit status 3");
        assert_eq!(
            logged(&lines),
            vec!["[1/1] doomed \u{2014} ".to_owned() + &step.args.join(" ")]
        );
    }

    #[test]
    fn a_command_writes_its_output_into_the_log() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        #[cfg(not(windows))]
        let argv = ["sh", "-c", "printf 'first\\n\\nsecond\\r\\n'"];
        #[cfg(windows)]
        let argv = ["cmd", "/C", "echo first&& echo.&& echo second"];

        run_in_dir(temp.path(), temp.path(), &log, &argv).expect("ran");
        let lines = logged(&lines);
        assert!(lines.iter().any(|line| line == "first"), "{lines:?}");
        assert!(lines.iter().any(|line| line == "second"), "{lines:?}");
        assert!(
            !lines.iter().any(|line| line.is_empty()),
            "blank lines are skipped: {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.ends_with('\r')),
            "carriage returns are trimmed: {lines:?}"
        );
    }

    #[test]
    fn streaming_drops_blank_and_unbounded_lines() {
        let (log, lines) = recorder();
        let mut input = String::from("\r\nkept\r\n");
        input.push_str(&"x".repeat(LOG_LINE_LIMIT + 1));
        input.push('\n');
        input.push_str("after\n");
        stream_to_log(std::io::Cursor::new(input), &log);

        assert_eq!(logged(&lines), vec!["kept", "after"]);
    }

    // -------------------------------------------------------- scaffolding

    #[test]
    fn a_non_empty_project_directory_is_refused() {
        let temp = TempDir::new();
        let (log, _) = recorder();
        let base = temp.path();
        let project = project_directory(base, "shop");
        fixture(&project, "index.html", "someone's work");

        let error = scaffold_framework(base, &PORTABLE, &project, &log).expect_err("not empty");
        assert_eq!(
            error.to_string(),
            format!("{} already exists and is not empty", project.display())
        );

        // An empty directory is accepted...
        let static_html = framework_by_name("Static HTML").expect("Static HTML");
        let empty = project_directory(base, "empty");
        crate::fsx::ensure_dir(&empty).expect("create");
        let result = scaffold_framework(base, static_html, &empty, &log).expect("scaffolded");
        assert_eq!(result.doc_root, empty);

        // ...and a missing one is created on the way.
        let missing = project_directory(base, "missing");
        assert!(!missing.exists());
        let result = scaffold_framework(base, static_html, &missing, &log).expect("scaffolded");
        assert!(result.doc_root.join("index.html").exists());
    }

    #[test]
    fn a_missing_required_tool_is_reported_verbatim() {
        let temp = TempDir::new();
        let (log, _) = recorder();
        let framework = Framework {
            name: "Static HTML",
            required_tools: &["lambo-no-such-tool-xyz"],
            ..FRAMEWORKS[16]
        };

        let error = scaffold_framework(temp.path(), &framework, &temp.join("site"), &log)
            .expect_err("missing tool");
        assert_eq!(
            error.to_string(),
            "missing lambo-no-such-tool-xyz: install it first (Settings \u{2192} runtimes)"
        );
    }

    #[test]
    fn composer_is_not_checked_for_as_a_tool() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let base = temp.path();
        // Every other tool of the composer frameworks is missing too, but the
        // check skips only composer - so the failure has to be the php one that
        // `scaffold_composer` raises, not a missing-tool error. Composer itself
        // is present, so nothing is downloaded here either.
        fixture(base, "bin/php/composer.phar", "<?php // composer");
        let framework = Framework {
            required_tools: &["composer"],
            ..*framework_by_name("Symfony").expect("Symfony")
        };

        let error = scaffold_framework(base, &framework, &project_directory(base, "s"), &log)
            .expect_err("no php.exe");
        assert_eq!(
            error.to_string(),
            format!(
                "php.exe not found at {} \u{2014} install the PHP service first",
                composer_path(base)
                    .parent()
                    .expect("bin/php")
                    .join("php.exe")
                    .display()
            )
        );
        assert!(
            logged(&lines).is_empty(),
            "nothing was logged before the check"
        );
    }

    #[test]
    fn a_command_framework_without_steps_is_refused() {
        let temp = TempDir::new();
        let (log, _) = recorder();
        // A command scaffold whose name has no steps registered: the original
        // refused it rather than creating an empty project directory.
        let framework = Framework {
            name: "Static HTML",
            kind: Kind::Cmd,
            required_tools: &[],
            ..PORTABLE
        };
        let project = project_directory(temp.path(), "app");
        crate::fsx::ensure_dir(&project).expect("create");

        let error = scaffold_cmd(temp.path(), &framework, &project, &log).expect_err("no steps");
        assert_eq!(
            error.to_string(),
            "no command steps registered for Static HTML"
        );
    }

    #[test]
    fn command_steps_are_logged_and_the_post_file_is_written() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let base = temp.path();
        let project = project_directory(base, "api");
        crate::fsx::ensure_dir(&project).expect("create");

        let result = scaffold_cmd_steps(
            base,
            &PORTABLE,
            &project,
            &log,
            &[portable_step("npm init"), portable_step("install express")],
        )
        .expect("scaffolded");

        assert_eq!(
            logged(&lines),
            vec![
                format!(
                    "[1/2] npm init \u{2014} {}",
                    portable_step("npm init").args.join(" ")
                ),
                format!(
                    "[2/2] install express \u{2014} {}",
                    portable_step("install express").args.join(" ")
                ),
                "  wrote index.js".to_owned(),
            ]
        );
        let written = fs::read_to_string(project.join("index.js")).expect("starter file");
        assert_eq!(written, EXPRESS_INDEX_JS);
        // Express is served from the project directory itself.
        assert_eq!(result.doc_root, project);
        assert_eq!(result.warning, None);
    }

    #[test]
    fn a_failed_step_writes_no_starter_file() {
        let temp = TempDir::new();
        let (log, _) = recorder();
        let base = temp.path();
        let project = project_directory(base, "api");
        crate::fsx::ensure_dir(&project).expect("create");

        scaffold_cmd_steps(
            base,
            &PORTABLE,
            &project,
            &log,
            &[failing_step("npm init", "1")],
        )
        .expect_err("step failed");
        assert!(!project.join("index.js").exists());
    }

    #[test]
    fn the_static_scaffold_writes_the_boilerplate() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let project = project_directory(temp.path(), "my-site");
        crate::fsx::ensure_dir(&project).expect("create");

        let result = scaffold_static(&project, &log).expect("scaffolded");
        let html = fs::read_to_string(project.join("index.html")).expect("index.html");

        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<title>my-site</title>"));
        assert!(html.contains("It works! \u{1f389}"));
        assert!(html.contains("served by Lambo PHP from"));
        assert!(html.contains(&project.to_string_lossy().into_owned()));
        assert!(html.contains("Edit <code>index.html</code> to get started."));
        assert_eq!(result.doc_root, project);
        assert_eq!(logged(&lines), vec!["created index.html boilerplate"]);
    }

    #[test]
    fn a_download_framework_uses_the_cache_and_strips_the_wrapper() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let base = temp.path();

        // A cached release, exactly as the downloader would have left it: the
        // archive the cache is asked for by name, holding the wrapper directory
        // the framework's `strip_top` removes.
        let wordpress = framework_by_name("WordPress").expect("WordPress");
        let cache = base.join(download_cache::DOWNLOADS_DIR);
        crate::fsx::ensure_dir(&cache).expect("cache");
        write_zip(
            &cache.join("WordPress.zip"),
            &[
                ("wordpress/", None),
                ("wordpress/wp-load.php", Some(b"<?php")),
                ("wordpress/wp-admin/", None),
                ("wordpress/wp-admin/index.php", Some(b"<?php")),
            ],
        );

        let project = project_directory(base, "blog");
        let result = scaffold_download(base, wordpress, &project, &log).expect("scaffolded");

        assert!(project.join("wp-load.php").exists(), "wrapper stripped");
        assert!(project.join("wp-admin").join("index.php").exists());
        assert!(!project.join("wordpress").exists());
        assert_eq!(result.doc_root, project, "WordPress serves its own root");

        let extracting = format!("extracting into {}", project.display());
        let lines = logged(&lines);
        assert_eq!(
            lines,
            vec!["  using cached WordPress.zip".to_owned(), extracting]
        );
    }

    /// Makes a fixture runnable, so a shell script can stand in for `php.exe`.
    #[cfg(unix)]
    fn make_runnable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path).expect("fixture").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("chmod");
    }

    /// Unix only: the fixture is a shell script standing in for `php.exe`, which
    /// is how the argument vectors are watched without a PHP installation. The
    /// vectors themselves are the same on Windows, where the lifecycle tests
    /// exercise the same code.
    #[cfg(unix)]
    #[test]
    fn the_composer_scaffold_creates_the_project_and_then_livewire() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let base = temp.path();
        let calls = base.join("calls.txt");
        let script = format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 0\n", calls.display());
        fixture(base, "bin/php/php.exe", &script);
        make_runnable(&base.join("bin").join("php").join("php.exe"));
        fixture(
            base,
            "bin/php/composer.phar",
            "<?php // not run by the fixture",
        );

        let framework = framework_by_name("Laravel + Livewire").expect("Livewire");
        let project = project_directory(base, "shop");
        // `scaffold_framework` creates this before dispatching, which is what
        // makes the project directory the command's working directory.
        crate::fsx::ensure_dir(&project).expect("create");
        let result = scaffold_composer(base, framework, &project, &log).expect("scaffolded");

        let calls = fs::read_to_string(&calls).expect("the fixture recorded its calls");
        let calls: Vec<&str> = calls.lines().collect();
        assert_eq!(
            calls[0],
            format!(
                "{} create-project --no-interaction laravel/laravel .",
                composer_path(base).display()
            )
        );
        assert_eq!(
            calls[1],
            format!(
                "{} require livewire/livewire --no-interaction",
                composer_path(base).display()
            )
        );

        let lines = logged(&lines);
        assert!(
            lines
                .iter()
                .any(|line| line == "adding Livewire via composer require...")
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("scaffolding Laravel + Livewire into"))
        );
        assert_eq!(result.doc_root, project.join("public"));
        assert_eq!(result.warning, None, "both commands succeeded");
    }

    // -------------------------------------------------------- registration

    #[test]
    fn a_project_name_is_required_before_anything_is_scaffolded() {
        let temp = TempDir::new();
        let (log, _) = recorder();
        let mut sink = Sink::default();
        let error =
            create_project(&mut sink, temp.path(), &PORTABLE, "", "", &log).expect_err("no name");
        assert_eq!(error.to_string(), "project name required");
        assert!(sink.projects.is_empty());
        assert!(!temp.path().join("www").exists());
    }

    #[test]
    fn an_empty_domain_defaults_to_the_project_name() {
        let temp = TempDir::new();
        let (log, _) = recorder();
        let base = temp.path();
        let mut sink = Sink::default();
        let static_html = framework_by_name("Static HTML").expect("Static HTML");

        let created =
            create_project(&mut sink, base, static_html, "my-shop", "", &log).expect("created");
        assert_eq!(created.domain, "my-shop.test");
        assert_eq!(sink.projects[0].domain, "my-shop.test");
        assert_eq!(sink.projects[0].framework, "Static HTML");
        assert_eq!(sink.projects[0].port, 0);
        assert_eq!(sink.hosts[0].port, 80);
        assert_eq!(sink.hosts[0].server_type, "apache");
        assert!(sink.hosts[0].enabled);
        assert_eq!(sink.hosts[0].proxy_port, 0);

        // A domain that was given is used as it is.
        let created = create_project(&mut sink, base, static_html, "other", "other.lan", &log)
            .expect("created");
        assert_eq!(created.domain, "other.lan");
    }

    // --------------------------------------------------------------- adopt

    #[test]
    fn an_existing_laravel_folder_is_loaded_with_what_detection_finds() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let mut sink = Sink::default();

        let folder = temp.path().join("My Shop");
        fs::create_dir_all(&folder).expect("the folder");
        fs::write(folder.join("artisan"), "#!/usr/bin/env php\n").expect("artisan");
        fs::write(
            folder.join("composer.json"),
            "{\"require\": {\"php\": \"^8.2\", \"laravel/framework\": \"^11.0\"}}",
        )
        .expect("composer.json");

        let adopted = adopt_project(&mut sink, temp.path(), &folder, &log).expect("adopted");
        assert_eq!(adopted.name, "my-shop");
        assert_eq!(adopted.framework, "Laravel");
        assert_eq!(adopted.domain, "my-shop.test");
        assert_eq!(adopted.doc_root, folder.join("public"));
        assert!(adopted.needs_database, "Laravel needs a database");
        assert_eq!(adopted.php_requirement.as_deref(), Some("^8.2"));
        assert!(adopted.summary.contains("Laravel"), "{}", adopted.summary);

        // The records a created project would have got, without scaffolding.
        assert_eq!(sink.projects.len(), 1);
        assert_eq!(sink.projects[0].framework, "Laravel");
        assert_eq!(sink.projects[0].domain, "my-shop.test");
        assert_eq!(
            sink.projects[0].port, 0,
            "an adopted project has no dev server"
        );
        assert_eq!(sink.hosts.len(), 1);
        assert_eq!(sink.hosts[0].port, 80);
        assert!(sink.hosts[0].enabled);
        assert_eq!(sink.hosts[0].proxy_port, 0);
        assert!(
            !folder.join("vendor").exists() && !folder.join("app").exists(),
            "nothing is scaffolded into an existing project"
        );

        // The detection the user sees names the framework and its evidence.
        let log = lines.lock().expect("log").join("\n");
        assert!(log.contains("my-shop"), "{log}");
        assert!(log.contains("Laravel"), "{log}");
        assert!(log.contains("needs a database"), "{log}");
        assert!(log.contains("asks for PHP ^8.2"), "{log}");
    }

    #[test]
    fn a_plain_php_folder_is_served_from_its_own_root() {
        let temp = TempDir::new();
        let (log, _) = recorder();
        let mut sink = Sink::default();

        let folder = temp.path().join("site");
        fs::create_dir_all(&folder).expect("the folder");
        fs::write(folder.join("index.php"), "<?php echo 'hi';\n").expect("index.php");

        let adopted = adopt_project(&mut sink, temp.path(), &folder, &log).expect("adopted");
        assert_eq!(adopted.framework, "plain PHP");
        assert_eq!(
            adopted.doc_root, folder,
            "plain PHP is served from its root"
        );
        assert!(!adopted.needs_database);
        assert_eq!(adopted.php_requirement, None);
    }

    #[test]
    fn a_folder_that_is_not_there_is_refused() {
        let temp = TempDir::new();
        let (log, _) = recorder();
        let mut sink = Sink::default();
        let missing = temp.path().join("nowhere");
        let error = adopt_project(&mut sink, temp.path(), &missing, &log).expect_err("refused");
        assert!(error.to_string().contains("is not a folder"));
        assert!(sink.projects.is_empty());
    }

    #[test]
    fn an_adopted_name_that_exists_already_is_refused() {
        let temp = TempDir::new();
        let (log, _) = recorder();
        let mut sink = Sink::default();

        let folder = temp.path().join("site");
        fs::create_dir_all(&folder).expect("the folder");
        fs::write(folder.join("index.php"), "<?php\n").expect("index.php");
        adopt_project(&mut sink, temp.path(), &folder, &log).expect("adopted");

        let error = adopt_project(&mut sink, temp.path(), &folder, &log).expect_err("duplicate");
        assert!(
            error.to_string().contains("already registered"),
            "{}",
            error
        );
        assert_eq!(sink.projects.len(), 1);
    }

    #[test]
    fn the_registered_records_are_the_originals() {
        let temp = TempDir::new();
        let base = temp.path();
        let next = framework_by_name("Next.js").expect("Next.js");
        let doc_root = project_directory(base, "app").join(".next");

        let (project, host) = project_records(base, next, "app", "app.test", &doc_root);
        assert_eq!(project.name, "app");
        assert_eq!(project.framework, "Next.js");
        assert_eq!(project.domain, "app.test");
        assert_eq!(project.docroot, doc_root.to_string_lossy());
        assert_eq!(project.port, 3000, "the dev server's port is recorded");
        assert_eq!(host.domain, "app.test");
        assert_eq!(host.docroot, "{base}/www/app/.next");
        assert_eq!(host.port, 80);
        assert_eq!(host.server_type, "apache");
        assert!(host.enabled);
        assert_eq!(host.proxy_port, 3000, "a dev-server framework is proxied");

        // A framework without a dev server gets a document-root vhost.
        let laravel = framework_by_name("Laravel").expect("Laravel");
        let (project, host) = project_records(
            base,
            laravel,
            "shop",
            "shop.test",
            &project_directory(base, "shop").join("public"),
        );
        assert_eq!(project.port, 0);
        assert_eq!(host.proxy_port, 0);
        assert_eq!(host.docroot, "{base}/www/shop/public");
    }

    #[test]
    fn the_document_root_reference_is_relative_with_forward_slashes() {
        // The Windows spelling of both paths, as the original saw them.
        let base = PathBuf::from(r"C:\lambo");
        let document = PathBuf::from(r"C:\lambo\www\shop\public");
        assert_eq!(
            document_root_reference(&base, &document),
            "{base}/www/shop/public"
        );

        // A trailing separator on the base directory changes nothing.
        assert_eq!(
            document_root_reference(Path::new(r"C:\lambo\"), &document),
            "{base}/www/shop/public"
        );

        // A path outside the installation keeps its absolute form.
        assert_eq!(
            document_root_reference(&base, Path::new(r"D:\sites\shop")),
            "D:/sites/shop"
        );
    }

    #[test]
    fn creating_a_project_records_then_applies_and_says_what_happened() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let base = temp.path();
        let mut sink = Sink::default();
        let static_html = framework_by_name("Static HTML").expect("Static HTML");

        let created =
            create_project(&mut sink, base, static_html, "my-shop", "", &log).expect("created");

        assert_eq!(sink.saved, 1, "the document is saved before the vhosts");
        assert_eq!(sink.applied, 1);
        assert!(
            project_directory(base, "my-shop")
                .join("index.html")
                .exists()
        );
        assert_eq!(created.doc_root, project_directory(base, "my-shop"));

        let lines = logged(&lines);
        assert_eq!(
            lines,
            vec![
                "created index.html boilerplate".to_owned(),
                format!(
                    "project 'my-shop' created at {} \u{2192} http://my-shop.test",
                    created.doc_root.display()
                ),
                "NOTE: Apache needs a restart to pick up the new vhost \u{2014} click 'Restart Stack'"
                    .to_owned(),
            ]
        );
    }

    #[test]
    fn a_failed_hosts_file_write_is_logged_with_the_administrator_hint_and_returned() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let mut sink = Sink {
            fail_apply: true,
            ..Sink::default()
        };
        let static_html = framework_by_name("Static HTML").expect("Static HTML");

        let error = create_project(&mut sink, temp.path(), static_html, "my-shop", "", &log)
            .expect_err("apply failed");
        assert_eq!(
            error.to_string(),
            "apply vhosts: failed to access the hosts file"
        );
        assert_eq!(sink.saved, 1, "the project is still registered");

        let lines = logged(&lines);
        assert!(
            lines
                .iter()
                .any(|line| line == "apply vhosts: failed to access the hosts file")
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("run Lambo PHP as administrator"))
        );
        assert!(
            !lines.iter().any(|line| line.starts_with("project '")),
            "a project that did not resolve is not announced as working: {lines:?}"
        );
    }

    #[test]
    fn a_failed_scaffold_registers_nothing() {
        let temp = TempDir::new();
        let (log, _) = recorder();
        let base = temp.path();
        let mut sink = Sink::default();
        let project = project_directory(base, "shop");
        fixture(&project, "composer.json", "{}");

        let laravel = framework_by_name("Laravel").expect("Laravel");
        create_project(&mut sink, base, laravel, "shop", "", &log).expect_err("not empty");
        assert!(sink.projects.is_empty());
        assert_eq!(sink.applied, 0);
    }

    #[test]
    fn deleting_a_project_removes_its_directory_its_record_and_its_domains() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let base = temp.path();
        let mut sink = Sink::default();
        let static_html = framework_by_name("Static HTML").expect("Static HTML");
        create_project(&mut sink, base, static_html, "my-shop", "", &log).expect("created");

        // A hand-added vhost on the same domain is the same site, and it goes
        // with the project: leaving it behind would serve a directory that is
        // gone.
        sink.hosts.push(Vhost {
            domain: "my-shop.test".to_owned(),
            ..sink.hosts[0].clone()
        });
        let root = project_directory(base, "my-shop");
        assert!(root.join("index.html").exists());

        assert!(
            delete_project(&mut sink, base, "my-shop", &log),
            "there was a project to delete"
        );
        assert!(!root.exists(), "the directory is gone");
        assert!(sink.projects.is_empty());
        assert!(sink.hosts.is_empty(), "every vhost on the domain is gone");
        assert_eq!(sink.removed, 1);
        assert_eq!(
            sink.applied, 2,
            "creation and deletion both apply the vhosts"
        );

        let lines = logged(&lines);
        assert!(
            lines
                .iter()
                .any(|line| line == "projects: deleting 'my-shop' (my-shop.test)..."),
            "{lines:?}"
        );
    }

    #[test]
    fn deleting_something_that_is_not_there_does_nothing() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let mut sink = Sink::default();

        assert!(!delete_project(&mut sink, temp.path(), "ghost", &log));
        assert!(logged(&lines).is_empty(), "nothing to say about nothing");
        assert_eq!(sink.applied, 0);
    }

    #[test]
    fn a_project_whose_directory_is_already_gone_is_still_unregistered() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let base = temp.path();
        let mut sink = Sink::default();
        let static_html = framework_by_name("Static HTML").expect("Static HTML");
        create_project(&mut sink, base, static_html, "my-shop", "", &log).expect("created");

        // `RemoveAll` answered nil for a path that is not there, so the
        // directory having been deleted by hand is not a failure.
        fs::remove_dir_all(project_directory(base, "my-shop")).expect("the fixture removes it");
        assert!(delete_project(&mut sink, base, "my-shop", &log));
        assert!(sink.projects.is_empty());

        let lines = logged(&lines);
        assert!(
            !lines
                .iter()
                .any(|line| line.starts_with("projects delete:")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_hosts_file_that_cannot_be_rewritten_is_reported_and_the_record_dropped_anyway() {
        let temp = TempDir::new();
        let (log, lines) = recorder();
        let base = temp.path();
        let mut sink = Sink::default();
        let static_html = framework_by_name("Static HTML").expect("Static HTML");
        create_project(&mut sink, base, static_html, "my-shop", "", &log).expect("created");
        sink.fail_apply = true;

        assert!(delete_project(&mut sink, base, "my-shop", &log));
        assert!(sink.projects.is_empty(), "the record is gone even so");

        let lines = logged(&lines);
        assert!(
            lines.iter().any(
                |line| line == "projects delete: apply vhosts: failed to access the hosts file"
            ),
            "{lines:?}"
        );
    }

    // ------------------------------------------------------------- naming

    #[test]
    fn the_project_slug_is_the_original_s_slugify() {
        assert_eq!(project_slug("My Shop!"), "my-shop");
        assert_eq!(
            project_slug("  Leading and trailing  "),
            "leading-and-trailing"
        );
        assert_eq!(project_slug("already-good"), "already-good");
        assert_eq!(project_slug("Version_2.0"), "version-2-0");
        assert_eq!(project_slug("a---b"), "a-b");
        assert_eq!(project_slug("Ünïcode Nàme"), "n-code-n-me");
        // Nothing usable stays nothing, so `create_project` rejects it - which
        // is where `naming::slugify`, used for Lambo's own projects, says `app`.
        assert_eq!(project_slug("!!!"), "");
        assert_eq!(project_slug(""), "");
        assert_eq!(naming::slugify("!!!"), "app");
    }

    #[test]
    fn domains_split_the_way_the_form_splits_them() {
        assert_eq!(
            split_domain("shop.test"),
            ("shop".to_owned(), ".test".to_owned())
        );
        assert_eq!(
            split_domain("  shop.lan  "),
            ("shop".to_owned(), ".lan".to_owned())
        );
        assert_eq!(
            split_domain("shop"),
            ("shop".to_owned(), ".test".to_owned()),
            "no extension means the default one"
        );
        assert_eq!(split_domain("shop.my-site.test").1, ".test");
        assert_eq!(DOMAIN_EXTENSIONS[0], ".test");
        assert_eq!(DOMAIN_EXTENSIONS.len(), 6);
    }
}
