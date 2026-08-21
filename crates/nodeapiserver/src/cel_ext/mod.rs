//! CEL cost estimation/budget, Kubernetes' extension libraries, and
//! type-checking against a structural schema.
//!
//! Status: **design pass done, no code yet** — see `docs/APISERVER.md`'s
//! own `cel_ext` section (right after Group K) for the real, verified
//! plan: crate choice (`cel-interpreter`), real upstream's own budget
//! numbers (`RuntimeCELCostBudget`/`PerCallLimit`/`CheckFrequency`, ...,
//! fetched directly from `k8s.io/apiserver/pkg/apis/cel/config.go` +
//! `pkg/cel/limits.go` — this is genuinely new territory for this
//! crate's own vendoring flow, which otherwise only pulls protos/
//! OpenAPI specs, not hand-written Go logic), the real two-layer
//! mechanism (static "checked cost" estimation at CRD-acceptance time,
//! separate from runtime cost accounting during real evaluation), and a
//! six-phase build-out plan. Blocks both Group J
//! (ValidatingAdmissionPolicy/MutatingAdmissionPolicy) and Group K
//! (`x-kubernetes-validations`) — an unbudgeted CEL evaluator in a real
//! request path is a real, unmitigated DoS surface, not hardening to add
//! later, so nothing wires this in before the cost-budget phases land.
//!
//! Named `cel_ext`, not `cel` — see the module-map note in `lib.rs` for why
//! (this crate also depends on the external `cel` crate).
