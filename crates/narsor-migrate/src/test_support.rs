//! A temporary directory for the unit tests, deleted on drop.

use std::fs;
use std::path::PathBuf;

/// A new, empty, canonical temporary directory.
pub struct Temp(pub PathBuf);

impl Temp {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("narsor-migrate-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        Self(fs::canonicalize(&dir).unwrap())
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
