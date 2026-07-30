//! `/containerLogs` — the endpoint `kubectl logs` talks to. Parses
//! containerd's CRI log file format (one line per write: `<RFC3339Nano
//! timestamp> <stdout|stderr> <F|P> <content>`, `P` meaning "line continues
//! in the next record" for writes that got split by a buffer boundary) back
//! into what the caller actually wrote, with kubectl's usual query knobs.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord<'a> {
    pub timestamp: &'a str,
    pub stream: Stream,
    pub partial: bool,
    pub content: &'a str,
}

/// Parse one line of a CRI-format log file. `None` for anything that
/// doesn't match the expected shape — malformed/truncated lines are
/// skipped rather than aborting the whole log read.
pub fn parse_log_line(line: &str) -> Option<LogRecord<'_>> {
    let mut parts = line.splitn(4, ' ');
    let timestamp = parts.next()?;
    let stream = match parts.next()? {
        "stdout" => Stream::Stdout,
        "stderr" => Stream::Stderr,
        _ => return None,
    };
    let tag = parts.next()?;
    let partial = match tag {
        "F" => false,
        "P" => true,
        _ => return None,
    };
    let content = parts.next().unwrap_or("");
    Some(LogRecord { timestamp, stream, partial, content })
}

#[derive(Clone, Debug, Default)]
pub struct LogOptions {
    /// Only keep the last N assembled (post partial-line-reassembly) lines.
    pub tail_lines: Option<usize>,
    /// Only keep lines whose timestamp is `>= since` (RFC3339Nano strings
    /// compare correctly lexicographically — containerd always writes UTC
    /// `Z`-suffixed timestamps, so no datetime parsing is needed).
    pub since: Option<String>,
    /// Prefix each output line with its timestamp (kubectl logs --timestamps).
    pub timestamps: bool,
}

/// Reassemble a CRI log file's raw lines into what `kubectl logs` shows:
/// strip the CRI prefix, stitch `P`-tagged (split) writes back into one
/// line, apply `since`/`tail_lines`/`timestamps`. Pure and allocation-light
/// on purpose — this runs over potentially large log files.
pub fn render_log_lines(raw_lines: &[&str], opts: &LogOptions) -> String {
    let mut assembled: Vec<(String, String)> = Vec::new(); // (timestamp, content)
    let mut pending: Option<(String, String)> = None; // (first fragment's timestamp, buffered content)

    for line in raw_lines {
        let Some(record) = parse_log_line(line) else { continue };
        if let Some(since) = &opts.since {
            if record.timestamp < since.as_str() {
                continue;
            }
        }
        match &mut pending {
            Some((_, buf)) => buf.push_str(record.content),
            None => pending = Some((record.timestamp.to_string(), record.content.to_string())),
        }
        if !record.partial {
            let (ts, content) = pending.take().unwrap();
            assembled.push((ts, content));
        }
    }
    // A trailing partial record with no closing F line yet (still being
    // written) — surface what's there so far rather than losing it.
    if let Some(entry) = pending.take() {
        assembled.push(entry);
    }

    let start = match opts.tail_lines {
        Some(n) if n < assembled.len() => assembled.len() - n,
        _ => 0,
    };

    let mut out = String::new();
    for (ts, content) in &assembled[start..] {
        if opts.timestamps {
            out.push_str(ts);
            out.push(' ');
        }
        out.push_str(content);
        out.push('\n');
    }
    out
}

/// Path to a container's previous (rotated) log, if `previous: true` was
/// requested — `rotate_log_file()` in `runtime/cri.rs` always rotates the
/// active log to `<path>.1`.
pub fn previous_log_path(log_path: &str) -> String {
    format!("{log_path}.1")
}

pub fn resolve_log_path(log_path: &str, previous: bool) -> String {
    if previous {
        previous_log_path(log_path)
    } else {
        log_path.to_string()
    }
}

pub fn log_file_exists(path: &str) -> bool {
    Path::new(path).is_file()
}

// ── HTTP handler ────────────────────────────────────────────────────────

use super::routes::{query_flag, query_value};
use super::{BoxedBody, ServerState};
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper::{Response, StatusCode};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt};
use tokio_stream::wrappers::ReceiverStream;

pub async fn handle_container_logs(
    state: &ServerState,
    namespace: &str,
    pod: &str,
    container: &str,
    query: &[(String, String)],
) -> Response<BoxedBody> {
    let log_path = match state.runtime.container_log_path(namespace, pod, container).await {
        Ok(Some(p)) => p,
        Ok(None) => return super::text_response(StatusCode::NOT_FOUND, "pod or container not found"),
        Err(e) => return super::text_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };

    let previous = query_flag(query, "previous");
    let follow = query_flag(query, "follow");
    let timestamps = query_flag(query, "timestamps");
    let tail_lines = query_value(query, "tailLines").and_then(|v| v.parse::<usize>().ok());
    let since = query_value(query, "sinceTime").map(|s| s.to_string());

    let resolved_path = resolve_log_path(&log_path, previous);
    if !log_file_exists(&resolved_path) {
        return super::text_response(
            StatusCode::NOT_FOUND,
            "log file not found (container may not have produced output yet, or has no rotated 'previous' log)",
        );
    }

    if follow {
        // Simplification: follow mode always streams from the start of the
        // file (then tails new writes) rather than honoring tailLines/since
        // first — real kubectl logs -f usually wants "last N lines, then
        // keep going", not "everything, then keep going". Good enough to
        // prove the streaming path works; a real gap for very large logs.
        return follow_stream_response(resolved_path, timestamps);
    }

    let content = match tokio::fs::read_to_string(&resolved_path).await {
        Ok(c) => c,
        Err(e) => return super::text_response(StatusCode::INTERNAL_SERVER_ERROR, format!("reading log file: {e}")),
    };
    let lines: Vec<&str> = content.lines().collect();
    let rendered = render_log_lines(&lines, &LogOptions { tail_lines, since, timestamps });
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from(rendered)).map_err(|never: std::convert::Infallible| match never {}).boxed())
        .unwrap()
}

/// Stream a log file: send what's there now, then poll for growth (no
/// inotify — matches the rest of nodelet's file-watching, e.g. probes.rs
/// and gc.rs, which all poll on a short interval rather than take an
/// inotify dependency for something checked every few hundred ms anyway).
fn follow_stream_response(path: String, timestamps: bool) -> Response<BoxedBody> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, super::BoxError>>(16);
    tokio::spawn(async move {
        let mut offset: u64 = 0;
        let mut pending: Option<(String, String)> = None; // (timestamp, buffered content)
        loop {
            let Ok(mut file) = tokio::fs::File::open(&path).await else {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            };
            if file.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
                break;
            }
            let mut reader = tokio::io::BufReader::new(&mut file);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) => break, // caught up; wait and re-poll
                    Ok(n) => {
                        offset += n as u64;
                        let trimmed = line.trim_end_matches('\n');
                        let Some(record) = parse_log_line(trimmed) else { continue };
                        match &mut pending {
                            Some((_, buf)) => buf.push_str(record.content),
                            None => pending = Some((record.timestamp.to_string(), record.content.to_string())),
                        }
                        if !record.partial {
                            let (ts, content) = pending.take().unwrap();
                            let mut out = String::new();
                            if timestamps {
                                out.push_str(&ts);
                                out.push(' ');
                            }
                            out.push_str(&content);
                            out.push('\n');
                            if tx.send(Ok(Frame::data(Bytes::from(out)))).await.is_err() {
                                return; // client disconnected
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });

    let stream = ReceiverStream::new(rx);
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/plain")
        .header("Transfer-Encoding", "chunked")
        .body(StreamBody::new(stream).boxed())
        .unwrap()
}

#[cfg(test)]
#[path = "logs_tests/parse_log_line.rs"]
mod tests_parse_log_line;
#[cfg(test)]
#[path = "logs_tests/render_log_lines.rs"]
mod tests_render_log_lines;
