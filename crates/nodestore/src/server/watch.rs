//! The `Watch` service: one bidirectional stream carrying many watchers.
//!
//! # Why one task for the whole stream
//!
//! etcd multiplexes every watch a client creates onto a single gRPC stream,
//! each identified by a `watch_id`. The obvious implementation gives each
//! watcher its own task and its own subscription; this one runs a single task
//! per *stream* holding a single subscription, and fans out to the watchers on
//! it.
//!
//! That is not a micro-optimisation. kube-apiserver opens one watch per
//! resource type — dozens on a real cluster — and with a subscription each,
//! every applied command would be cloned into dozens of channels, then
//! discarded by all but the one or two watchers whose range actually matched.
//! One subscription per stream means one copy per stream, filtered once.
//!
//! # Not missing events
//!
//! apiserver's watch cache assumes a stream is gapless, and only re-lists when
//! explicitly told it cannot be trusted (a compaction error). Two hazards, one
//! answer — the store still holds the history, so a watcher that is unsure
//! re-reads from its own last delivered revision:
//!
//!   * **Start-up race.** Subscribe first, *then* replay history. Anything
//!     that lands during the replay is already buffered in the subscription
//!     and is filtered out by revision on the way past.
//!   * **Falling behind.** A slow consumer gets lagged off the bounded
//!     broadcast buffer. `Lagged` is not an error here, it is a signal: resync
//!     from the store and carry on.

use crate::command::KeyRange;
use crate::error::Error;
use crate::pb::etcdserverpb as pb;
use crate::server::{convert, EtcdApi};
use std::collections::HashMap;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

/// One live watcher on a stream.
struct Watcher {
    id: i64,
    range: KeyRange,
    prev_kv: bool,
    /// Highest revision delivered so far. The resync point after a lag, and
    /// the filter that makes the start-up race harmless.
    last_revision: i64,
    filter_put: bool,
    filter_delete: bool,
}

impl Watcher {
    fn wants(&self, event: &crate::store::Event) -> bool {
        if !self.range.contains(&event.kv.key) {
            return false;
        }
        match event.kind {
            crate::store::EventKind::Put => !self.filter_put,
            crate::store::EventKind::Delete => !self.filter_delete,
        }
    }
}

#[tonic::async_trait]
impl pb::watch_server::Watch for EtcdApi {
    type WatchStream = std::pin::Pin<
        Box<
            dyn tokio_stream::Stream<Item = std::result::Result<pb::WatchResponse, Status>>
                + Send
                + 'static,
        >,
    >;

    async fn watch(
        &self,
        request: Request<tonic::Streaming<pb::WatchRequest>>,
    ) -> std::result::Result<Response<Self::WatchStream>, Status> {
        let mut inbound = request.into_inner();
        let api = self.clone();
        // Subscribed here, before the task starts and before any replay, so
        // nothing applied between now and the first watcher's history read can
        // slip through the gap.
        let mut events = api.node().watch_hub().subscribe();
        let (tx, rx) = mpsc::channel::<std::result::Result<pb::WatchResponse, Status>>(64);

        tokio::spawn(async move {
            let mut watchers: HashMap<i64, Watcher> = HashMap::new();
            let mut next_watch_id: i64 = 0;
            // A client may close its *sending* half and keep receiving —
            // that is what a one-shot client does after issuing its create
            // request, and it is a legal gRPC half-close, not a hangup.
            // Treating it as the end of the stream would deliver the
            // "created" response and then silently nothing, which is
            // indistinguishable from a store that never sees any writes.
            let mut inbound_open = true;

            loop {
                tokio::select! {
                    // Client → server: create/cancel/progress.
                    incoming = inbound.message(), if inbound_open => {
                        match incoming {
                            Ok(Some(req)) => {
                                if !handle_request(&api, req, &mut watchers, &mut next_watch_id, &tx).await {
                                    break;
                                }
                            }
                            // Half-closed: stop reading, keep delivering.
                            Ok(None) => inbound_open = false,
                            // A broken connection is the end of it.
                            Err(_) => break,
                        }
                    }

                    // Store → client: applied events.
                    batch = events.recv() => {
                        match batch {
                            Ok(batch) => {
                                if !dispatch(&api, &batch, &mut watchers, &tx).await {
                                    break;
                                }
                            }
                            Err(RecvError::Lagged(missed)) => {
                                tracing::warn!(
                                    missed,
                                    "a watch stream fell behind the event buffer; resyncing it from the store"
                                );
                                if !resync(&api, &mut watchers, &tx).await {
                                    break;
                                }
                            }
                            // The hub is gone, which only happens on shutdown.
                            Err(RecvError::Closed) => break,
                        }
                    }
                }

                // Nothing left to read from and nothing left to send to:
                // without this the task would park forever on a select whose
                // only live branch can never fire again.
                if !inbound_open && watchers.is_empty() {
                    break;
                }
            }
        });

        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))))
    }
}

/// Handle one inbound request. Returns false when the stream should end.
async fn handle_request(
    api: &EtcdApi,
    req: pb::WatchRequest,
    watchers: &mut HashMap<i64, Watcher>,
    next_watch_id: &mut i64,
    tx: &mpsc::Sender<std::result::Result<pb::WatchResponse, Status>>,
) -> bool {
    match req.request_union {
        Some(pb::watch_request::RequestUnion::CreateRequest(create)) => {
            create_watch(api, create, watchers, next_watch_id, tx).await
        }
        Some(pb::watch_request::RequestUnion::CancelRequest(cancel)) => {
            watchers.remove(&cancel.watch_id);
            let revision = api.current_revision().unwrap_or(0);
            tx.send(Ok(pb::WatchResponse {
                header: api.header(revision),
                watch_id: cancel.watch_id,
                canceled: true,
                ..Default::default()
            }))
            .await
            .is_ok()
        }
        Some(pb::watch_request::RequestUnion::ProgressRequest(_)) => {
            // "Tell me where you are." apiserver uses this to advance its
            // bookmarks without waiting for real traffic — answering it is
            // what keeps an idle cluster's watch cache from looking stalled.
            // Reconcile first: the store revision can be ahead of the
            // broadcast receiver when this request wins the select, and
            // advertising that revision before replaying it would let the
            // client skip a real event. An explicit progress request covers
            // every watcher, so etcd uses -1 for the broadcast response.
            if !resync(api, watchers, tx).await {
                return false;
            }
            // Each position was established by the same locked read as its
            // replay. A fresh current_revision() here could include a write
            // made while sending that replay, falsely acknowledging it.
            let revision = watchers.values().map(|watcher| watcher.last_revision).min()
                .unwrap_or(0);
            tx.send(Ok(pb::WatchResponse {
                header: api.header(revision),
                watch_id: -1,
                ..Default::default()
            }))
            .await
            .is_ok()
        }
        None => true, // empty request; nothing to do
    }
}

async fn create_watch(
    api: &EtcdApi,
    create: pb::WatchCreateRequest,
    watchers: &mut HashMap<i64, Watcher>,
    next_watch_id: &mut i64,
    tx: &mpsc::Sender<std::result::Result<pb::WatchResponse, Status>>,
) -> bool {
    let watch_id = if create.watch_id != 0 {
        create.watch_id
    } else {
        let id = *next_watch_id;
        *next_watch_id += 1;
        id
    };

    let range = KeyRange::decode(create.key.clone(), create.range_end.clone());
    let filter_put = create
        .filters
        .contains(&(pb::watch_create_request::FilterType::Noput as i32));
    let filter_delete = create
        .filters
        .contains(&(pb::watch_create_request::FilterType::Nodelete as i32));

    let current = match api.current_revision() {
        Ok(r) => r,
        Err(e) => {
            return tx.send(Err(Status::from(e))).await.is_ok();
        }
    };
    // start_revision is inclusive, and 0 means "from now on".
    let start = if create.start_revision <= 0 { current } else { create.start_revision - 1 };

    // The created-response comes first, before any event, because the client
    // keys its bookkeeping off the watch_id in it.
    if tx
        .send(Ok(pb::WatchResponse {
            header: api.header(current),
            watch_id,
            created: true,
            ..Default::default()
        }))
        .await
        .is_err()
    {
        return false;
    }

    let mut watcher = Watcher {
        id: watch_id,
        range: range.clone(),
        prev_kv: create.prev_kv,
        last_revision: start,
        filter_put,
        filter_delete,
    };

    // Replay whatever the client has already missed. A compaction error here
    // is the one answer that must not be swallowed: it is how apiserver learns
    // to re-list instead of assuming it is up to date.
    if create.start_revision > 0 {
        match api.node().read(|s| s.events_since(start, &range)) {
            Ok(history) => {
                if !send_events(api, &mut watcher, history, tx).await {
                    return false;
                }
            }
            Err(Error::Compacted { compact_revision }) => {
                let _ = tx
                    .send(Ok(pb::WatchResponse {
                        header: api.header(current),
                        watch_id,
                        canceled: true,
                        compact_revision,
                        cancel_reason: "etcdserver: mvcc: required revision has been compacted"
                            .to_string(),
                        ..Default::default()
                    }))
                    .await;
                return true; // the stream survives; this watcher does not
            }
            Err(e) => {
                return tx.send(Err(Status::from(e))).await.is_ok();
            }
        }
    }

    watchers.insert(watch_id, watcher);
    true
}

/// Send a replayed batch, advancing the watcher's position as it goes.
async fn send_events(
    api: &EtcdApi,
    watcher: &mut Watcher,
    history: Vec<(i64, crate::store::Event)>,
    tx: &mpsc::Sender<std::result::Result<pb::WatchResponse, Status>>,
) -> bool {
    // Grouped by revision, because etcd delivers one response per revision and
    // a client is entitled to treat a response as an atomic set of changes.
    let mut current_revision = 0;
    let mut group: Vec<crate::pb::mvccpb::Event> = Vec::new();

    for (revision, event) in history {
        if !watcher.wants(&event) {
            watcher.last_revision = watcher.last_revision.max(revision);
            continue;
        }
        if revision != current_revision && !group.is_empty() {
            if !flush(api, watcher, current_revision, std::mem::take(&mut group), tx).await {
                return false;
            }
        }
        current_revision = revision;
        group.push(convert::event_to_pb(&event, watcher.prev_kv));
    }
    if !group.is_empty() && !flush(api, watcher, current_revision, group, tx).await {
        return false;
    }
    true
}

async fn flush(
    api: &EtcdApi,
    watcher: &mut Watcher,
    revision: i64,
    events: Vec<crate::pb::mvccpb::Event>,
    tx: &mpsc::Sender<std::result::Result<pb::WatchResponse, Status>>,
) -> bool {
    watcher.last_revision = watcher.last_revision.max(revision);
    tx.send(Ok(pb::WatchResponse {
        header: api.header(revision),
        watch_id: watcher.id,
        events,
        ..Default::default()
    }))
    .await
    .is_ok()
}

/// Deliver one applied command's events to every watcher that wants them.
async fn dispatch(
    api: &EtcdApi,
    batch: &crate::watch::WatchBatch,
    watchers: &mut HashMap<i64, Watcher>,
    tx: &mpsc::Sender<std::result::Result<pb::WatchResponse, Status>>,
) -> bool {
    for watcher in watchers.values_mut() {
        // Already delivered — the start-up race, closed here: a watcher whose
        // replay covered this revision skips it instead of seeing it twice.
        if batch.revision <= watcher.last_revision {
            continue;
        }
        let events: Vec<_> = batch
            .events
            .iter()
            .filter(|e| watcher.wants(e))
            .map(|e| convert::event_to_pb(e, watcher.prev_kv))
            .collect();

        // Advance regardless of whether anything matched: this watcher is
        // provably up to date as of this revision either way, and a resync
        // should not re-read history it has already decided it doesn't want.
        watcher.last_revision = batch.revision;
        if events.is_empty() {
            continue;
        }
        if !flush(api, watcher, batch.revision, events, tx).await {
            return false;
        }
    }
    true
}

/// Re-read from the store after a lag, per watcher, from its own position.
async fn resync(
    api: &EtcdApi,
    watchers: &mut HashMap<i64, Watcher>,
    tx: &mpsc::Sender<std::result::Result<pb::WatchResponse, Status>>,
) -> bool {
    let ids: Vec<i64> = watchers.keys().copied().collect();
    for id in ids {
        let Some(watcher) = watchers.get_mut(&id) else { continue };
        let from = watcher.last_revision;
        let range = watcher.range.clone();
        match api.node().read(|s| Ok((s.events_since(from, &range)?, s.revision()?))) {
            Ok((history, revision)) => {
                if !send_events(api, watcher, history, tx).await {
                    return false;
                }
                // Even a range with no matching writes is caught up to the
                // snapshot revision. Never sample a later revision after the
                // asynchronous sends above.
                watcher.last_revision = watcher.last_revision.max(revision);
            }
            Err(Error::Compacted { compact_revision }) => {
                // The events it missed have been compacted away. Cancelling
                // with a compaction reason is the contract: it tells the
                // client to re-list rather than continue with a hole.
                let revision = api.current_revision().unwrap_or(0);
                let sent = tx
                    .send(Ok(pb::WatchResponse {
                        header: api.header(revision),
                        watch_id: id,
                        canceled: true,
                        compact_revision,
                        cancel_reason: "etcdserver: mvcc: required revision has been compacted"
                            .to_string(),
                        ..Default::default()
                    }))
                    .await;
                watchers.remove(&id);
                if sent.is_err() {
                    return false;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, watch_id = id, "resync failed");
                return tx.send(Err(Status::from(e))).await.is_ok();
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, PutOp};
    use crate::consensus::{Node, SingleNode};
    use crate::store::Store;
    use std::future::Future;
    use std::sync::Arc;

    async fn put(node: &Node, key: &str) -> i64 {
        node.propose(Command::Put(PutOp {
            key: key.as_bytes().to_vec(), value: b"value".to_vec(),
            lease: 0, prev_kv: false, ignore_value: false, ignore_lease: false,
        })).await.unwrap().revision
    }

    #[tokio::test]
    async fn progress_does_not_acknowledge_a_write_during_replay() {
        let node = Node::new(Store::open(std::path::Path::new(":memory:")).unwrap(),
            Arc::new(SingleNode::new(1, 1)), 16);
        put(&node, "/first").await;
        let replay_revision = put(&node, "/second").await;
        let api = EtcdApi::new(node.clone());
        let mut watchers = HashMap::from([(0, Watcher {
            id: 0, range: KeyRange::All, prev_kv: false,
            last_revision: 1, filter_put: false, filter_delete: false,
        })]);
        // Zero free seats deliberately parks replay after its snapshot read.
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(Ok(pb::WatchResponse::default())).await.unwrap();
        let req = pb::WatchRequest {
            request_union: Some(pb::watch_request::RequestUnion::ProgressRequest(
                pb::WatchProgressRequest {})),
        };
        let mut next_id = 1;
        let progress = handle_request(&api, req, &mut watchers, &mut next_id, &tx);
        tokio::pin!(progress);
        std::future::poll_fn(|cx| {
            assert!(progress.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        }).await;
        let late_revision = put(&node, "/late").await;
        rx.recv().await.unwrap().unwrap(); // remove the deliberate blocker
        let drain = async {
            let mut delivered = Vec::new();
            loop {
                let response = rx.recv().await.unwrap().unwrap();
                if response.watch_id == -1 {
                    assert_eq!(response.header.unwrap().revision, replay_revision);
                    assert!(replay_revision < late_revision);
                    break;
                }
                delivered.extend(response.events);
            }
            assert_eq!(delivered.len(), 2);
        };
        let (ok, ()) = tokio::join!(progress, drain);
        assert!(ok);
    }
}
