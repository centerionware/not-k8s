//! The subset of real upstream's `admission.Attributes`
//! (`staging/src/k8s.io/apiserver/pkg/admission/interfaces.go`, fetched and
//! read directly) this crate actually needs to make a decision: which
//! operation, on what `(group, resource, subresource)`, in which namespace,
//! under what name. Real upstream's `Attributes` also carries the
//! submitted/old `runtime.Object`, `user.Info`, dry-run, and an annotation
//! sink for audit — none of those are needed by the one plugin this module
//! has today ([`crate::admission::namespace_lifecycle`]); add them when a
//! plugin that actually reads them lands, same "don't build ahead of a real
//! need" posture `authz::rbac::RequestAttributes` already takes.

/// Real upstream's `admission.Operation` constants — `Connect` (used for
/// subresource proxy operations this crate doesn't serve yet) is omitted
/// for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Create,
    Update,
    Delete,
}

#[derive(Debug, Clone, Copy)]
pub struct Attributes<'a> {
    pub operation: Operation,
    pub group: &'a str,
    pub resource: &'a str,
    pub namespace: &'a str,
    pub name: &'a str,
}
