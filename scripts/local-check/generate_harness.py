#!/usr/bin/env python3
"""Builds a rustc-only harness around the real `lambo-core` sources.

`cargo test` cannot run where the crate registry cannot be reached: `serde`,
`thiserror`, `semver`, `flate2` and `crc32fast` are all out of reach, and the
`--locked` build means they cannot be substituted either. This script is how the
engine's unit tests were still run while the port was being written:

  * modules that need no external crate are compiled **from their real files**
    (via `#[path = "..."] mod x;`), so the tests that run here are the tests
    that ship;
  * `error` and `panel` are the real files with the `#[error(..)]` attributes
    turned into a hand-written `Display` impl and the `serde` derives dropped -
    no logic is touched;
  * four small modules are replaced (`vendor`, `archive`, `runtime`, `testutil`)
    (`frameworks` is real: the scaffold rules, the tool resolution and the
    project records need no `config` and no `catalog`, which is what lets the
    whole framework subsystem be tested here)
    where they would otherwise need a crate that cannot be fetched. `archive` and
    `testutil` are real enough to read and write stored-entry ZIP files, so the
    installer's extraction path is exercised rather than mocked.

What is *not* here, and why: `session.rs`, `apache.rs`, `database.rs`, `dbui.rs`,
`doctor.rs` and the integration tests sit on `serde`/`semver` through
`config`, `catalog` and `project` - a stub for one of them is a stub for the
catalogue format, the project file and the global configuration at once, which is
more than a harness can honestly fake. Those modules are compiled by CI; locally
they are checked with `check-format.sh`, whose formatter parses every file it
formats.

Run `--report` to print what is real and what is not, which is the honest
summary of what a green harness run proves.

Usage: generate_harness.py [--out DIR] [--report]
"""

import argparse
import os
import re
import shutil
import subprocess
import sys

REPOSITORY = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
REAL = os.path.join(REPOSITORY, "crates", "lambo-core", "src")
GUI = os.path.join(REPOSITORY, "crates", "lambo-gui", "src")

# Modules compiled from their real file. The value is why they can be.
REAL_MODULES = {
    "platform": "",
    "paths": "",
    "sha256": "",
    "secret": "",
    "fsx": "",
    "browser": "",
    "frameworks": "",
    "logs": "",
    "process": "",
    "naming": "",
    "port": "",
    "download": "",
    "download_cache": "",
    "catalog_panel": "",
    "console": "",
    "postinstall": "",
    "vhost": "",
    "installer": "",
    "tray": "",
    "service": "",
    "stack": "",
    "ui_state": "",
    "zombies": "",
}

# Modules that are transcribed rather than compiled as they are.
TRANSFORMED = {
    "error": "the `thiserror` derive becomes a hand-written `Display` impl",
    "panel": "the `serde` derives are dropped and `load`/`save` are removed (they are "
             "`serde_json`'s, and so are their tests); the state model is verbatim",
    "runtime": "only `RuntimeKind` is kept; the rest of the file needs `semver`",
}

# Modules that are replaced.
STUBBED = {
    "catalog": "a `serde` document reader; `Family` and `Release` are transcribed verbatim",
    "vendor": "reads Apache Lounge and Zig with `serde_json`; the stub always fails",
    "archive": "needs `flate2` and `crc32fast`; the stub reads stored-entry ZIPs",
    "testutil": "needs `flate2` for tar.gz; the stub writes stored-entry ZIPs",
}


def read(name):
    with open(os.path.join(REAL, name), encoding="utf-8") as handle:
        return handle.read()


def write(directory, name, text):
    os.makedirs(directory, exist_ok=True)
    with open(os.path.join(directory, name), "w", encoding="utf-8") as handle:
        handle.write(text)


# --------------------------------------------------------------------- error


def extract_error_attributes(text):
    """Every `#[error(..)]` attribute, in order, and the text without them."""
    kept = []
    arguments = []
    lines = text.split("\n")
    index = 0
    while index < len(lines):
        line = lines[index]
        if not line.strip().startswith("#[error("):
            kept.append(line)
            index += 1
            continue

        collected = [line]
        depth = line.count("(") - line.count(")")
        while depth > 0:
            index += 1
            collected.append(lines[index])
            depth += lines[index].count("(") - lines[index].count(")")

        blob = "\n".join(collected)
        arguments.append(blob[blob.index("(") + 1 : blob.rindex(")")].strip())
        index += 1
    return "\n".join(kept), arguments


def error_variant_shapes(text):
    """The variants of `Error`, in order, as `(name, kind, field names)`."""
    body = text[text.index("pub enum Error {") :]
    body = body[: body.index("\n}\n")]
    shapes = []
    current = None
    for line in body.split("\n")[1:]:
        stripped = line.strip()
        if not stripped or stripped.startswith("//"):
            continue

        struct_variant = re.match(r"^([A-Z][A-Za-z0-9_]*)\s*\{$", stripped)
        if struct_variant:
            current = [struct_variant.group(1), "{", []]
            shapes.append(current)
            continue

        tuple_variant = re.match(r"^([A-Z][A-Za-z0-9_]*)\s*\((.*)\)\s*,?$", stripped)
        if tuple_variant:
            names = tuple_variant.group(2).strip()
            shapes.append(
                [
                    tuple_variant.group(1),
                    "(",
                    [f"_{i}" for i, _ in enumerate(names.split(","))] if names else [],
                ]
            )
            continue

        unit_variant = re.match(r"^([A-Z][A-Za-z0-9_]*)\s*,?$", stripped)
        if unit_variant:
            shapes.append([unit_variant.group(1), "", []])
            continue

        if current is not None:
            if stripped == "}":
                current = None
                continue
            field = re.match(r"^([a-z_][A-Za-z0-9_]*)\s*:", stripped)
            if field:
                current[2].append(field.group(1))
    return shapes


def generate_error(directory):
    text = read("error.rs")
    text = text.replace("use thiserror::Error;\n", "")
    text = text.replace("#[derive(Debug, Error)]", "#[derive(Debug)]")
    text = text.replace("serde_yaml::Error", "HarnessError")
    text = text.replace("serde_json::Error", "HarnessError")
    text, arguments = extract_error_attributes(text)
    shapes = error_variant_shapes(text)
    if len(shapes) != len(arguments):
        raise SystemExit(
            f"error.rs: {len(shapes)} variants but {len(arguments)} #[error] attributes"
        )

    arms = []
    for (name, kind, fields), argument in zip(shapes, arguments):
        if kind == "(":
            # `#[error(".. {0} ..")]` names a tuple field by position.
            for position in range(len(fields)):
                argument = argument.replace("{%d}" % position, "{_%d}" % position)
                argument = argument.replace("{%d:" % position, "{_%d:" % position)
            pattern = f"Self::{name}({', '.join(fields)})"
        elif kind == "{":
            pattern = "Self::%s { %s }" % (name, ", ".join(fields))
        else:
            pattern = f"Self::{name}"
        arms.append(f"            {pattern} => write!(f, {argument}),")

    display = """
/// The harness's stand-in for a `serde_yaml`/`serde_json` error.
#[derive(Debug)]
pub struct HarnessError;

impl std::fmt::Display for HarnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a serde error")
    }
}

impl std::error::Error for Error {}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
%s
        }
    }
}
""" % "\n".join(arms)

    write(directory, "error.rs", text.rstrip() + "\n" + display)


# --------------------------------------------------------------------- panel


def drop_function(text, signature):
    """Removes a function, its doc comment and its body."""
    start = text.index(signature)
    line_start = text.rfind("\n", 0, start) + 1
    # Walk back over the doc comment and attributes above it.
    while True:
        previous = text.rfind("\n", 0, line_start - 1) + 1
        line = text[previous:line_start].strip()
        if line.startswith("///") or line.startswith("#["):
            line_start = previous
            continue
        break
    depth = 0
    index = text.index("{", start)
    while index < len(text):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return text[:line_start] + text[index + 2 :]
        index += 1
    raise SystemExit("unbalanced braces while dropping " + signature)


def generate_panel(directory):
    text = read("panel.rs")
    text = text.replace("use serde::{Deserialize, Serialize};\n", "")
    text = re.sub(r"#\[serde\([^\n]*\)\]\n", "", text)
    # The attributes that wrap across lines (`args`, `env`, `extensions`), and
    # any that survive one line because the replacement above anchored on the
    # final `)]` rather than the first.
    text = re.sub(r"#\[serde\((?:[^()]|\([^()]*\))*?\)\]\n", "", text, flags=re.S)
    text = text.replace("Serialize, Deserialize, ", "")
    text = text.replace("Deserialize, Serialize, ", "")
    text = text.replace(", Serialize, Deserialize", "")
    # The document reader and writer are `serde_json`'s; the harness has no
    # JSON, and the rules they wrap are not what is being verified here.
    text = drop_function(text, "pub fn load(base_dir: &Path) -> Result<Self> {")
    text = drop_function(text, "pub fn save(&self, base_dir: &Path) -> Result<()> {")
    # And with them, the tests that round-trip a document.
    text = text[: text.index("#[cfg(test)]\nmod tests {")]
    write(directory, "panel.rs", "#![allow(dead_code, unused_imports)]\n" + text)


# ------------------------------------------------------------------- runtime


def generate_runtime(directory):
    """Only `RuntimeKind`: the rest of the file is built on `semver`."""
    text = read("runtime.rs")
    body = text[text.index("pub enum RuntimeKind {"):]
    body = body[: body.index("\n}\n") + 3]

    block = text[text.index("impl RuntimeKind {"):]
    block = block[: block.index("\n}\n")]

    methods = []
    for name in ("as_str", "display_name", "install_command"):
        start = block.index(f"    pub fn {name}(&self)")
        end = block.index("\n    }\n", start) + len("\n    }\n")
        methods.append(block[start:end])

    executable = text[text.index("pub fn is_executable_file(path: &Path, os: Os) -> bool {"):]
    executable = executable[: executable.index("\n}\n") + 3]
    bit = text[text.index("fn executable_bit(metadata: &fs::Metadata) -> bool {"):]
    bit = bit[: bit.index("\n}\n") + 3]

    write(
        directory,
        "runtime.rs",
        "//! `RuntimeKind` and the executable probe, extracted verbatim: the rest\n"
        "//! of `runtime.rs` is built on `semver`, which the harness cannot fetch.\n\n"
        "use std::fs;\n"
        "use std::path::Path;\n\n"
        "use crate::platform::Os;\n\n"
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n" + body + "\nimpl RuntimeKind {\n"
        + "\n".join(methods) + "}\n\n"
        + executable + "\n\n"
        "#[cfg(unix)]\n"
        + bit
        + "\n\n/// On a platform with no executable bit, existence is the best answer.\n"
        "#[cfg(not(unix))]\n"
        "fn executable_bit(_metadata: &fs::Metadata) -> bool {\n    true\n}\n",
    )


# --------------------------------------------------------------------- stubs

CATALOG = '''//! Just the two types `error.rs` names: `Family` and `Release`. The rest of
//! `catalog.rs` is a `serde` document reader, which the harness cannot fetch.

use std::fmt;

/// The sections of the catalogue, exactly as `catalog.rs` defines them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// PHP.
    Php,
    /// Apache httpd.
    Apache,
    /// MariaDB.
    Mariadb,
    /// Oracle MySQL.
    Mysql,
    /// The database manager (Adminer).
    DbUi,
}

impl Family {
    /// Every family, in display order.
    pub const ALL: [Self; 5] = [
        Self::Php,
        Self::Apache,
        Self::Mariadb,
        Self::Mysql,
        Self::DbUi,
    ];

    /// Key used in the catalogue document.
    pub fn key(self) -> &\'static str {
        match self {
            Self::Php => "php",
            Self::Apache => "apache",
            Self::Mariadb => "mariadb",
            Self::Mysql => "mysql",
            Self::DbUi => "dbui",
        }
    }

    /// Human-readable name.
    pub fn display_name(self) -> &\'static str {
        match self {
            Self::Php => "PHP",
            Self::Apache => "Apache",
            Self::Mariadb => "MariaDB",
            Self::Mysql => "MySQL",
            Self::DbUi => "Adminer",
        }
    }
}

impl fmt::Display for Family {
    fn fmt(&self, f: &mut fmt::Formatter<\'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// One release of one family, with the fields `error.rs` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Exact version, e.g. `8.4.2`.
    pub version: String,
    /// Platform key, e.g. `windows-x64`.
    pub platform: String,
    /// Download URL.
    pub url: String,
}
'''

VENDOR = '''//! Stub for `vendor.rs`, which parses Apache Lounge and Zig index pages with
//! `serde_json`. The installer only needs the resolved shape and a resolver
//! that fails, which is what the real one does when a page cannot be fetched.

use crate::download::Downloader;
use crate::error::{Error, Result};
use crate::logs::LogFn;

/// A resolved download, exactly as `vendor.rs` defines it.
///
/// `strip_top` is a `String` there and not an `Option<String>`, because that is
/// what the original's resolver signature returns; getting this wrong here once
/// let a type error in `catalog_panel::plan` go unnoticed by the whole harness.
/// Keep it identical to the real struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Where the artifact is.
    pub url: String,
    /// The name it is cached under.
    pub file_name: String,
    /// The wrapper directory to strip, empty when there is none.
    pub strip_top: String,
    /// The version the page advertised.
    pub version: String,
}

/// No page is fetched here.
pub fn resolve_apache_latest(_downloader: &dyn Downloader, _log: &LogFn) -> Result<Resolved> {
    Err(Error::InvalidInput(
        "harness: no index page is fetched".to_owned(),
    ))
}

/// No page is fetched here.
pub fn resolve_zig_latest(_downloader: &dyn Downloader, _log: &LogFn) -> Result<Resolved> {
    Err(Error::InvalidInput(
        "harness: no index page is fetched".to_owned(),
    ))
}
'''

ARCHIVE = '''//! Stub for `archive.rs`, which needs `flate2` and `crc32fast`.
//!
//! It reads ZIP archives with *stored* entries, which is what the harness's
//! `testutil::write_zip` writes, so the installer's extraction path is
//! exercised for real. Compressed entries and tar.gz are not supported here,
//! which means `archive.rs` itself is not verified by this harness.

use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};

/// The result of an extraction, as `archive.rs` reports it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Extraction {
    /// Number of files written.
    pub files: usize,
    /// Number of directories created.
    pub directories: usize,
    /// Top-level entries.
    pub top_level: Vec<String>,
    /// Total uncompressed bytes written.
    pub bytes: u64,
}

/// Extracts the stored entries of a ZIP archive.
pub fn extract_zip_with(
    archive: &Path,
    destination: &Path,
    strip_top: Option<&str>,
    progress: Option<&dyn Fn(usize, usize)>,
) -> Result<Extraction> {
    let data = fs::read(archive).map_err(|source| Error::Io {
        path: archive.to_path_buf(),
        source,
    })?;
    let entries =
        stored_entries(&data).ok_or_else(|| Error::InvalidInput("harness: not a zip".to_owned()))?;
    let total = entries.len();

    let mut extraction = Extraction::default();
    for (index, (name, contents)) in entries.iter().enumerate() {
        if let Some(progress) = progress {
            progress(index + 1, total);
        }
        let name = match strip_top {
            Some(prefix) => name.strip_prefix(prefix).unwrap_or(name.as_str()),
            None => name.as_str(),
        };
        if name.is_empty() {
            continue;
        }
        let target = destination.join(name);
        if name.ends_with('/') {
            fs::create_dir_all(&target).map_err(|source| Error::Io {
                path: target.clone(),
                source,
            })?;
            extraction.directories += 1;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|source| Error::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            let mut file = fs::File::create(&target).map_err(|source| Error::Io {
                path: target.clone(),
                source,
            })?;
            file.write_all(contents).map_err(|source| Error::Io {
                path: target.clone(),
                source,
            })?;
            extraction.files += 1;
            extraction.bytes += contents.len() as u64;
        }
        let top = name.split('/').next().unwrap_or(name).to_owned();
        if !extraction.top_level.contains(&top) {
            extraction.top_level.push(top);
        }
    }
    Ok(extraction)
}

/// Reads a ZIP's central directory, for entries stored without compression.
fn stored_entries(data: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    // The count and the directory's offset both live in the end-of-central-
    // directory record; the first central record starts with "PK\x01\x02".
    let (count, mut offset) = find_central_directory(data)?;
    let mut out = Vec::new();
    for _ in 0..count {
        let name_length = u16::from_le_bytes([data[offset + 28], data[offset + 29]]) as usize;
        let extra_length = u16::from_le_bytes([data[offset + 30], data[offset + 31]]) as usize;
        let comment_length = u16::from_le_bytes([data[offset + 32], data[offset + 33]]) as usize;
        let local_offset = u32::from_le_bytes([
            data[offset + 42],
            data[offset + 43],
            data[offset + 44],
            data[offset + 45],
        ]) as usize;
        let name = String::from_utf8(data[offset + 46..offset + 46 + name_length].to_vec()).ok()?;

        let local_name_length =
            u16::from_le_bytes([data[local_offset + 26], data[local_offset + 27]]) as usize;
        let local_extra_length =
            u16::from_le_bytes([data[local_offset + 28], data[local_offset + 29]]) as usize;
        let compressed = u32::from_le_bytes([
            data[local_offset + 18],
            data[local_offset + 19],
            data[local_offset + 20],
            data[local_offset + 21],
        ]) as usize;
        let start = local_offset + 30 + local_name_length + local_extra_length;
        out.push((name, data[start..start + compressed].to_vec()));

        offset += 46 + name_length + extra_length + comment_length;
    }
    Some(out)
}

/// The entry count and the offset of the first central-directory record.
fn find_central_directory(data: &[u8]) -> Option<(u16, usize)> {
    let signature = [0x50u8, 0x4b, 0x05, 0x06];
    let position = data.windows(4).rposition(|window| window == signature)?;
    let count = u16::from_le_bytes([data[position + 10], data[position + 11]]);
    let offset = u32::from_le_bytes([
        data[position + 16],
        data[position + 17],
        data[position + 18],
        data[position + 19],
    ]) as usize;
    Some((count, offset))
}
'''

TESTUTIL = '''//! Stub for `testutil.rs`, which needs `flate2` for its tar.gz writer.
//!
//! `TempDir`, `fixture` and `home` match the real ones. `write_zip` writes a
//! real ZIP with stored entries, which the harness's archive stub reads.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::paths::Paths;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique temporary directory that deletes itself on drop.
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates a new, empty temporary directory.
    pub(crate) fn new() -> Self {
        Self::named("lambo-harness")
    }

    fn named(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{nanos}-{count}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("failed to create temporary test directory");
        Self { path }
    }

    /// Path of the temporary directory.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Joins a path onto the temporary directory.
    pub(crate) fn join(&self, child: impl AsRef<Path>) -> PathBuf {
        self.path.join(child)
    }

    /// A fresh Lambo home rooted inside this directory.
    pub(crate) fn home(&self) -> Paths {
        let paths = Paths::from_root(self.path.join("lambo-home"));
        paths
            .ensure_layout()
            .expect("failed to create the Lambo layout");
        paths
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Writes a file inside `dir`, creating parent directories.
pub(crate) fn fixture(dir: &Path, relative: &str, contents: &str) {
    let path = dir.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create fixture directory");
    }
    fs::write(&path, contents).expect("failed to write fixture");
}

/// A ZIP with stored (uncompressed) entries: `(name, contents)`.
///
/// `None` writes a directory entry, which is how the real `testutil`'s archive
/// writer is called for a wrapper directory.
pub(crate) fn write_zip(path: &Path, entries: &[(&str, Option<&[u8]>)]) {
    let mut out = Vec::new();
    let mut central = Vec::new();
    let mut offset = 0u32;

    for (name, contents) in entries {
        let name = name.as_bytes();
        let contents = contents.unwrap_or(b"");
        let crc = crc32(contents);

        let mut local = Vec::new();
        local.extend_from_slice(&0x04034b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&(contents.len() as u32).to_le_bytes());
        local.extend_from_slice(&(contents.len() as u32).to_le_bytes());
        local.extend_from_slice(&(name.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name);
        local.extend_from_slice(contents);

        central.extend_from_slice(&0x02014b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(contents.len() as u32).to_le_bytes());
        central.extend_from_slice(&(contents.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);

        offset += local.len() as u32;
        out.extend_from_slice(&local);
    }

    let central_offset = out.len() as u32;
    let central_size = central.len() as u32;
    out.extend_from_slice(&central);

    out.extend_from_slice(&0x06054b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());

    let mut file = fs::File::create(path).expect("failed to create zip fixture");
    file.write_all(&out).expect("failed to write zip fixture");
}

/// CRC-32, the one part of the ZIP format the harness needs.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffffffffu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb88320 & mask);
        }
    }
    !crc
}
'''

LIB = '''//! A rustc-only harness around lambo-core's real modules.
//!
//! See `scripts/local-check/README.md` for what is real, what is transformed
//! and what is stubbed, which is the honest scope of a run of these tests.

#![allow(dead_code, unused_imports, unused_variables, unused_mut)]

/// The product name, as `lib.rs` defines it.
pub const PRODUCT: &str = "Lambo PHP";

/// The binary name, as `lib.rs` defines it.
pub const BIN_NAME: &str = "lambo";

#[path = "error.rs"]
pub mod error;
#[path = "catalog.rs"]
pub mod catalog;
#[path = "runtime.rs"]
pub mod runtime;
#[path = "vendor.rs"]
pub mod vendor;
#[path = "archive.rs"]
pub mod archive;
#[path = "testutil.rs"]
pub mod testutil;
#[path = "panel.rs"]
pub mod panel;

#[path = "{real}/platform.rs"]
pub mod platform;
#[path = "{real}/paths.rs"]
pub mod paths;
#[path = "{real}/sha256.rs"]
pub mod sha256;
#[path = "{real}/secret.rs"]
pub mod secret;
#[path = "{real}/fsx.rs"]
pub mod fsx;
#[path = "{real}/frameworks.rs"]
pub mod frameworks;
#[path = "{real}/browser.rs"]
pub mod browser;
#[path = "{real}/logs.rs"]
pub mod logs;
#[path = "{real}/process.rs"]
pub mod process;
#[path = "{real}/download.rs"]
pub mod download;
#[path = "{real}/download_cache.rs"]
pub mod download_cache;
#[path = "{real}/catalog_panel.rs"]
pub mod catalog_panel;
#[path = "{real}/postinstall.rs"]
pub mod postinstall;
#[path = "{real}/vhost.rs"]
pub mod vhost;
#[path = "{real}/installer.rs"]
pub mod installer;
#[path = "{real}/tray.rs"]
pub mod tray;
#[path = "{real}/service.rs"]
pub mod service;
#[path = "{real}/port.rs"]
pub mod port;
#[path = "{real}/naming.rs"]
pub mod naming;
#[path = "{real}/zombies.rs"]
pub mod zombies;
#[path = "{real}/stack.rs"]
pub mod stack;
#[path = "{real}/console.rs"]
pub mod console;
#[path = "{real}/ui_state.rs"]
pub mod ui_state;

// ---------------------------------------------------------------------------
// The interface's platform-independent half
// ---------------------------------------------------------------------------
//
// `lambo-gui` says `lambo_core::...` because that is the crate it depends on.
// Here the core *is* this crate, so it is aliased to that name and the
// interface's own modules compile unchanged - which is what makes the window's
// decisions testable on the machine the engine is developed on.

// A crate alias for the harness itself, so a module written against the real
// crate graph - `use lambo_core::ui_state::..` - resolves here too.
extern crate self as lambo_core;

#[path = "{gui}/view.rs"]
pub mod gui_view;

#[path = "{gui}/state.rs"]
pub mod gui_state;
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--out",
        default=os.path.join(REPOSITORY, "scripts", "local-check", "harness"),
        help="where to write the harness crate root",
    )
    parser.add_argument("--report", action="store_true", help="print the scope and exit")
    options = parser.parse_args()

    if options.report:
        print("compiled from the real sources:")
        for name in REAL_MODULES:
            print(f"  {name}")
        print("transformed, logic untouched:")
        for name, why in TRANSFORMED.items():
            print(f"  {name}: {why}")
        print("stubbed out (not verified here):")
        for name, why in STUBBED.items():
            print(f"  {name}: {why}")
        print("not in the harness at all (no stub written for them):")
        for name in ("apache", "config", "database", "dbui", "detect",
                     "doctor", "envfile", "http", "lambofile", "migration",
                     "pathenv", "php", "project", "session", "sources",
                     "version", "workspace", "yaml"):
            print(f"  {name}")
        print("  (the Windows-only code in them is not compiled here at all)")
        return 0

    shutil.rmtree(options.out, ignore_errors=True)
    generate_error(options.out)
    generate_panel(options.out)
    generate_runtime(options.out)
    write(options.out, "catalog.rs", CATALOG)
    write(options.out, "vendor.rs", VENDOR)
    write(options.out, "archive.rs", ARCHIVE)
    write(options.out, "testutil.rs", TESTUTIL)
    write(
        options.out,
        "lib.rs",
        LIB.format(real=REAL.replace("\\", "/"), gui=GUI.replace("\\", "/")),
    )

    print(f"harness written to {options.out}")
    print("build and run it with:")
    print(f"  rustc --edition 2024 --test --sysroot <sysroot> -o <out> {options.out}/lib.rs")
    return 0


if __name__ == "__main__":
    sys.exit(main())
