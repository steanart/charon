//! Hears about changes inside the watched folders (FSEvents on macOS, ReadDirectoryChanges
//! on Windows, inotify on Linux) and reports which job they belong to, and where.

use crate::model::JobId;
use crate::scan::{is_partial, MARKER_FOLDER, TRASH_FOLDER};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A folder that couldn't be watched is tried again after this long.
const RETRY_AFTER: Duration = Duration::from_secs(60);

type Routes = Arc<Mutex<Vec<Route>>>;

/// A watched folder of a job: the path the job names it by, and the path it really has (a
/// link resolved), under which some systems report its changes (FSEvents on macOS).
#[derive(Debug, Clone)]
pub struct Route {
    root: PathBuf,
    real: Option<PathBuf>,
    job: JobId,
}

impl Route {
    pub fn new(root: &Path, real: Option<PathBuf>, job: &JobId) -> Self {
        Self { root: root.to_path_buf(), real: real.filter(|real| real != root), job: job.clone() }
    }

    /// The folder, as the job names it.
    #[cfg(test)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `path` as a path under the folder the job names, if it's in the folder under either
    /// of its paths.
    fn place(&self, path: &Path) -> Option<PathBuf> {
        if path.starts_with(&self.root) {
            return Some(path.to_path_buf());
        }
        let inside = path.strip_prefix(self.real.as_ref()?).ok()?;
        Some(if inside.as_os_str().is_empty() { self.root.clone() } else { self.root.join(inside) })
    }

    /// The jobs whose folders `path` is in, with the path under each job's folder (one per
    /// job, even when two of its routes match).
    pub fn find(routes: &[Route], path: &Path) -> Vec<(JobId, PathBuf)> {
        let mut found: Vec<(JobId, PathBuf)> = Vec::new();
        for route in routes {
            if found.iter().any(|(job, _)| job == &route.job) {
                continue;
            }
            if let Some(placed) = route.place(path) {
                found.push((route.job.clone(), placed));
            }
        }
        found
    }
}

pub struct FolderWatcher {
    watcher: RecommendedWatcher,
    routes: Routes,
    watched: HashSet<PathBuf>,
    failed: HashMap<PathBuf, Instant>,
}

impl FolderWatcher {
    /// A watcher that sends the id of the job whose folder changed, and the changed path
    /// (under the folder as the job names it), to `changes`.
    pub fn new(changes: Sender<(JobId, PathBuf)>) -> notify::Result<Self> {
        let routes: Routes = Arc::default();
        let lookup = routes.clone();
        let watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            let Ok(event) = result else { return };
            if event.kind.is_access() {
                return;
            }
            let routes = lookup.lock().expect("routes lock");
            let mut told: HashSet<JobId> = HashSet::new();
            for path in event.paths.iter().filter(|p| !Self::is_own_file(p)) {
                for (id, placed) in Route::find(&routes, path) {
                    if told.insert(id.clone()) {
                        let _ = changes.send((id, placed));
                    }
                }
            }
        })?;
        Ok(Self { watcher, routes, watched: HashSet::new(), failed: HashMap::new() })
    }

    /// Watches exactly the folders in `routes`, starting and stopping watches as needed.
    pub fn follow(&mut self, routes: Vec<Route>) {
        let wanted: HashSet<PathBuf> = routes.iter().map(|route| route.root.clone()).collect();

        for root in self.watched.difference(&wanted).cloned().collect::<Vec<_>>() {
            let _ = self.watcher.unwatch(&root);
            self.watched.remove(&root);
        }
        for root in wanted.difference(&self.watched).cloned().collect::<Vec<_>>() {
            if self.failed.get(&root).is_some_and(|t| t.elapsed() < RETRY_AFTER) {
                continue;
            }
            match self.watcher.watch(&root, RecursiveMode::Recursive) {
                Ok(()) => {
                    self.failed.remove(&root);
                    self.watched.insert(root);
                }
                Err(e) => {
                    log::warn!("couldn't watch {}: {e}", root.display());
                    self.failed.insert(root, Instant::now());
                }
            }
        }
        *self.routes.lock().expect("routes lock") = routes;
    }

    /// Charon's trash, markers and half-copied files: never a reason to look again.
    fn is_own_file(path: &Path) -> bool {
        path.components().any(|c| c.as_os_str() == TRASH_FOLDER || c.as_os_str() == MARKER_FOLDER)
            || path.file_name().is_some_and(|n| is_partial(&n.to_string_lossy()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_under_the_real_path_of_a_linked_folder_reaches_its_job() {
        let job = "job".to_string();
        let routes = vec![Route::new(Path::new("/Users/me/Dropbox"), Some(PathBuf::from("/Users/me/Library/CloudStorage/Dropbox")), &job)];
        // As the job names it, and as the system may report it.
        assert_eq!(Route::find(&routes, Path::new("/Users/me/Dropbox/a.txt")), vec![(job.clone(), PathBuf::from("/Users/me/Dropbox/a.txt"))]);
        assert_eq!(
            Route::find(&routes, Path::new("/Users/me/Library/CloudStorage/Dropbox/Work/a.txt")),
            vec![(job.clone(), PathBuf::from("/Users/me/Dropbox/Work/a.txt"))]
        );
        assert_eq!(Route::find(&routes, Path::new("/Users/me/Library/CloudStorage/Dropbox")), vec![(job, PathBuf::from("/Users/me/Dropbox"))]);
        assert!(Route::find(&routes, Path::new("/Users/me/Library/CloudStorage/Dropbox2/a.txt")).is_empty());
    }

    #[test]
    fn a_change_in_two_jobs_folders_reaches_both_once() {
        let (one, two) = ("one".to_string(), "two".to_string());
        let routes = vec![
            Route::new(Path::new("/data/Docs"), None, &one),
            Route::new(Path::new("/backup/Docs"), Some(PathBuf::from("/data/Docs")), &one),
            Route::new(Path::new("/data"), None, &two),
        ];
        let found = Route::find(&routes, Path::new("/data/Docs/a.txt"));
        assert_eq!(found.len(), 2);
        assert_eq!(found[0], (one, PathBuf::from("/data/Docs/a.txt")));
        assert_eq!(found[1], (two, PathBuf::from("/data/Docs/a.txt")));
    }
}
