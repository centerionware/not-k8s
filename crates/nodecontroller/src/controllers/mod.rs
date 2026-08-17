//! One module per controller — see docs/CONTROLLER_MANAGER.md for the full
//! group breakdown and the deliberately scoped pieces that remain deferred.

pub mod attach_detach;
pub mod cron_job;
pub mod csr;
pub mod daemon_set;
pub mod deployment;
pub mod disruption;
pub mod endpoint_slice;
pub mod garbage_collector;
pub mod job;
pub mod node_ipam;
pub mod node_lifecycle;
pub mod namespace;
pub mod pv_binder;
pub mod replica_set;
pub mod resource_claim;
pub mod resource_quota;
pub mod root_ca_publisher;
pub mod service_account;
pub mod stateful_set;
pub mod storage_protection;
pub mod ttl_after_finished;
