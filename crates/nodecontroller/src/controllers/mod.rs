//! One module per controller — see docs/CONTROLLER_MANAGER.md for the full
//! group breakdown. Only Group A (node lifecycle) is implemented so far;
//! each future group is its own module here, its own PR, per the plan's
//! delivery order (A, then B/C, then D, then E/F, then G, then H/I/J).

pub mod attach_detach;
pub mod cron_job;
pub mod daemon_set;
pub mod deployment;
pub mod endpoint_slice;
pub mod garbage_collector;
pub mod job;
pub mod node_ipam;
pub mod node_lifecycle;
pub mod pv_binder;
pub mod replica_set;
pub mod resource_quota;
pub mod service_account;
pub mod stateful_set;
pub mod storage_protection;
pub mod ttl_after_finished;
