//! Content negotiation: `Accept`/`Content-Type` header parsing against the
//! three formats this crate speaks. Pure string parsing — no `hyper`/`http`
//! dependency yet (that's Group C/E), so this is testable well before there
//! is a real listener to drive it.
//!
//! Kubernetes' `Accept` header can carry more than a bare media type — the
//! `as=<Kind>;g=<group>;v=<version>` parameters (`kubectl get`'s own
//! `as=Table;g=meta.k8s.io;v=v1` for server-side printing is the most
//! common case, but real clients also send other `as=` kinds — aggregated
//! discovery's `as=APIGroupDiscoveryList;v=v2;g=apidiscovery.k8s.io`, for
//! one) are part of the same header this module parses, captured
//! generically (`Accepted::as_kind`/`as_group`/`as_version`) rather than
//! special-cased to Table alone, so `negotiate()` returns them alongside
//! the chosen format rather than making a second pass over the header
//! later.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Yaml,
    Protobuf,
}

impl Format {
    fn from_media_type(media_type: &str) -> Option<Format> {
        match media_type {
            "application/json" => Some(Format::Json),
            "application/yaml" => Some(Format::Yaml),
            "application/vnd.kubernetes.protobuf" => Some(Format::Protobuf),
            _ => None,
        }
    }

    /// The canonical `Content-Type` value the apiserver writes in a
    /// response using this format.
    pub fn media_type(self) -> &'static str {
        match self {
            Format::Json => "application/json",
            Format::Yaml => "application/yaml",
            Format::Protobuf => "application/vnd.kubernetes.protobuf",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub format: Format,
    /// The raw `as=` value, case-preserved exactly as the client sent it —
    /// real upstream compares this against literal Go string constants
    /// (`"Table"`, `"APIGroupDiscoveryList"`, `"PartialObjectMetadata"`,
    /// `"PartialObjectMetadataList"`), case-sensitively, so this module
    /// doesn't normalize case either. `None` means the client didn't ask
    /// for an alternate representation at all — just `format` applies.
    pub as_kind: Option<String>,
    /// `g`/`v` — which group/version of `as_kind`'s type the client
    /// understands (e.g. `meta.k8s.io`/`v1` for `as=Table`,
    /// `apidiscovery.k8s.io`/`v2` for `as=APIGroupDiscoveryList`).
    pub as_group: Option<String>,
    pub as_version: Option<String>,
}

impl Accepted {
    /// `true` for `kubectl get`'s own server-side-printing request
    /// (`as=Table`) — the one `as=` kind this crate can currently act on
    /// (`codec::table::convert_to_table`); every other `as_kind` value is
    /// parsed but not yet something a caller can convert an object into.
    pub fn wants_table(&self) -> bool {
        self.as_kind.as_deref() == Some("Table")
    }
}

/// Parses one `Accept` header value (comma-separated media ranges, each
/// optionally carrying `;q=` and other parameters) and returns the
/// highest-priority range this crate actually supports. Ties (equal `q`)
/// keep the client's own encounter order, matching real HTTP content
/// negotiation (RFC 9110 §12.5.1) rather than picking an arbitrary one.
///
/// `None` means nothing in the header is a format this crate speaks — the
/// caller's job is turning that into a `406 Not Acceptable`.
pub fn negotiate(accept_header: &str) -> Option<Accepted> {
    let mut best: Option<(f32, usize, Accepted)> = None;
    for (index, range) in accept_header.split(',').enumerate() {
        let mut parts = range.split(';').map(str::trim);
        let media_type = parts.next().unwrap_or("").to_ascii_lowercase();
        // "*/*" and "application/*" are not resolved to a concrete format —
        // this crate has no notion of a "default" here; the caller decides
        // what to do with a negotiation that found nothing concrete.
        let Some(format) = Format::from_media_type(&media_type) else { continue };

        let mut q: f32 = 1.0;
        let mut as_kind = None;
        let mut as_group = None;
        let mut as_version = None;
        for param in parts {
            let Some((key, value)) = param.split_once('=') else { continue };
            match key.trim() {
                "q" => q = value.trim().parse().unwrap_or(1.0),
                "as" => as_kind = Some(value.trim().to_string()),
                "g" => as_group = Some(value.trim().to_string()),
                "v" => as_version = Some(value.trim().to_string()),
                _ => {}
            }
        }

        let candidate = Accepted { format, as_kind, as_group, as_version };
        let better = match &best {
            None => true,
            Some((best_q, best_index, _)) => q > *best_q || (q == *best_q && index < *best_index),
        };
        if better {
            best = Some((q, index, candidate));
        }
    }
    best.map(|(_, _, accepted)| accepted)
}

/// Parses a `Content-Type` header (the request body's own format — no
/// `q`-values, no `as=Table`, that parameter set only ever appears on
/// `Accept`) into a [`Format`]. Ignores a trailing `; charset=...` or
/// similar parameter, same as `negotiate()` does.
pub fn content_type(header: &str) -> Option<Format> {
    let media_type = header.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    Format::from_media_type(&media_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_media_type_negotiates_to_its_format() {
        assert_eq!(negotiate("application/json").unwrap().format, Format::Json);
        assert_eq!(negotiate("application/yaml").unwrap().format, Format::Yaml);
        assert_eq!(negotiate("application/vnd.kubernetes.protobuf").unwrap().format, Format::Protobuf);
    }

    #[test]
    fn an_unsupported_media_type_alone_negotiates_to_nothing() {
        assert!(negotiate("application/xml").is_none());
        assert!(negotiate("*/*").is_none());
    }

    #[test]
    fn higher_q_value_wins_regardless_of_order() {
        let accepted = negotiate("application/json;q=0.5, application/yaml;q=0.9").unwrap();
        assert_eq!(accepted.format, Format::Yaml);
    }

    #[test]
    fn equal_q_values_keep_the_clients_own_order() {
        let accepted = negotiate("application/yaml;q=0.8, application/json;q=0.8").unwrap();
        assert_eq!(accepted.format, Format::Yaml, "first-listed of equal priority must win");
    }

    #[test]
    fn an_unsupported_range_is_skipped_in_favor_of_a_supported_one() {
        let accepted = negotiate("application/xml, application/json").unwrap();
        assert_eq!(accepted.format, Format::Json);
    }

    /// The real shape `kubectl get` sends when it wants server-side
    /// printing: `as=Table` plus the `meta.k8s.io` group/version it
    /// understands, alongside a protobuf fallback if the server doesn't
    /// support Table.
    #[test]
    fn kubectl_style_table_request_is_parsed() {
        let accepted = negotiate(
            "application/json;as=Table;v=v1;g=meta.k8s.io, application/json",
        )
        .unwrap();
        assert_eq!(accepted.format, Format::Json);
        assert!(accepted.wants_table());
        assert_eq!(accepted.as_group.as_deref(), Some("meta.k8s.io"));
        assert_eq!(accepted.as_version.as_deref(), Some("v1"));
    }

    /// `as=` is a generic client-requested-kind parameter, not a
    /// Table-specific boolean — `kubectl get --raw /apis` (or client-go's
    /// aggregated discovery client) sends a completely different `as=`
    /// value, and this module must capture it just as faithfully.
    #[test]
    fn a_non_table_as_value_is_captured_generically_and_does_not_want_table() {
        let accepted = negotiate("application/json;as=APIGroupDiscoveryList;v=v2;g=apidiscovery.k8s.io").unwrap();
        assert_eq!(accepted.as_kind.as_deref(), Some("APIGroupDiscoveryList"));
        assert_eq!(accepted.as_group.as_deref(), Some("apidiscovery.k8s.io"));
        assert_eq!(accepted.as_version.as_deref(), Some("v2"));
        assert!(!accepted.wants_table());
    }

    #[test]
    fn no_as_parameter_at_all_means_no_alternate_representation_was_requested() {
        let accepted = negotiate("application/json").unwrap();
        assert_eq!(accepted.as_kind, None);
        assert!(!accepted.wants_table());
    }

    #[test]
    fn content_type_ignores_a_trailing_charset_parameter() {
        assert_eq!(content_type("application/json; charset=utf-8"), Some(Format::Json));
    }

    #[test]
    fn content_type_of_an_unsupported_type_is_none() {
        assert_eq!(content_type("text/plain"), None);
    }

    #[test]
    fn media_type_round_trips_through_negotiate() {
        for f in [Format::Json, Format::Yaml, Format::Protobuf] {
            assert_eq!(negotiate(f.media_type()).unwrap().format, f);
        }
    }
}
