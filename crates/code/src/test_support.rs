use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Workspace;

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(1);

/// A temporary workspace root that is removed even when a test panics.
pub(crate) struct TempWorkspace {
    root: PathBuf,
}

impl TempWorkspace {
    pub(crate) fn new(name: &str) -> Self {
        let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("nuillu-code-{name}-{}-{id}", std::process::id()));
        fs::create_dir_all(&root).expect("create temporary workspace root");
        Self { root }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        let parent = path.parent().expect("fixture path has a parent");
        fs::create_dir_all(parent).expect("create fixture directory");
        fs::write(path, contents).expect("write fixture file");
    }

    pub(crate) fn read(&self, relative: &str) -> Vec<u8> {
        fs::read(self.root.join(relative)).expect("read fixture file")
    }

    pub(crate) fn open(&self) -> Workspace {
        Workspace::open(&self.root).expect("open temporary workspace")
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
