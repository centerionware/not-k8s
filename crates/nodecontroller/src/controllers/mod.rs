//! One module per controller — see docs/CONTROLLER_MANAGER.md for the full
//! group breakdown. Only Group A (node lifecycle) is implemented so far;
//! each future group is its own module here, its own PR, per the plan's
//! delivery order (A, then B/C, then D, then E/F, then G, then H/I/J).

pub mod endpoint_slice;
pub mod node_ipam;
pub mod node_lifecycle;
pub mod resource_quota;
pub mod service_account;
