//! Shared state-file helpers.
//!
//! `shdeps` state remains intentionally human-readable, but writes should not
//! be human-fragile. Centralizing atomic replacement here keeps manifest,
//! stamp, link-state, and future cache writers from each inventing slightly
//! different temp-file behavior.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Result;

/// Replaces a state file with `content` using a same-directory temp file.
pub fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let temp = temp_path(path);
    let write_result = (|| -> Result<()> {
        // Keep the temp file beside the destination so rename stays within one
        // filesystem. Readers then observe either the old complete file or the
        // new complete file, never a partial write from a crashed update.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result
}

fn temp_path(path: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");

    path.with_file_name(format!(".{name}.tmp.{}.{stamp}", std::process::id()))
}
