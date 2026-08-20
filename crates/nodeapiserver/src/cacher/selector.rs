//! Label and field selector parsing + matching — the last item on Group
//! D's not-yet-landed list. Pure, no cache/storage dependency at all:
//! callers hand this a label map (or a field-lookup closure) and get a
//! yes/no match, so it's exercisable well before Group F decides how
//! objects are actually decoded out of cached bytes.
//!
//! Both grammars are faithful ports of upstream's own parsers, verified
//! directly against the real source rather than assumed from memory:
//! - Label selectors:
//!   `staging/src/k8s.io/apimachinery/pkg/labels/selector.go` (`release-1.34`).
//!   The doc comment's own BNF omits `>`/`<` (GreaterThan/LessThan,
//!   numeric-only comparisons) — confirmed present in the real lexer's
//!   `string2token` table, so this port includes them too rather than
//!   trusting the doc comment over the code.
//! - Field selectors:
//!   `staging/src/k8s.io/apimachinery/pkg/fields/selector.go`. Much
//!   smaller grammar: comma-separated `key=value`/`key==value`/`key!=value`
//!   terms only — no `in`/`notin`/exists/gt/lt. Backslash-escaped commas
//!   inside a term are preserved as literal data, not treated as
//!   separators (upstream's `splitTerms`).

use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    Equals,
    NotEquals,
    In,
    NotIn,
    Exists,
    DoesNotExist,
    GreaterThan,
    LessThan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    pub key: String,
    pub operator: Operator,
    pub values: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("empty requirement in selector")]
    EmptyRequirement,
    #[error("unterminated value list — missing ')'")]
    UnterminatedValueList,
    #[error("could not parse requirement {0:?}")]
    Malformed(String),
}

/// Matches upstream's own `(*Requirement).Matches` exactly, including the
/// "absent key satisfies NotIn/NotEquals" and "non-integer value never
/// satisfies Gt/Lt" cases — both verified against the real source, not
/// assumed from the operator names alone.
pub fn matches_labels(requirements: &[Requirement], labels: &BTreeMap<String, String>) -> bool {
    requirements.iter().all(|r| matches_one(r, labels))
}

fn matches_one(r: &Requirement, labels: &BTreeMap<String, String>) -> bool {
    match r.operator {
        Operator::Equals | Operator::In => labels.get(&r.key).is_some_and(|v| r.values.iter().any(|rv| rv == v)),
        Operator::NotEquals | Operator::NotIn => match labels.get(&r.key) {
            None => true,
            Some(v) => !r.values.iter().any(|rv| rv == v),
        },
        Operator::Exists => labels.contains_key(&r.key),
        Operator::DoesNotExist => !labels.contains_key(&r.key),
        Operator::GreaterThan | Operator::LessThan => {
            let (Some(actual), Some(want)) = (
                labels.get(&r.key).and_then(|v| v.parse::<i64>().ok()),
                r.values.first().and_then(|v| v.parse::<i64>().ok()),
            ) else {
                return false;
            };
            if r.operator == Operator::GreaterThan { actual > want } else { actual < want }
        }
    }
}

/// `<selector-syntax> ::= <requirement> | <requirement> "," <selector-syntax>`
/// — top-level commas split requirements, except commas inside a
/// `(value-set)`, which belong to that requirement's value list.
pub fn parse_label_selector(input: &str) -> Result<Vec<Requirement>, ParseError> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(Vec::new());
    }
    split_top_level(input, ',')?.into_iter().map(|part| parse_label_requirement(part.trim())).collect()
}

fn parse_label_requirement(s: &str) -> Result<Requirement, ParseError> {
    if s.is_empty() {
        return Err(ParseError::EmptyRequirement);
    }
    if let Some(key) = s.strip_prefix('!') {
        return Ok(Requirement { key: key.trim().to_string(), operator: Operator::DoesNotExist, values: Vec::new() });
    }

    // Leftmost match wins, not a fixed keyword-priority scan over the
    // *whole* string — a naive "check ' in ' before '='" ordering
    // mis-parses `key=value in here` (an exact-match requirement whose
    // *value* happens to contain " in ") as a set-based `in` requirement,
    // because " in " would be found somewhere after the real "=" operator.
    // Scanning for the earliest-occurring operator, with the longer
    // operator winning ties at the same start position (so "==" beats "="
    // and "!=" is found before "=" would even overlap it), matches how a
    // real left-to-right lexer would tokenize this.
    let Some((idx, op_str, op)) = find_leftmost_operator(s) else {
        // Bare "KEY" — exists.
        return Ok(Requirement { key: s.to_string(), operator: Operator::Exists, values: Vec::new() });
    };

    let key = s[..idx].trim();
    if key.is_empty() {
        return Err(ParseError::Malformed(s.to_string()));
    }
    let rest = s[idx + op_str.len()..].trim();
    match op {
        Operator::In | Operator::NotIn => Ok(Requirement { key: key.to_string(), operator: op, values: parse_value_set(rest)? }),
        _ => Ok(Requirement { key: key.to_string(), operator: op, values: vec![rest.to_string()] }),
    }
}

/// The earliest-occurring recognized operator in `s`, preferring the
/// longer operator string when two candidates start at the same position
/// (`"=="`/`"!="` over a bare `"="`).
fn find_leftmost_operator(s: &str) -> Option<(usize, &'static str, Operator)> {
    const CANDIDATES: &[(&str, Operator)] = &[
        (" notin ", Operator::NotIn),
        (" in ", Operator::In),
        ("==", Operator::Equals),
        ("!=", Operator::NotEquals),
        ("=", Operator::Equals),
        (">", Operator::GreaterThan),
        ("<", Operator::LessThan),
    ];
    let mut best: Option<(usize, &'static str, Operator)> = None;
    for (op_str, op) in CANDIDATES {
        let Some(idx) = s.find(op_str) else { continue };
        best = match best {
            None => Some((idx, op_str, *op)),
            Some((best_idx, best_str, _)) if idx < best_idx || (idx == best_idx && op_str.len() > best_str.len()) => Some((idx, op_str, *op)),
            some => some,
        };
    }
    best
}

/// `"(" <values> ")"`, `<values> ::= VALUE | VALUE "," <values>`. The empty
/// string is a valid VALUE (`"z notin ()"`, `"x in (foo,,baz)"` from
/// upstream's own doc comment example).
fn parse_value_set(s: &str) -> Result<Vec<String>, ParseError> {
    let inner = s.strip_prefix('(').and_then(|s| s.strip_suffix(')')).ok_or(ParseError::UnterminatedValueList)?;
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    Ok(inner.split(',').map(|v| v.trim().to_string()).collect())
}

/// Splits `s` on every top-level occurrence of `sep` — one not enclosed in
/// `(...)`. Used for the label-selector grammar's requirement list; field
/// selectors use their own backslash-escaping split instead
/// (`split_field_terms`), a different rule for a different grammar.
fn split_top_level(s: &str, sep: char) -> Result<Vec<&str>, ParseError> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c == sep && depth == 0 => {
                parts.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(ParseError::UnterminatedValueList);
    }
    parts.push(&s[start..]);
    Ok(parts)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRequirement {
    pub field: String,
    pub value: String,
    pub negated: bool,
}

/// Field selectors only ever AND their terms, and `get_field` is the
/// caller's own way of reading `field`'s current value off whatever object
/// representation it holds — this module has no opinion on that
/// representation, matching `matches_labels`'s own decoupling.
pub fn matches_fields(requirements: &[FieldRequirement], get_field: impl Fn(&str) -> Option<String>) -> bool {
    requirements.iter().all(|r| {
        let actual = get_field(&r.field);
        let equal = actual.as_deref() == Some(r.value.as_str());
        if r.negated { !equal } else { equal }
    })
}

/// `key=value` / `key==value` / `key!=value`, comma-separated. Backslash-
/// escaped commas inside a term are data, not a separator boundary —
/// preserved with their leading backslash intact, exactly matching
/// upstream's own `splitTerms` (the caller of `termFromString` unescapes,
/// this module does not need to for its own matching purposes since the
/// escaped value never needs to equal an *unescaped* string).
pub fn parse_field_selector(input: &str) -> Result<Vec<FieldRequirement>, ParseError> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    split_field_terms(input).into_iter().map(parse_field_term).collect()
}

fn split_field_terms(s: &str) -> Vec<&str> {
    let mut terms = Vec::new();
    let mut start = 0;
    let mut in_slash = false;
    for (i, c) in s.char_indices() {
        if in_slash {
            in_slash = false;
        } else if c == '\\' {
            in_slash = true;
        } else if c == ',' {
            terms.push(&s[start..i]);
            start = i + 1;
        }
    }
    terms.push(&s[start..]);
    terms
}

fn parse_field_term(term: &str) -> Result<FieldRequirement, ParseError> {
    for (op_str, negated) in [("!=", true), ("==", false), ("=", false)] {
        if let Some(idx) = term.find(op_str) {
            let field = term[..idx].to_string();
            let value = term[idx + op_str.len()..].to_string();
            return Ok(FieldRequirement { field, value, negated });
        }
    }
    Err(ParseError::Malformed(term.to_string()))
}

// --- Wiring onto a decoded object (closes the "an actual LIST call over
// cached items" gap `docs/APISERVER.md` names for this module) ---
//
// `matches_labels`/`matches_fields` above are already decoupled from any
// particular object representation — deliberately, so this half didn't
// have to wait on Group F's decode path to exist. Group F's `codec`
// module now produces `serde_json::Value` for a decoded object, so this
// is the concrete adapter: extract a label map and answer field lookups
// from that shape specifically.
//
// The two halves are *not* symmetric, and that's a real, named
// asymmetry, not an oversight: every Kind's labels live at exactly
// `metadata.labels` (`ObjectMeta` is shared structurally by every type),
// so `object_labels` is genuinely generic. Field selectors are not — real
// upstream restricts which fields are even selectable per Kind via a
// hand-written `SelectableFields` function per type
// (`pkg/registry/*/*/strategy.go`, e.g. Pod adds `spec.nodeName`/
// `status.phase`/... on top of the universal `metadata.name`/
// `metadata.namespace`), and no such per-type allowlist is vendored or
// built here yet. `field_value` below is a generic dotted-JSON-path
// fallback (`"metadata.namespace"` -> pointer `/metadata/namespace`) —
// it will resolve *any* path that exists on the object, a strict
// superset of what real upstream allows for a given Kind, not a faithful
// per-type restriction. Good enough to make selector filtering actually
// work against real objects today; the per-type allowlist is separate,
// named, not-yet-started work, same shape as Group F's still-missing
// conversion.

/// Every Kind's labels live at `metadata.labels` — the one part of "get a
/// label map out of a decoded object" that's the same for every type.
/// Missing `metadata.labels`, a non-object value there, or a non-string
/// label value all degrade to "that label absent" rather than panicking.
pub fn object_labels(item: &Value) -> BTreeMap<String, String> {
    item.pointer("/metadata/labels")
        .and_then(Value::as_object)
        .map(|labels| labels.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
        .unwrap_or_default()
}

/// Reads a dot-separated field path (`"metadata.namespace"`,
/// `"spec.nodeName"`) off a decoded object as a string, via the
/// equivalent JSON pointer. `None` if the path doesn't exist or isn't a
/// scalar — a field selector can never match against an object/array
/// value, matching upstream's own field-selector grammar (string
/// equality only).
pub fn field_value(item: &Value, field: &str) -> Option<String> {
    let pointer = format!("/{}", field.replace('.', "/"));
    match item.pointer(&pointer) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// The real predicate a LIST call filters decoded cache items with: both
/// an empty label selector and an empty field selector mean "no
/// constraint from that half," matching upstream's own `Everything()`
/// selector semantics.
pub fn object_matches(item: &Value, label_reqs: &[Requirement], field_reqs: &[FieldRequirement]) -> bool {
    matches_labels(label_reqs, &object_labels(item)) && matches_fields(field_reqs, |f| field_value(item, f))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn equals_and_not_equals() {
        let reqs = parse_label_selector("app=web").unwrap();
        assert_eq!(reqs, vec![Requirement { key: "app".into(), operator: Operator::Equals, values: vec!["web".into()] }]);
        assert!(matches_labels(&reqs, &labels(&[("app", "web")])));
        assert!(!matches_labels(&reqs, &labels(&[("app", "db")])));
        assert!(!matches_labels(&reqs, &labels(&[])), "missing key must not satisfy Equals");

        let reqs = parse_label_selector("app==web").unwrap();
        assert_eq!(reqs[0].operator, Operator::Equals, "== is a synonym for =");

        let reqs = parse_label_selector("app!=web").unwrap();
        assert!(matches_labels(&reqs, &labels(&[("app", "db")])));
        assert!(matches_labels(&reqs, &labels(&[])), "missing key must satisfy NotEquals");
        assert!(!matches_labels(&reqs, &labels(&[("app", "web")])));
    }

    #[test]
    fn exists_and_does_not_exist() {
        let reqs = parse_label_selector("app").unwrap();
        assert_eq!(reqs[0].operator, Operator::Exists);
        assert!(matches_labels(&reqs, &labels(&[("app", "anything")])));
        assert!(!matches_labels(&reqs, &labels(&[])));

        let reqs = parse_label_selector("!app").unwrap();
        assert_eq!(reqs[0].operator, Operator::DoesNotExist);
        assert!(!matches_labels(&reqs, &labels(&[("app", "x")])));
        assert!(matches_labels(&reqs, &labels(&[])));
    }

    /// Upstream's own doc-comment example: `"x in (foo,,baz),y,z notin ()"`.
    #[test]
    fn upstreams_own_documented_example_parses_and_matches() {
        let reqs = parse_label_selector("x in (foo,,baz),y,z notin ()").unwrap();
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[0], Requirement { key: "x".into(), operator: Operator::In, values: vec!["foo".into(), "".into(), "baz".into()] });
        assert_eq!(reqs[1], Requirement { key: "y".into(), operator: Operator::Exists, values: vec![] });
        assert_eq!(reqs[2], Requirement { key: "z".into(), operator: Operator::NotIn, values: vec![] });

        // z notin () — empty value set means NotIn always holds (no value
        // in the set can ever match), so z is satisfied whether or not it
        // exists, as long as x and y are also satisfied.
        assert!(matches_labels(&reqs, &labels(&[("x", "foo"), ("y", "1")])));
        assert!(matches_labels(&reqs, &labels(&[("x", "baz"), ("y", "1"), ("z", "anything")])));
        assert!(!matches_labels(&reqs, &labels(&[("x", "nope"), ("y", "1")])));
    }

    #[test]
    fn in_and_notin_with_real_values() {
        let reqs = parse_label_selector("tier in (frontend, backend)").unwrap();
        assert!(matches_labels(&reqs, &labels(&[("tier", "frontend")])));
        assert!(matches_labels(&reqs, &labels(&[("tier", "backend")])));
        assert!(!matches_labels(&reqs, &labels(&[("tier", "db")])));
        assert!(!matches_labels(&reqs, &labels(&[])));

        let reqs = parse_label_selector("tier notin (frontend, backend)").unwrap();
        assert!(!matches_labels(&reqs, &labels(&[("tier", "frontend")])));
        assert!(matches_labels(&reqs, &labels(&[("tier", "db")])));
        assert!(matches_labels(&reqs, &labels(&[])), "missing key must satisfy NotIn");
    }

    #[test]
    fn greater_than_and_less_than_are_numeric_only() {
        let reqs = parse_label_selector("priority>3").unwrap();
        assert!(matches_labels(&reqs, &labels(&[("priority", "5")])));
        assert!(!matches_labels(&reqs, &labels(&[("priority", "2")])));
        assert!(!matches_labels(&reqs, &labels(&[("priority", "not-a-number")])), "a non-integer value never satisfies Gt");
        assert!(!matches_labels(&reqs, &labels(&[])));

        let reqs = parse_label_selector("priority<3").unwrap();
        assert!(matches_labels(&reqs, &labels(&[("priority", "1")])));
        assert!(!matches_labels(&reqs, &labels(&[("priority", "5")])));
    }

    /// A naive "check the ' in ' keyword before checking '='" ordering
    /// mis-parses this as a set-based `in` requirement, because " in "
    /// occurs somewhere in the *value* after the real "=" operator.
    /// Leftmost-match is what makes this parse as exact-match instead.
    #[test]
    fn a_value_containing_the_in_keyword_does_not_confuse_the_operator_search() {
        let reqs = parse_label_selector("description=value in here").unwrap();
        assert_eq!(reqs, vec![Requirement { key: "description".into(), operator: Operator::Equals, values: vec!["value in here".into()] }]);
    }

    #[test]
    fn multiple_comma_separated_requirements_are_anded() {
        let reqs = parse_label_selector("app=web,tier=frontend").unwrap();
        assert_eq!(reqs.len(), 2);
        assert!(matches_labels(&reqs, &labels(&[("app", "web"), ("tier", "frontend")])));
        assert!(!matches_labels(&reqs, &labels(&[("app", "web")])), "missing the second requirement must fail the AND");
    }

    #[test]
    fn an_empty_selector_matches_everything() {
        let reqs = parse_label_selector("").unwrap();
        assert!(reqs.is_empty());
        assert!(matches_labels(&reqs, &labels(&[])));
        assert!(matches_labels(&reqs, &labels(&[("any", "thing")])));
    }

    #[test]
    fn an_unterminated_value_list_is_a_parse_error() {
        assert_eq!(parse_label_selector("x in (foo,bar"), Err(ParseError::UnterminatedValueList));
    }

    #[test]
    fn field_selector_equals_and_not_equals() {
        let reqs = parse_field_selector("metadata.name=my-pod,status.phase!=Running").unwrap();
        assert_eq!(reqs.len(), 2);
        assert!(!reqs[0].negated);
        assert!(reqs[1].negated);

        let get = |field: &str| match field {
            "metadata.name" => Some("my-pod".to_string()),
            "status.phase" => Some("Pending".to_string()),
            _ => None,
        };
        assert!(matches_fields(&reqs, get));

        let get_running = |field: &str| match field {
            "metadata.name" => Some("my-pod".to_string()),
            "status.phase" => Some("Running".to_string()),
            _ => None,
        };
        assert!(!matches_fields(&reqs, get_running));
    }

    #[test]
    fn field_selector_double_equals_is_a_synonym_for_equals() {
        let reqs = parse_field_selector("metadata.name==my-pod").unwrap();
        assert!(!reqs[0].negated);
        assert_eq!(reqs[0].value, "my-pod");
    }

    #[test]
    fn field_selector_preserves_backslash_escaped_commas_as_literal_data() {
        // Matches upstream's own splitTerms doc comment precisely: the
        // leading backslash is kept, not stripped, in the returned term.
        let terms = split_field_terms(r"a=b\,c,d=e");
        assert_eq!(terms, vec![r"a=b\,c", "d=e"]);
    }

    fn pod(name: &str, ns: &str, node: &str, labels: serde_json::Value) -> Value {
        serde_json::json!({
            "metadata": {"name": name, "namespace": ns, "labels": labels},
            "spec": {"nodeName": node},
        })
    }

    #[test]
    fn object_labels_reads_metadata_labels() {
        let item = pod("web-1", "default", "node-a", serde_json::json!({"app": "web", "tier": "frontend"}));
        assert_eq!(object_labels(&item), labels(&[("app", "web"), ("tier", "frontend")]));
    }

    #[test]
    fn object_labels_defaults_to_empty_when_absent_or_malformed() {
        assert_eq!(object_labels(&serde_json::json!({"metadata": {}})), BTreeMap::new());
        assert_eq!(object_labels(&serde_json::json!({})), BTreeMap::new());
        // A non-object "labels" value (malformed data) degrades to empty
        // rather than panicking.
        assert_eq!(object_labels(&serde_json::json!({"metadata": {"labels": "not-an-object"}})), BTreeMap::new());
    }

    #[test]
    fn field_value_reads_a_dotted_path_via_json_pointer() {
        let item = pod("web-1", "default", "node-a", serde_json::json!({}));
        assert_eq!(field_value(&item, "metadata.name"), Some("web-1".to_string()));
        assert_eq!(field_value(&item, "metadata.namespace"), Some("default".to_string()));
        assert_eq!(field_value(&item, "spec.nodeName"), Some("node-a".to_string()));
        assert_eq!(field_value(&item, "metadata.missing"), None);
    }

    #[test]
    fn field_value_is_none_for_a_non_scalar() {
        let item = pod("web-1", "default", "node-a", serde_json::json!({"app": "web"}));
        // "metadata.labels" resolves to a JSON object, not a scalar — a
        // field selector can never match against it.
        assert_eq!(field_value(&item, "metadata.labels"), None);
    }

    #[test]
    fn object_matches_ands_the_label_and_field_halves() {
        let item = pod("web-1", "default", "node-a", serde_json::json!({"app": "web"}));

        let label_reqs = parse_label_selector("app=web").unwrap();
        let field_reqs = parse_field_selector("spec.nodeName=node-a").unwrap();
        assert!(object_matches(&item, &label_reqs, &field_reqs));

        let wrong_field = parse_field_selector("spec.nodeName=node-b").unwrap();
        assert!(!object_matches(&item, &label_reqs, &wrong_field), "a field mismatch must fail the whole match");

        let wrong_label = parse_label_selector("app=db").unwrap();
        assert!(!object_matches(&item, &wrong_label, &field_reqs), "a label mismatch must fail the whole match");
    }

    #[test]
    fn object_matches_with_no_selectors_at_all_matches_everything() {
        let item = pod("web-1", "default", "node-a", serde_json::json!({}));
        assert!(object_matches(&item, &[], &[]));
    }
}
