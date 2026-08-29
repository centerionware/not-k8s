//! Append-only audit event persistence.
//!
//! Kubernetes' audit log backend is a JSON-lines file selected by
//! `--audit-log-path`. `AuditSink` keeps that useful compatibility surface
//! without introducing a logging framework dependency: one process-wide
//! mutex serializes complete event lines, and the file is opened in append
//! mode so a restart never truncates an existing audit trail. Rotation,
//! batching, and webhook delivery are intentionally separate backends.

use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct AuditSink {
    file: Arc<Mutex<File>>,
}

impl AuditSink {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
        })
    }

    pub fn write(&self, event: &Value) -> io::Result<()> {
        let encoded = serde_json::to_vec(event).map_err(io::Error::other)?;
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        file.write_all(&encoded)?;
        file.write_all(b"\n")?;
        file.flush()
    }
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
}
