//! Append-only audit event persistence.
//!
//! Kubernetes' audit log backend is a JSON-lines file selected by
//! `--audit-log-path`. `AuditSink` keeps that useful compatibility surface
//! without introducing a logging framework dependency: one process-wide
//! mutex serializes complete event lines, and the file is opened in append
//! mode so a restart never truncates an existing audit trail. Optional
//! size-based rotation keeps a bounded set of numbered backups, matching the
//! useful file-backend part of kube-apiserver's audit configuration.

use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct AuditSink {
    state: Arc<Mutex<State>>,
}

struct State {
    file: Option<File>,
    path: PathBuf,
    max_size_bytes: Option<u64>,
    max_backups: usize,
}

impl AuditSink {
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_with_rotation(path, None, 0)
    }

    pub fn open_with_rotation(
        path: &Path,
        max_size_bytes: Option<u64>,
        max_backups: usize,
    ) -> io::Result<Self> {
        if max_size_bytes.is_some() && max_backups == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "audit log rotation requires at least one backup",
            ));
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                file: Some(file),
                path: path.to_path_buf(),
                max_size_bytes,
                max_backups,
            })),
        })
    }

    pub fn write(&self, event: &Value) -> io::Result<()> {
        let encoded = serde_json::to_vec(event).map_err(io::Error::other)?;
        let mut file = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let line_len = encoded.len() as u64 + 1;
        let current_len = file
            .file
            .as_ref()
            .expect("audit sink file is always open")
            .metadata()?
            .len();
        if file
            .max_size_bytes
            .is_some_and(|limit| current_len > 0 && current_len.saturating_add(line_len) > limit)
        {
            rotate(&mut file)?;
        }
        let output = file.file.as_mut().expect("audit sink file is always open");
        output.write_all(&encoded)?;
        output.write_all(b"\n")?;
        output.flush()
    }
}

fn rotate(state: &mut State) -> io::Result<()> {
    let max_backups = state.max_backups;
    let path = state.path.clone();
    let old_file = state.file.take().expect("audit sink file is always open");
    if let Err(error) = old_file.sync_data() {
        drop(old_file);
        state.file = Some(OpenOptions::new().create(true).append(true).open(&path)?);
        return Err(error);
    }
    drop(old_file);

    let result = (|| {
        for index in (1..max_backups).rev() {
            let from = backup_path(&path, index);
            let to = backup_path(&path, index + 1);
            match std::fs::rename(&from, &to) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        std::fs::rename(&path, backup_path(&path, 1))
    })();

    let reopen = OpenOptions::new().create(true).append(true).open(&path);
    state.file = Some(reopen?);
    result
}

fn backup_path(path: &Path, index: usize) -> PathBuf {
    PathBuf::from(format!("{}.{index}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn appends_complete_json_lines() {
        let path =
            std::env::temp_dir().join(format!("nodeapiserver-audit-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let sink = AuditSink::open(&path).unwrap();
        sink.write(&json!({"requestURI": "/version"})).unwrap();
        sink.write(&json!({"requestURI": "/readyz"})).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            lines,
            vec![
                json!({"requestURI": "/version"}),
                json!({"requestURI": "/readyz"})
            ]
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rotates_before_a_line_would_exceed_the_configured_size() {
        let path = std::env::temp_dir().join(format!(
            "nodeapiserver-audit-rotation-{}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(backup_path(&path, 1));
        let _ = std::fs::remove_file(backup_path(&path, 2));

        let sink = AuditSink::open_with_rotation(&path, Some(16), 2).unwrap();
        sink.write(&json!({"n": 1})).unwrap();
        sink.write(&json!({"n": 2})).unwrap();
        sink.write(&json!({"n": 3})).unwrap();
        sink.write(&json!({"n": 4})).unwrap();
        sink.write(&json!({"n": 5})).unwrap();
        drop(sink);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"n\":5}\n");
        assert_eq!(
            std::fs::read_to_string(backup_path(&path, 1)).unwrap(),
            "{\"n\":3}\n{\"n\":4}\n"
        );
        assert_eq!(
            std::fs::read_to_string(backup_path(&path, 2)).unwrap(),
            "{\"n\":1}\n{\"n\":2}\n"
        );

        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(backup_path(&path, 1)).unwrap();
        std::fs::remove_file(backup_path(&path, 2)).unwrap();
    }
}
