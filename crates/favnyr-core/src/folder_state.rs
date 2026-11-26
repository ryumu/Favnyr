//! Per-folder presentation state shared by every tab, workspace and window.
//!
//! Manual ordering belongs to the directory being displayed, not to the tab
//! that happened to display it when the order was created. Keeping this data
//! outside the workspace also prevents named workspace snapshots from
//! diverging and makes navigation back to a directory restore its order.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::{Result, paths};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct FolderPresentation {
    /// Complete entry names in display order, including entries currently
    /// hidden by the view. An empty vector still means that manual ordering is
    /// enabled for an empty directory.
    #[serde(default)]
    manual_order: Vec<String>,
}

/// Presentation settings keyed by directory path.
///
/// A `BTreeMap` makes the TOML deterministic and keeps hand inspection useful.
/// Merely having an entry enables manual ordering for that directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderStateStore {
    #[serde(default, rename = "folder")]
    folders: BTreeMap<String, FolderPresentation>,
}

impl FolderStateStore {
    /// Loads a store without ever making startup fail. A malformed file is
    /// preserved before an empty store is returned.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(content) => match toml::from_str::<Self>(&content) {
                Ok(mut store) => {
                    store.refold_keys();
                    store
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        path = %path.display(),
                        "unreadable folder state store"
                    );
                    paths::preserve_unreadable(path);
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Saves atomically. When no directory uses folder-local state, the file
    /// itself is removed instead of retaining an empty shell.
    pub fn save(&self, path: &Path) -> Result<()> {
        if self.folders.is_empty() {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
            return Ok(());
        }
        let text = toml::to_string_pretty(self)
            .map_err(|err| Error::Workspace(format!("folder state serialization: {err}")))?;
        paths::write_atomic(path, &text)?;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.folders.is_empty()
    }

    pub fn is_manual(&self, directory: &Path) -> bool {
        self.folders.contains_key(&paths::storage_key(directory))
    }

    pub fn order(&self, directory: &Path) -> Option<&[String]> {
        self.folders
            .get(&paths::storage_key(directory))
            .map(|folder| folder.manual_order.as_slice())
    }

    pub fn order_mut(&mut self, directory: &Path) -> Option<&mut Vec<String>> {
        self.folders
            .get_mut(&paths::storage_key(directory))
            .map(|folder| &mut folder.manual_order)
    }

    /// Enables manual ordering and seeds it with the complete deterministic
    /// listing supplied by the caller. Existing state is left untouched.
    pub fn enable(&mut self, directory: &Path, names: Vec<String>) -> bool {
        let key = paths::storage_key(directory);
        if self.folders.contains_key(&key) {
            return false;
        }
        self.folders.insert(
            key,
            FolderPresentation {
                manual_order: unique_names(names),
            },
        );
        true
    }

    /// Disables every folder-local setting for this directory.
    pub fn disable(&mut self, directory: &Path) -> bool {
        self.folders
            .remove(&paths::storage_key(directory))
            .is_some()
    }

    /// Removes stale names, deduplicates the stored order and appends newly
    /// discovered names deterministically. The supplied names must come from a
    /// complete listing, including hidden entries.
    pub fn reconcile(&mut self, directory: &Path, names: &[String]) -> bool {
        let Some(order) = self.order_mut(directory) else {
            return false;
        };
        Self::reconcile_order(order, names)
    }

    /// Reconciles a snapshot of one folder's manual order against a complete listing.
    pub fn reconcile_order(order: &mut Vec<String>, names: &[String]) -> bool {
        let previous = order.clone();
        let existing: HashSet<&str> = names.iter().map(String::as_str).collect();
        let mut seen = HashSet::new();
        order.retain(|name| existing.contains(name.as_str()) && seen.insert(name.clone()));

        if order.is_empty() {
            order.extend(
                names
                    .iter()
                    .filter(|name| seen.insert((*name).clone()))
                    .cloned(),
            );
            return *order != previous;
        }

        let mut new_names: Vec<&String> = names
            .iter()
            .filter(|name| !seen.contains(name.as_str()))
            .collect();
        new_names.sort_by(|left, right| {
            left.to_lowercase()
                .cmp(&right.to_lowercase())
                .then_with(|| left.cmp(right))
        });
        order.extend(new_names.into_iter().cloned());
        *order != previous
    }

    /// Adds successful filesystem targets to their parent's order when that
    /// parent uses manual ordering.
    pub fn append_paths(&mut self, targets: &[PathBuf]) -> bool {
        let mut changed = false;
        for target in targets {
            let (Some(parent), Some(name)) = (
                target.parent(),
                target.file_name().and_then(|name| name.to_str()),
            ) else {
                continue;
            };
            let Some(order) = self.order_mut(parent) else {
                continue;
            };
            if !order.iter().any(|candidate| candidate == name) {
                order.push(name.to_owned());
                changed = true;
            }
        }
        changed
    }

    /// Removes paths from their parent's order without touching presentation
    /// state stored for a directory among those paths. This is suitable for a
    /// move or a recoverable trash operation.
    pub fn remove_paths(&mut self, paths_to_remove: &[PathBuf]) -> bool {
        let mut changed = false;
        for path in paths_to_remove {
            let (Some(parent), Some(name)) = (
                path.parent(),
                path.file_name().and_then(|name| name.to_str()),
            ) else {
                continue;
            };
            let Some(order) = self.order_mut(parent) else {
                continue;
            };
            let previous_len = order.len();
            order.retain(|candidate| candidate != name);
            changed |= order.len() != previous_len;
        }
        changed
    }

    /// Carries an item's slot and every configured directory below it after a
    /// successful in-app rename or move.
    pub fn relocate(&mut self, from: &Path, to: &Path) -> bool {
        let before = self.folders.clone();

        match (from.parent(), to.parent()) {
            (Some(from_parent), Some(to_parent)) if paths_equal(from_parent, to_parent) => {
                if let (Some(from_name), Some(to_name), Some(order)) = (
                    from.file_name().and_then(|name| name.to_str()),
                    to.file_name().and_then(|name| name.to_str()),
                    self.order_mut(from_parent),
                ) {
                    replace_name(order, from_name, to_name);
                }
            }
            _ => {
                self.remove_paths(&[from.to_path_buf()]);
                self.append_paths(&[to.to_path_buf()]);
            }
        }

        let folded_from = paths::fold_for_storage(from);
        let moved: Vec<(String, String, FolderPresentation)> = self
            .folders
            .iter()
            .filter_map(|(key, folder)| {
                let rest = Path::new(key).strip_prefix(&folded_from).ok()?;
                let destination = if rest.as_os_str().is_empty() {
                    to.to_path_buf()
                } else {
                    to.join(rest)
                };
                Some((
                    key.clone(),
                    paths::storage_key(&destination),
                    folder.clone(),
                ))
            })
            .collect();
        for (old, new, folder) in moved {
            self.folders.remove(&old);
            self.folders.insert(new, folder);
        }

        self.folders != before
    }

    /// Removes an item from its parent order and drops all directory state at
    /// or below that path. Reserved for permanent deletion.
    pub fn forget(&mut self, path: &Path) -> bool {
        let before = self.folders.clone();
        self.remove_paths(&[path.to_path_buf()]);
        let folded = paths::fold_for_storage(path);
        self.folders
            .retain(|key, _| Path::new(key).strip_prefix(&folded).is_err());
        self.folders != before
    }

    fn refold_keys(&mut self) {
        if self
            .folders
            .keys()
            .all(|key| paths::storage_key(Path::new(key)) == *key)
        {
            return;
        }
        self.folders = std::mem::take(&mut self.folders)
            .into_iter()
            .map(|(key, folder)| (paths::storage_key(Path::new(&key)), folder))
            .collect();
    }
}

fn unique_names(names: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    names
        .into_iter()
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

fn replace_name(order: &mut Vec<String>, from: &str, to: &str) -> bool {
    if !order.iter().any(|name| name == from) {
        return false;
    }
    let previous = order.clone();
    let mut replaced = false;
    order.retain_mut(|name| {
        if name == from && !replaced {
            *name = to.to_owned();
            replaced = true;
            true
        } else {
            name != to && name != from
        }
    });
    *order != previous
}

#[cfg(windows)]
fn paths_equal(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(windows))]
fn paths_equal(left: &Path, right: &Path) -> bool {
    left == right
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        PathBuf::from("/somewhere").join(name)
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "favnyr-folder-state-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn presence_enables_even_an_empty_directory() {
        let mut store = FolderStateStore::default();
        let path = folder("folder01");
        assert!(store.enable(&path, Vec::new()));
        assert!(store.is_manual(&path));
        assert_eq!(store.order(&path), Some([].as_slice()));
        assert!(store.disable(&path));
        assert!(!store.is_manual(&path));
    }

    #[test]
    fn reconciliation_removes_stale_names_and_appends_new_ones() {
        let mut store = FolderStateStore::default();
        let path = folder("folder01");
        store.enable(
            &path,
            vec!["b.txt".into(), "gone.txt".into(), "a.txt".into()],
        );

        assert!(store.reconcile(&path, &["c.txt".into(), "a.txt".into(), "b.txt".into()]));
        assert_eq!(
            store.order(&path),
            Some(["b.txt", "a.txt", "c.txt"].map(str::to_string).as_slice())
        );
    }

    #[test]
    fn same_parent_rename_preserves_the_slot() {
        let mut store = FolderStateStore::default();
        let path = folder("folder01");
        store.enable(&path, vec!["a.txt".into(), "b.txt".into(), "c.txt".into()]);

        assert!(store.relocate(&path.join("b.txt"), &path.join("renamed.txt")));
        assert_eq!(
            store.order(&path),
            Some(
                ["a.txt", "renamed.txt", "c.txt"]
                    .map(str::to_string)
                    .as_slice()
            )
        );
    }

    #[test]
    fn moving_a_directory_rebases_its_descendant_state() {
        let mut store = FolderStateStore::default();
        let source = folder("folder01");
        let destination = folder("folder02");
        store.enable(&source, vec!["inside.txt".into()]);
        store.enable(&source.join("nested"), vec!["deeper.txt".into()]);

        assert!(store.relocate(&source, &destination));
        assert!(!store.is_manual(&source));
        assert_eq!(
            store.order(&destination),
            Some(["inside.txt".to_string()].as_slice())
        );
        assert!(store.is_manual(&destination.join("nested")));
    }

    #[test]
    fn permanent_delete_forgets_the_whole_subtree() {
        let mut store = FolderStateStore::default();
        let source = folder("folder01");
        store.enable(&source, vec!["inside.txt".into()]);
        store.enable(&source.join("nested"), Vec::new());
        store.enable(&folder("folder02"), Vec::new());

        assert!(store.forget(&source));
        assert!(!store.is_manual(&source));
        assert!(!store.is_manual(&source.join("nested")));
        assert!(store.is_manual(&folder("folder02")));
    }

    #[test]
    fn round_trip_keeps_empty_enabled_folders() {
        let mut store = FolderStateStore::default();
        let path = folder("folder01");
        store.enable(&path, Vec::new());
        let text = toml::to_string_pretty(&store).unwrap();
        assert!(!text.contains("version"));
        let restored: FolderStateStore = toml::from_str(&text).unwrap();
        assert!(restored.is_manual(&path));
        assert_eq!(restored.order(&path), Some([].as_slice()));
    }

    #[test]
    fn save_load_round_trip_and_empty_cleanup() {
        let root = scratch("round-trip");
        let file = root.join("folder_state.toml");
        let directory = folder("folder01");
        let mut store = FolderStateStore::default();
        store.enable(&directory, vec!["b.txt".into(), "a.txt".into()]);

        store.save(&file).unwrap();
        assert_eq!(FolderStateStore::load(&file), store);

        store.disable(&directory);
        store.save(&file).unwrap();
        assert!(!file.exists(), "an empty store must leave no stale file");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_corrupted_file_is_preserved_before_starting_empty() {
        let root = scratch("corrupt");
        let file = root.join("folder_state.toml");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&file, "not = [valid").unwrap();

        let store = FolderStateStore::load(&file);
        assert!(store.is_empty());
        assert!(!file.exists());
        assert!(root.join("folder_state.toml.corrupt").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
