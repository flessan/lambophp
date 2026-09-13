//! Platform facts: which operating system, which CPU, which conventions.
//!
//! Lambo runs on Windows first and on Linux/macOS second, so platform
//! differences are *data*, never surprises: every place in the engine that
//! has to care about the host asks this module instead of sprinkling
//! `#[cfg(windows)]` through business logic.
//!
//! The functions here are pure - they take the facts they need as arguments
//! and never touch the environment themselves - so the Windows code paths
//! stay testable on any host. The `*_host()` wrappers are the only ones that
//! read the real system.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// A supported operating system family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    /// Microsoft Windows (10, 11, Server).
    Windows,
    /// Linux distributions.
    Linux,
    /// Apple macOS.
    MacOs,
    /// Anything else (BSDs, …); treated as Unix-like.
    OtherUnix,
}

impl Os {
    /// The operating system this binary is running on.
    pub fn host() -> Self {
        Self::from_rust_os(std::env::consts::OS)
    }

    /// Maps Rust's `std::env::consts::OS` value onto [`Os`].
    pub fn from_rust_os(name: &str) -> Self {
        match name {
            "windows" => Self::Windows,
            "linux" | "android" => Self::Linux,
            "macos" | "ios" => Self::MacOs,
            _ => Self::OtherUnix,
        }
    }

    /// Stable machine name (`windows`, `linux`, `macos`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Linux => "linux",
            Self::MacOs => "macos",
            Self::OtherUnix => "unix",
        }
    }

    /// Human-readable name used in `lambo doctor` output.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Windows => "Windows",
            Self::Linux => "Linux",
            Self::MacOs => "macOS",
            Self::OtherUnix => "Unix",
        }
    }

    /// `true` on Microsoft Windows.
    pub fn is_windows(self) -> bool {
        matches!(self, Self::Windows)
    }

    /// `true` where `/` separates path components and `kill`-style
    /// termination exists (Linux, macOS, BSDs).
    pub fn is_unix_like(self) -> bool {
        !self.is_windows()
    }

    /// `true` where the filesystem ignores letter case by default.
    ///
    /// Windows always does; macOS does for the default APFS/HFS+ setup.
    pub fn is_case_insensitive(self) -> bool {
        matches!(self, Self::Windows | Self::MacOs)
    }

    /// Extension of executables on this platform (`exe`, or `""`).
    pub fn exe_extension(self) -> &'static str {
        match self {
            Self::Windows => "exe",
            _ => "",
        }
    }

    /// Appends the platform executable extension when needed.
    ///
    /// `php` becomes `php.exe` on Windows and stays `php` elsewhere. An
    /// explicit extension in the input is preserved.
    pub fn executable_name(self, program: &str) -> String {
        let extension = self.exe_extension();
        if extension.is_empty() || program.ends_with(&format!(".{extension}")) {
            program.to_owned()
        } else {
            format!("{program}.{extension}")
        }
    }

    /// Directories that are searched for helper programs, in order.
    ///
    /// Lambo prefers its own managed runtimes, but will happily use a
    /// system installation when the user already has one. `PATH` is
    /// consulted separately; this list covers the conventional install
    /// locations that are frequently *not* on `PATH` on Windows.
    pub fn program_search_dirs(self) -> Vec<PathBuf> {
        match self {
            Self::Windows => [
                r"C:\Program Files\Apache Software Foundation",
                r"C:\Apache24",
                r"C:\php",
                r"C:\Program Files\MariaDB 11.4\bin",
                r"C:\Program Files\MySQL\MySQL Server 8.0\bin",
            ]
            .map(PathBuf::from)
            .to_vec(),
            _ => ["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin", "/bin"]
                .map(PathBuf::from)
                .to_vec(),
        }
    }
}

impl std::fmt::Display for Os {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display_name())
    }
}

/// A CPU architecture Lambo supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    /// 64-bit x86 (AMD64) - the primary Windows target.
    X86_64,
    /// 64-bit ARM (Apple Silicon, Raspberry Pi 4+, Windows on ARM).
    Aarch64,
    /// 32-bit x86; supported only for completeness.
    X86,
}

impl Arch {
    /// The architecture this binary was built for.
    pub fn host() -> Self {
        Self::from_rust_arch(std::env::consts::ARCH)
    }

    /// Maps Rust's `std::env::consts::ARCH` value onto [`Arch`].
    pub fn from_rust_arch(name: &str) -> Self {
        match name {
            "x86_64" => Self::X86_64,
            "aarch64" => Self::Aarch64,
            _ => Self::X86,
        }
    }

    /// Stable machine name (`x86_64`, `aarch64`, `x86`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
            Self::X86 => "x86",
        }
    }

    /// The spelling used by upstream PHP/Apache/MariaDB download sites.
    pub fn download_name(self) -> &'static str {
        match self {
            Self::X86_64 => "x64",
            Self::Aarch64 => "arm64",
            Self::X86 => "x86",
        }
    }

    /// Short human-readable name (`x64`, `arm64`).
    pub fn short_name(self) -> &'static str {
        match self {
            Self::X86_64 => "x64",
            Self::Aarch64 => "arm64",
            Self::X86 => "x86",
        }
    }
}

impl std::fmt::Display for Arch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.short_name())
    }
}

/// The host platform as a single value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    /// Operating system.
    pub os: Os,
    /// CPU architecture.
    pub arch: Arch,
}

impl Platform {
    /// The platform this process runs on.
    pub fn host() -> Self {
        Self {
            os: Os::host(),
            arch: Arch::host(),
        }
    }

    /// An explicit platform, for tests and for catalogue lookups.
    pub const fn new(os: Os, arch: Arch) -> Self {
        Self { os, arch }
    }

    /// Key used in download catalogues, e.g. `windows-x64`.
    pub fn key(&self) -> String {
        format!("{}-{}", self.os.as_str(), self.arch.download_name())
    }

    /// Parses a catalogue platform key back into its parts.
    ///
    /// This is the inverse of [`Self::key`] and the only supported way to read
    /// a catalogue platform: splitting the string by hand means an unknown
    /// architecture silently becomes whatever the fallback says, which is how
    /// a Linux binary ends up installed for Windows. `None` means the key is
    /// not a platform Lambo knows, and callers must report that rather than
    /// guess.
    pub fn from_key(key: &str) -> Option<Self> {
        let (os, arch) = key.split_once('-')?;
        let os = match os {
            "windows" => Os::Windows,
            "linux" => Os::Linux,
            "macos" => Os::MacOs,
            _ => return None,
        };
        let arch = match arch {
            "x64" => Arch::X86_64,
            "arm64" => Arch::Aarch64,
            "x86" => Arch::X86,
            _ => return None,
        };
        Some(Self { os, arch })
    }

    /// Whether Lambo can manage runtimes for this platform.
    pub fn is_supported(&self) -> bool {
        matches!(
            (self.os, self.arch),
            (Os::Windows, Arch::X86_64)
                | (Os::Linux, Arch::X86_64 | Arch::Aarch64)
                | (Os::MacOs, Arch::X86_64 | Arch::Aarch64)
        )
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.os.display_name(), self.arch.short_name())
    }
}

/// Resolves the default data directory for a platform.
///
/// Windows: `%USERPROFILE%\Lambo` (i.e. `C:\Users\<user>\Lambo`), so the
/// whole environment sits in one place a user can see and back up.
/// Unix: `$HOME/.lambo`, following the dotfile convention.
///
/// Takes the home directory as an argument so the logic is testable on any
/// host; see [`Paths::detect`](crate::paths::Paths::detect) for the
/// environment-aware wrapper.
pub fn default_data_dir(os: Os, home: impl AsRef<Path>) -> Result<PathBuf> {
    let home = home.as_ref();
    if home.as_os_str().is_empty() {
        return Err(Error::NoHomeDir);
    }
    Ok(match os {
        Os::Windows => home.join("Lambo"),
        _ => home.join(".lambo"),
    })
}

/// Names a Windows filesystem forbids, regardless of extension.
///
/// A project called `con` or a database called `aux` is unrepresentable on
/// Windows, so validation rejects those names up front instead of failing
/// half-way through `lambo init`.
const WINDOWS_RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Characters Windows forbids in file and directory names.
const WINDOWS_FORBIDDEN: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Whether `name` is usable as a single path component on Windows.
///
/// Rejects reserved device names, control characters and characters the
/// Win32 API refuses. Unix allows most of these, but Lambo validates
/// everywhere so a project created on macOS keeps working after the
/// directory is copied to Windows.
pub fn is_valid_path_component(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    if name
        .chars()
        .any(|c| c.is_control() || WINDOWS_FORBIDDEN.contains(&c))
    {
        return false;
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return false;
    }
    let stem = name.split('.').next().unwrap_or(name);
    !WINDOWS_RESERVED.contains(&stem.to_ascii_uppercase().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_platform_is_supported_or_explains_itself() {
        let platform = Platform::host();
        // Whatever this test runs on, `Platform` must render and key cleanly.
        assert!(!platform.key().is_empty());
        assert!(!platform.to_string().is_empty());
        assert!(matches!(
            platform.os,
            Os::Windows | Os::Linux | Os::MacOs | Os::OtherUnix
        ));
    }

    #[test]
    fn os_mapping_is_stable() {
        assert_eq!(Os::from_rust_os("windows"), Os::Windows);
        assert_eq!(Os::from_rust_os("linux"), Os::Linux);
        assert_eq!(Os::from_rust_os("macos"), Os::MacOs);
        assert_eq!(Os::from_rust_os("freebsd"), Os::OtherUnix);
        assert!(Os::Windows.is_windows());
        assert!(Os::Linux.is_unix_like());
        assert!(Os::MacOs.is_case_insensitive());
        assert!(!Os::Linux.is_case_insensitive());
    }

    #[test]
    fn executable_names_gain_an_extension_only_on_windows() {
        assert_eq!(Os::Windows.executable_name("php"), "php.exe");
        assert_eq!(Os::Windows.executable_name("php.exe"), "php.exe");
        assert_eq!(Os::Windows.executable_name("httpd"), "httpd.exe");
        assert_eq!(Os::Linux.executable_name("php"), "php");
        assert_eq!(Os::MacOs.executable_name("mariadbd"), "mariadbd");
        assert_eq!(Os::Windows.exe_extension(), "exe");
        assert_eq!(Os::Linux.exe_extension(), "");
    }

    #[test]
    fn program_search_dirs_are_platform_specific() {
        let windows = Os::Windows.program_search_dirs();
        // Compared as strings: `Path::starts_with` matches whole components,
        // and on a Unix host a Windows path is a single component.
        assert!(
            windows
                .iter()
                .any(|p| p.to_string_lossy().starts_with(r"C:\Program Files"))
        );
        assert!(
            !windows
                .iter()
                .any(|p| p.to_string_lossy().starts_with("/usr"))
        );

        let linux = Os::Linux.program_search_dirs();
        assert!(linux.contains(&PathBuf::from("/usr/bin")));
    }

    #[test]
    fn default_data_dir_follows_platform_convention() {
        let windows_home = PathBuf::from(r"C:\Users\Thio");
        let windows = default_data_dir(Os::Windows, &windows_home).unwrap();
        assert_eq!(windows, windows_home.join("Lambo"));
        assert!(windows.to_string_lossy().ends_with("Lambo"));

        let linux = default_data_dir(Os::Linux, "/home/thio").unwrap();
        assert_eq!(linux, PathBuf::from("/home/thio/.lambo"));

        let macos = default_data_dir(Os::MacOs, "/Users/thio").unwrap();
        assert_eq!(macos, PathBuf::from("/Users/thio/.lambo"));

        assert!(default_data_dir(Os::Windows, "").is_err());
    }

    #[test]
    fn windows_reserved_names_are_rejected_everywhere() {
        for name in ["con", "CON", "aux.txt", "nul.tar.gz", "COM1", "lpt9"] {
            assert!(!is_valid_path_component(name), "{name} must be rejected");
        }
        for name in ["shop", "my-project", "index.php", "a b", "Ünïcode"] {
            assert!(is_valid_path_component(name), "{name} must be accepted");
        }
    }

    #[test]
    fn windows_forbidden_characters_are_rejected() {
        for name in [
            "a<b", "a>b", "a:b", "a\"b", "a|b", "a?b", "a*b", "trail.", "trail ",
        ] {
            assert!(!is_valid_path_component(name), "{name} must be rejected");
        }
        assert!(!is_valid_path_component(""));
        assert!(!is_valid_path_component("."));
        assert!(!is_valid_path_component(".."));
    }

    #[test]
    fn platform_keys_match_catalogue_convention() {
        assert_eq!(
            Platform::new(Os::Windows, Arch::X86_64).key(),
            "windows-x64"
        );
        assert_eq!(Platform::new(Os::Linux, Arch::Aarch64).key(), "linux-arm64");
        assert_eq!(Platform::new(Os::MacOs, Arch::X86_64).key(), "macos-x64");
        assert!(Platform::new(Os::Windows, Arch::X86_64).is_supported());
        assert!(!Platform::new(Os::OtherUnix, Arch::X86).is_supported());
    }
}
