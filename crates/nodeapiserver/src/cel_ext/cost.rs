//! Real upstream's own `SizeEstimate`/`CostEstimate` arithmetic
//! (`google/cel-go`'s `checker/cost.go` + `common/cost/cost.go`, fetched
//! and read directly) — the min/max-range primitives Phase 3's own
//! static cost estimator (`docs/APISERVER.md`'s `cel_ext` section) is
//! built on. Both types are a `{min, max}` pair over `u64`, propagated
//! through a CEL expression's AST: a literal has an exact size (`min ==
//! max`), a schema-bounded field has the schema's own declared range, an
//! unresolvable one is `{0, u64::MAX}` (real upstream's own
//! `unknownSizeEstimate`) — genuinely unbounded, not a guess.
//!
//! **`u64::saturating_add`/`_mul` replace real upstream's own
//! hand-written `SafeAdd`/`SafeMultiply`** — checked directly against
//! `common/cost/cost.go`'s own doc comment ("saturating at
//! `math.MaxUint64` rather than wrapping"): Rust's standard library
//! saturating arithmetic already provides the exact same semantics, so
//! there's nothing to hand-port there. `safe_multiply_by_factor`/
//! `safe_ceil` are the one piece worth porting by hand — real upstream's
//! own float-to-uint64 overflow guard (`SafeMultiplyByFactor`), needed
//! because `f64 -> u64` conversion is real UB-adjacent territory in most
//! languages when the value is out of range, Rust's own `as` cast
//! saturates safely in *this* direction already (confirmed: Rust's
//! float-to-int `as` cast has been a saturating, defined operation since
//! Rust 1.45, not UB like C) — but real upstream's own bound check
//! (`xFloat > MaxUint64AsFloat/factor`) still matters here for the
//! *pre-cast* overflow case a plain `as u64` cast alone doesn't catch
//! (`f64` losing precision well before `u64::MAX`, silently rounding an
//! enormous-but-finite product down instead of saturating it up) — kept
//! for exact real-upstream-rounding fidelity, not because Rust would
//! panic or UB without it.

/// Real upstream's own base cost constants (`common/cost.go`), confirmed
/// directly — these are not tunable knobs, they're the literal values
/// every real Kubernetes cluster's own `x-kubernetes-validations` budget
/// is calibrated against.
pub const SELECT_AND_IDENT_COST: u64 = 1;
pub const CONST_COST: u64 = 0;
pub const LIST_CREATE_BASE_COST: u64 = 10;
pub const MAP_CREATE_BASE_COST: u64 = 30;
pub const STRUCT_CREATE_BASE_COST: u64 = 40;
pub const STRING_TRAVERSAL_COST_FACTOR: f64 = 0.1;
pub const REGEX_STRING_LENGTH_COST_FACTOR: f64 = 0.25;

/// Real upstream's own `SizeEstimate` — the estimated size of a
/// variable-length string/bytes/list/map value, in the same units CEL's
/// own `size()` function returns (unicode characters, bytes, or
/// entries).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SizeEstimate {
    pub min: u64,
    pub max: u64,
}

impl SizeEstimate {
    /// Real upstream's own `FixedSizeEstimate` — an exact, known size
    /// (a literal, or an inline list/map's own element count).
    pub fn fixed(size: u64) -> Self {
        Self { min: size, max: size }
    }

    /// Real upstream's own `UnknownSizeEstimate` — no bound at all.
    pub fn unknown() -> Self {
        Self { min: 0, max: u64::MAX }
    }

    pub fn add(self, other: Self) -> Self {
        Self { min: self.min.saturating_add(other.min), max: self.max.saturating_add(other.max) }
    }

    pub fn multiply(self, other: Self) -> Self {
        Self { min: self.min.saturating_mul(other.min), max: self.max.saturating_mul(other.max) }
    }

    /// Real upstream's own `Union` — the widest range covering both
    /// inputs, used whenever a node's real size could be either of two
    /// possibilities (an `if`/`?:`'s two branches, a comprehension's
    /// element sizes, ...).
    pub fn union(self, other: Self) -> Self {
        Self { min: self.min.min(other.min), max: self.max.max(other.max) }
    }

    /// Real upstream's own `MultiplyByCostFactor` — converts a size
    /// range into a cost range by multiplying by a real per-unit cost
    /// (e.g. [`STRING_TRAVERSAL_COST_FACTOR`] for "the cost of scanning
    /// this string once"), rounding each bound up to the nearest whole
    /// cost unit.
    pub fn multiply_by_cost_factor(self, cost_per_unit: f64) -> CostEstimate {
        CostEstimate { min: safe_multiply_by_factor(self.min, cost_per_unit), max: safe_multiply_by_factor(self.max, cost_per_unit) }
    }

    /// Real upstream's own `AsCost` — a size range treated directly as a
    /// cost range (a per-unit cost factor of exactly `1`).
    pub fn as_cost(self) -> CostEstimate {
        self.multiply_by_cost_factor(1.0)
    }

    /// Real upstream's own `MultiplyByCost` — this size times a real
    /// cost range (a comprehension's own element count times its loop
    /// body's own per-iteration cost).
    pub fn multiply_by_cost(self, cost: CostEstimate) -> CostEstimate {
        CostEstimate { min: self.min.saturating_mul(cost.min), max: self.max.saturating_mul(cost.max) }
    }
}

/// Real upstream's own `CostEstimate` — an estimated cost range in CEL's
/// own abstract cost units (real upstream's own comment: "+/- 50% of a
/// base cost unit which on an Intel xeon 2.20GHz CPU is 50ns").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CostEstimate {
    pub min: u64,
    pub max: u64,
}

impl CostEstimate {
    pub fn fixed(cost: u64) -> Self {
        Self { min: cost, max: cost }
    }

    pub fn unknown() -> Self {
        SizeEstimate::unknown().as_cost()
    }

    pub fn add(self, other: Self) -> Self {
        Self { min: self.min.saturating_add(other.min), max: self.max.saturating_add(other.max) }
    }

    pub fn multiply(self, other: Self) -> Self {
        Self { min: self.min.saturating_mul(other.min), max: self.max.saturating_mul(other.max) }
    }

    pub fn multiply_by_cost_factor(self, cost_per_unit: f64) -> Self {
        Self { min: safe_multiply_by_factor(self.min, cost_per_unit), max: safe_multiply_by_factor(self.max, cost_per_unit) }
    }

    pub fn union(self, other: Self) -> Self {
        Self { min: self.min.min(other.min), max: self.max.max(other.max) }
    }
}

/// Real upstream's own `SafeMultiplyByFactor` — see this module's own
/// doc comment for exactly why the pre-cast bound check still matters
/// even though Rust's own `f64 as u64` cast is already a defined,
/// saturating operation.
fn safe_multiply_by_factor(x: u64, factor: f64) -> u64 {
    let x_float = x as f64;
    if x_float > 0.0 && factor > 0.0 && x_float > (u64::MAX as f64) / factor {
        return u64::MAX;
    }
    safe_ceil(x_float * factor)
}

/// Real upstream's own `SafeCeil` — rounds up, saturating at
/// `u64::MAX`, flooring at zero for a negative or `NaN` input (real
/// upstream's own documented behavior, not a defensive addition).
fn safe_ceil(x: f64) -> u64 {
    if x.is_nan() || x <= 0.0 {
        return 0;
    }
    let ceil = x.ceil();
    if ceil >= u64::MAX as f64 {
        return u64::MAX;
    }
    ceil as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_has_equal_min_and_max() {
        let s = SizeEstimate::fixed(42);
        assert_eq!(s.min, 42);
        assert_eq!(s.max, 42);
    }

    #[test]
    fn unknown_size_is_zero_to_max() {
        let s = SizeEstimate::unknown();
        assert_eq!(s, SizeEstimate { min: 0, max: u64::MAX });
    }

    #[test]
    fn add_saturates_instead_of_overflowing() {
        let s = SizeEstimate::fixed(u64::MAX).add(SizeEstimate::fixed(10));
        assert_eq!(s, SizeEstimate::fixed(u64::MAX), "must saturate, not wrap");
    }

    #[test]
    fn multiply_saturates_instead_of_overflowing() {
        let s = SizeEstimate::fixed(u64::MAX).multiply(SizeEstimate::fixed(2));
        assert_eq!(s, SizeEstimate::fixed(u64::MAX));
    }

    #[test]
    fn union_widens_to_cover_both_ranges() {
        let a = SizeEstimate { min: 5, max: 10 };
        let b = SizeEstimate { min: 2, max: 20 };
        assert_eq!(a.union(b), SizeEstimate { min: 2, max: 20 });
    }

    #[test]
    fn multiply_by_cost_factor_rounds_up() {
        // 3 units * 0.1/unit = 0.3, rounds up to 1 -- real upstream's own
        // documented "nearest integer, rounded up" behavior.
        let c = SizeEstimate::fixed(3).multiply_by_cost_factor(STRING_TRAVERSAL_COST_FACTOR);
        assert_eq!(c, CostEstimate::fixed(1));
    }

    #[test]
    fn multiply_by_cost_factor_of_zero_is_free() {
        let c = SizeEstimate::fixed(1000).multiply_by_cost_factor(0.0);
        assert_eq!(c, CostEstimate::fixed(0));
    }

    #[test]
    fn multiply_by_cost_factor_saturates_on_a_huge_product() {
        let c = SizeEstimate::fixed(u64::MAX).multiply_by_cost_factor(2.0);
        assert_eq!(c, CostEstimate::fixed(u64::MAX));
    }

    #[test]
    fn as_cost_is_a_direct_one_to_one_mapping() {
        let s = SizeEstimate::fixed(7);
        assert_eq!(s.as_cost(), CostEstimate::fixed(7));
    }

    #[test]
    fn multiply_by_cost_scales_a_per_iteration_cost_by_element_count() {
        let range = SizeEstimate::fixed(100);
        let per_iteration = CostEstimate::fixed(3);
        assert_eq!(range.multiply_by_cost(per_iteration), CostEstimate::fixed(300));
    }

    #[test]
    fn cost_estimate_union_widens_to_cover_both_ranges() {
        let a = CostEstimate { min: 5, max: 10 };
        let b = CostEstimate { min: 1, max: 3 };
        assert_eq!(a.union(b), CostEstimate { min: 1, max: 10 });
    }

    #[test]
    fn unknown_cost_is_zero_to_max() {
        assert_eq!(CostEstimate::unknown(), CostEstimate { min: 0, max: u64::MAX });
    }
}
