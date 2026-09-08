//! Kubernetes' `kubernetes.regex` CEL extension library.
//!
//! This is the runtime surface of upstream's regex library: `find` returns
//! the first matching substring and `findAll` returns every matching
//! substring, optionally capped by a non-negative limit.

use cel::extractors::{Arguments, This};
use cel::{ExecutionError, FunctionContext, Value};
use regex::Regex;
use std::sync::Arc;

fn compile_regex(ftx: &FunctionContext, pattern: &str) -> Result<Regex, ExecutionError> {
    Regex::new(pattern).map_err(|error| ftx.error(format!("Illegal regex: {error}")))
}

/// Return the first substring matching `pattern`, or the empty string when
/// the pattern has no match.
pub fn find_binding(
    ftx: &FunctionContext,
    This(text): This<Arc<String>>,
    pattern: Arc<String>,
) -> Result<Value, ExecutionError> {
    let regex = compile_regex(ftx, &pattern)?;
    Ok(Value::String(Arc::new(
        regex.find(&text).map_or_else(String::new, |m| m.as_str().to_string()),
    )))
}

/// Return all matching substrings. The optional integer is the upstream
/// `findAll` limit: negative means unlimited, zero means no results, and a
/// positive value caps the number of returned matches.
pub fn find_all_binding(
    ftx: &FunctionContext,
    This(text): This<Arc<String>>,
    Arguments(arguments): Arguments,
) -> Result<Value, ExecutionError> {
    let (pattern, limit) = match arguments.as_slice() {
        [Value::String(pattern)] => (pattern.as_ref(), None),
        [Value::String(pattern), Value::Int(limit)] => (pattern.as_ref(), Some(*limit)),
        _ => {
            return Err(ftx.error(
                "findAll() requires a string pattern and an optional integer limit",
            ));
        }
    };
    let regex = compile_regex(ftx, pattern)?;
    let matches = regex
        .find_iter(&text)
        .take(limit.filter(|limit| *limit >= 0).unwrap_or(i64::MAX) as usize)
        .map(|m| Value::String(Arc::new(m.as_str().to_string())))
        .collect();
    Ok(Value::List(Arc::new(matches)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_returns_the_first_match_or_an_empty_string() {
        let regex = Regex::new(r"[0-9]+").unwrap();
        assert_eq!(regex.find("abc 123 def 456").unwrap().as_str(), "123");
        assert!(Regex::new("xyz").unwrap().find("abc").is_none());
    }

    #[test]
    fn find_all_limit_matches_upstream_shape() {
        let regex = Regex::new(r"[0-9]+").unwrap();
        let matches: Vec<_> = regex.find_iter("123 abc 456").map(|m| m.as_str()).collect();
        assert_eq!(matches, vec!["123", "456"]);
        assert_eq!(matches.into_iter().take(1).collect::<Vec<_>>(), vec!["123"]);
    }
}
