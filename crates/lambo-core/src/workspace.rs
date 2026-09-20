//! Workspaces: named groups of projects that share a lifecycle.
//!
//! The registry lives at `$LAMBO_HOME/config/workspaces.yml`; projects join a
//! workspace with `lambo workspace add`. Starting a whole workspace in one
//! command is not implemented yet (see docs/roadmap.md): the registry is
//! bookkeeping, and each project is still started on its own with `lambo up`.
//! All rules - de-duplication, cascades, on-disk schema - are enforced here so
//! no interface can corrupt the registry.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::paths::Paths;
use crate::yaml;

/// Workspace used when the user does not name one.
pub const DEFAULT_WORKSPACE: &str = "default";

/// Current schema version of the on-disk registry.
const SCHEMA_VERSION: u32 = 1;

const HEADER: &str = "\
# Lambo PHP project registry - managed by `lambo workspace`
";

/// Serialized shape of `workspaces.yml`. Versioned so future migrations
/// are additive instead of breaking.
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct WorkspacesFile {
    version: u32,
    workspaces: BTreeMap<String, Vec<PathBuf>>,
}

impl Default for WorkspacesFile {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            workspaces: BTreeMap::new(),
        }
    }
}

/// The in-memory registry of workspaces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Workspaces {
    map: BTreeMap<String, Vec<PathBuf>>,
}

impl Workspaces {
    /// Loads the registry; a missing file means an empty registry.
    pub fn load(paths: &Paths) -> Result<Self> {
        Self::load_from(&paths.workspaces_file())
    }

    /// Loads a registry from an arbitrary file.
    ///
    /// Used by [`crate::migration`] to read the registry of an older
    /// installation without copying it first.
    pub fn load_from(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let file: WorkspacesFile = yaml::from_str(&text).map_err(|source| Error::Yaml {
                    path: path.to_path_buf(),
                    source,
                })?;
                Ok(Self {
                    map: file.workspaces,
                })
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::io(path, e)),
        }
    }

    /// Persists the registry.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        let path = paths.workspaces_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let file = WorkspacesFile {
            version: SCHEMA_VERSION,
            workspaces: self.map.clone(),
        };
        let body = yaml::to_string(&file).map_err(Error::Serialize)?;
        fs::write(&path, format!("{HEADER}{body}")).map_err(|e| Error::io(&path, e))
    }

    /// The key a project is stored under.
    ///
    /// A directory that exists is canonicalized, so `lambo workspace add .`
    /// from two different shells records one entry and `remove` finds it
    /// again; a path that cannot be resolved (a project that has not been
    /// created yet) is kept exactly as it was given. The rule is here rather
    /// than in an interface so every caller keys the registry the same way.
    pub fn key(project: &Path) -> PathBuf {
        std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf())
    }

    /// Adds `project` to `workspace`.
    ///
    /// Returns `false` when the project was already registered - adding is
    /// idempotent and never duplicates.
    pub fn add(&mut self, workspace: &str, project: &Path) -> bool {
        let project = Self::key(project);
        let projects = self.map.entry(workspace.to_owned()).or_default();
        if projects.contains(&project) {
            return false;
        }
        projects.push(project);
        projects.sort();
        true
    }

    /// Removes `project` from `workspace`.
    ///
    /// Returns `Ok(false)` when the project was not registered, and errors
    /// with [`Error::WorkspaceNotFound`] when the workspace itself is
    /// unknown. Empty workspaces are removed automatically.
    pub fn remove(&mut self, workspace: &str, project: &Path) -> Result<bool> {
        let project = Self::key(project);
        let Some(projects) = self.map.get_mut(workspace) else {
            return Err(Error::WorkspaceNotFound(workspace.to_owned()));
        };
        let before = projects.len();
        projects.retain(|p| p != &project);
        let removed = projects.len() != before;
        if projects.is_empty() {
            self.map.remove(workspace);
        }
        Ok(removed)
    }

    /// Projects of a workspace.
    pub fn projects(&self, workspace: &str) -> Result<&[PathBuf]> {
        self.map
            .get(workspace)
            .map(Vec::as_slice)
            .ok_or_else(|| Error::WorkspaceNotFound(workspace.to_owned()))
    }

    /// Iterates all workspaces and their projects, alphabetically.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[PathBuf])> {
        self.map
            .iter()
            .map(|(name, projects)| (name.as_str(), projects.as_slice()))
    }

    /// Total number of registered projects across all workspaces.
    pub fn project_count(&self) -> usize {
        self.map.values().map(Vec::len).sum()
    }

    /// Whether nothing is registered yet.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn add_dedupes_and_sorts() {
        let mut reg = Workspaces::default();
        assert!(reg.add("personal", Path::new("/home/dev/shop")));
        assert!(reg.add("personal", Path::new("/home/dev/blog")));
        assert!(!reg.add("personal", Path::new("/home/dev/shop"))); // duplicate

        let projects = reg.projects("personal").unwrap();
        assert_eq!(
            projects,
            [
                PathBuf::from("/home/dev/blog"),
                PathBuf::from("/home/dev/shop")
            ]
        );
        assert_eq!(reg.project_count(), 2);
    }

    #[test]
    fn the_registry_key_is_the_canonical_path_of_an_existing_directory() {
        let temp = TempDir::new();
        std::fs::create_dir_all(temp.path().join("www").join("shop")).expect("fixture");
        let absolute = temp.path().join("www").join("shop");

        // The same directory reached two ways is one entry: that is what makes
        // `lambo workspace add .` and a later `remove .` agree.
        let mut reg = Workspaces::default();
        assert!(reg.add("personal", &absolute));
        let indirect = absolute.join(".").join("");
        assert!(
            !reg.add("personal", &indirect),
            "a second spelling of the same directory must not duplicate it"
        );
        assert_eq!(reg.projects("personal").unwrap().len(), 1);
        assert!(reg.remove("personal", &indirect).unwrap());
        assert!(reg.is_empty());

        // A path that does not exist yet is kept verbatim rather than being
        // dropped: the project may be created a moment later.
        let missing = temp.path().join("not-yet");
        assert_eq!(Workspaces::key(&missing), missing);
        assert!(reg.add("personal", &missing));
        assert_eq!(reg.projects("personal").unwrap(), [missing]);
    }

    #[test]
    fn remove_cascades_and_reports() {
        let mut reg = Workspaces::default();
        reg.add("personal", Path::new("/home/dev/shop"));

        assert!(matches!(
            reg.remove("other", Path::new("/x")),
            Err(Error::WorkspaceNotFound(_))
        ));
        assert!(!reg.remove("personal", Path::new("/elsewhere")).unwrap());
        assert!(reg.remove("personal", Path::new("/home/dev/shop")).unwrap());
        assert!(reg.is_empty(), "empty workspaces must be removed");
        assert!(reg.projects("personal").is_err());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let temp = TempDir::new();
        let paths = Paths::from_root(temp.path());

        let mut reg = Workspaces::default();
        reg.add("personal", Path::new("/home/dev/shop"));
        reg.add(DEFAULT_WORKSPACE, Path::new("/home/dev/api"));
        reg.save(&paths).unwrap();

        let raw = fs::read_to_string(paths.workspaces_file()).unwrap();
        assert!(raw.contains("version: 1"));

        assert_eq!(Workspaces::load(&paths).unwrap(), reg);
    }

    #[test]
    fn missing_file_is_empty_registry() {
        let temp = TempDir::new();
        let paths = Paths::from_root(temp.path());
        let reg = Workspaces::load(&paths).unwrap();
        assert!(reg.is_empty());
        assert_eq!(reg.iter().count(), 0);
    }
}
