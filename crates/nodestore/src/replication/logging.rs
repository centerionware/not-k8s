//! A slog drain that forwards raft-rs's log records into tracing.
//!
//! raft-rs logs through slog; everything else here logs through tracing. The
//! easy options are both bad: `slog::Discard` throws away exactly the messages
//! that explain a failover ("became leader at term 4", "received vote
//! rejection"), and raft's own `default-logger` feature pulls in a second
//! logging stack that writes somewhere else entirely.
//!
//! So: a ~40-line drain. Levels are mapped down one step from raft's own
//! judgement — raft logs routine elections at INFO, and on a cluster doing
//! nothing else that would be the loudest thing in the log — except for
//! warnings and errors, which pass through unchanged.

use slog::{Drain, Level, OwnedKVList, Record};

pub struct TracingDrain;

impl Drain for TracingDrain {
    type Ok = ();
    type Err = slog::Never;

    fn log(&self, record: &Record<'_>, _values: &OwnedKVList) -> Result<Self::Ok, Self::Err> {
        // The message only: slog's structured values would need a serializer
        // to extract, and raft's messages are self-describing sentences
        // ("became follower at term 3") that read fine on their own.
        let msg = record.msg().to_string();
        match record.level() {
            Level::Critical | Level::Error => tracing::error!(target: "raft", "{msg}"),
            Level::Warning => tracing::warn!(target: "raft", "{msg}"),
            Level::Info => tracing::debug!(target: "raft", "{msg}"),
            Level::Debug | Level::Trace => tracing::trace!(target: "raft", "{msg}"),
        }
        Ok(())
    }
}

/// A logger for `raft::RawNode`.
pub fn raft_logger() -> slog::Logger {
    slog::Logger::root(TracingDrain.fuse(), slog::o!())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_logger_accepts_records_without_panicking() {
        // The drain runs inside raft's own loop; a panic there would take
        // consensus down over a log line.
        let logger = raft_logger();
        slog::info!(logger, "became leader at term {}", 4);
        slog::error!(logger, "storage unavailable");
        slog::debug!(logger, "sending append to {}", 2);
    }
}
