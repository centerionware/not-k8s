//! Errors, and the exact gRPC statuses etcd returns for them.
//!
//! The message strings below are not descriptions — they are protocol. etcd's
//! Go client maps a status back to a typed error by comparing the message
//! against a fixed table (`rpctypes.errStringToError`), and kube-apiserver
//! then branches on the typed error: a compaction error is what makes the
//! watch cache re-list instead of giving up, and it is recognised *by that
//! exact string*. A friendlier message here would be silently downgraded to
//! "unknown error" and surface as an apiserver that stops watching.
//!
//! So: when adding a variant, copy the string from etcd's api/v3rpc/rpctypes
//! rather than writing one.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    /// The requested revision is older than the compaction point, so the
    /// history needed to answer is gone.
    #[error("required revision has been compacted (compact revision {compact_revision})")]
    Compacted { compact_revision: i64 },

    /// The requested revision hasn't happened yet.
    #[error("required revision {requested} is a future revision (current {current})")]
    FutureRevision { requested: i64, current: i64 },

    #[error("key not found")]
    KeyNotFound,

    /// A transaction requested something etcd forbids.
    #[error("{0}")]
    InvalidRequest(String),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The applier is gone — the store is shutting down, or a previous apply
    /// panicked and poisoned it.
    #[error("store is unavailable: {0}")]
    Unavailable(String),
}

impl From<Error> for tonic::Status {
    fn from(e: Error) -> tonic::Status {
        match e {
            // Code and string both come from etcd. `OutOfRange` is what its
            // client keys on together with the message.
            Error::Compacted { .. } => tonic::Status::new(
                tonic::Code::OutOfRange,
                "etcdserver: mvcc: required revision has been compacted",
            ),
            Error::FutureRevision { .. } => tonic::Status::new(
                tonic::Code::OutOfRange,
                "etcdserver: mvcc: required revision is a future revision",
            ),
            Error::KeyNotFound => {
                tonic::Status::new(tonic::Code::InvalidArgument, "etcdserver: key not found")
            }
            Error::InvalidRequest(msg) => tonic::Status::new(tonic::Code::InvalidArgument, msg),
            // Everything below is a genuine internal fault rather than a
            // protocol-level answer, so it gets our own wording — a client
            // retrying it is the correct behaviour either way.
            Error::Sqlite(e) => {
                tonic::Status::new(tonic::Code::Internal, format!("nodestore: storage error: {e}"))
            }
            Error::Io(e) => {
                tonic::Status::new(tonic::Code::Internal, format!("nodestore: io error: {e}"))
            }
            Error::Unavailable(msg) => {
                tonic::Status::new(tonic::Code::Unavailable, format!("nodestore: {msg}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // If these strings drift, apiserver's watch cache stops recognising a
    // compaction and the failure looks like "watches just stop working"
    // rather than anything pointing here.
    #[test]
    fn compaction_maps_to_etcds_exact_status() {
        let status: tonic::Status = Error::Compacted { compact_revision: 42 }.into();
        assert_eq!(status.code(), tonic::Code::OutOfRange);
        assert_eq!(status.message(), "etcdserver: mvcc: required revision has been compacted");
    }

    #[test]
    fn future_revision_maps_to_etcds_exact_status() {
        let status: tonic::Status = Error::FutureRevision { requested: 9, current: 3 }.into();
        assert_eq!(status.code(), tonic::Code::OutOfRange);
        assert_eq!(status.message(), "etcdserver: mvcc: required revision is a future revision");
    }

    #[test]
    fn key_not_found_maps_to_etcds_exact_status() {
        let status: tonic::Status = Error::KeyNotFound.into();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(status.message(), "etcdserver: key not found");
    }
}
