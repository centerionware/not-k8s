//! Parses the vendored `proto2` `.proto` files
//! (`vendor/protos/**/generated.proto`, upstream's `go-to-protobuf` output)
//! into a flat field table: `(qualified message name, JSON field name) ->
//! (field number, repeated, map, proto type)`.
//!
//! See `docs/APISERVER_PLAN.md` finding 6 for why this exists instead of
//! generating a second struct universe with prost: field numbers have gaps
//! (a removed field leaves a hole), so they must be *parsed* from the real
//! source, never inferred from declaration order. This module is a small
//! hand-written tokenizer, not a general proto grammar — go-to-protobuf's
//! output is machine-generated and structurally uniform (flat top-level
//! `message` blocks, no `oneof`/`extend`/nested `enum` in the k8s API
//! surface, comments only ever precede a field or message), so a general
//! parser would be solving a much bigger problem than the one this crate
//! actually has.
//!
//! # The message-name bridge
//!
//! A proto file's `package` (e.g. `k8s.io.api.apps.v1`) plus a message name
//! (e.g. `DaemonSetSpec`) needs to line up with the OpenAPI v3 definition
//! key for the *same* type (`io.k8s.api.apps.v1.DaemonSetSpec`) so
//! `openapi_parse.rs`'s table and this one can be joined by one shared key.
//! Verified directly against the vendored artifacts (not assumed): the
//! OpenAPI key is the proto package with its first two dot-segments
//! reversed (`k8s.io` -> `io.k8s`) — the standard Java-style reverse-DNS
//! form `go-to-protobuf`/`openapi-gen` both derive from the same Go import
//! path, just written two different ways. `qualified_name()` below performs
//! exactly that swap.

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ProtoField {
    /// OpenAPI-style qualified message name, e.g. `io.k8s.api.apps.v1.DaemonSetSpec`.
    pub message: String,
    /// JSON field name — identical to the proto field name for the
    /// overwhelming majority of fields (verified, finding 6): `selector`,
    /// `minReadySeconds`, etc. **One real, named exception, found live**:
    /// `JSONSchemaProps`'s own `x-kubernetes-*` extension fields
    /// (`x-kubernetes-list-type`, `x-kubernetes-preserve-unknown-fields`,
    /// ...) have a real Go JSON tag that does *not* follow the standard
    /// camelCase-from-field-name convention every other vendored field
    /// does — `xKubernetesListType`'s real JSON key is
    /// `x-kubernetes-list-type`, kebab-case with a literal `x-` prefix.
    /// Undetected, this silently drops every one of the seven such
    /// fields on encode (the protobuf codec's field lookup by JSON key
    /// never matches a submitted `x-kubernetes-list-type`, so it's
    /// treated as an unrecognized key and skipped) — found live by
    /// `tests/crd_roundtrip.rs`'s own strategic-merge-patch-against-a-CRD
    /// round trip, which stores and rereads a real
    /// `CustomResourceDefinition` and needs its own
    /// `x-kubernetes-list-map-keys` to survive that round trip.
    /// `real_x_kubernetes_json_name` below detects and corrects this one
    /// specific, well-known family (verified against the actual vendored
    /// proto: exactly seven fields anywhere in the whole vendored set
    /// start with `xKubernetes`, all in this one message).
    pub json_name: String,
    pub number: u32,
    pub repeated: bool,
    /// A `map<K, V>` field. Encoded on the wire as `repeated` of a
    /// synthetic two-field (`key = 1`, `value = 2`) entry message — callers
    /// needing the wire encoding must know this, since `repeated` alone
    /// isn't enough to reconstruct it.
    pub map: bool,
    /// The proto type as written: a scalar (`int32`, `string`, `bool`, ...),
    /// `map<K, V>`, or a (possibly package-qualified, leading-dot) message
    /// type name. Left as-written rather than resolved — the field table's
    /// consumer (Group B's codec) resolves cross-references against this
    /// same table, so resolving here would just be redone downstream.
    pub proto_type: String,
}

/// Parse every `generated.proto` found under `root` (recursively) into one
/// flat field table, plus the set of qualified message names seen (message
/// existence matters even for a message with zero fields, e.g. empty
/// `List`-wrapper params).
pub fn parse_all(root: &Path) -> (Vec<ProtoField>, Vec<String>) {
    let mut fields = Vec::new();
    let mut messages = Vec::new();
    let mut proto_files = Vec::new();
    collect_proto_files(root, &mut proto_files);
    proto_files.sort();
    for path in &proto_files {
        let src = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        parse_file(&src, &mut fields, &mut messages);
    }
    (fields, messages)
}

fn collect_proto_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_proto_files(&path, out);
        } else if path.file_name().and_then(|n| n.to_str()) == Some("generated.proto") {
            out.push(path);
        }
    }
}

/// The reverse-DNS bridge described in the module doc: `k8s.io.api.apps.v1`
/// -> `io.k8s.api.apps.v1`. Every vendored package starts with the two
/// segments `k8s`, `io` (the Go module's own domain, reversed by
/// go-to-protobuf into dotted-package form); swapping just those two
/// leading segments is the whole transform; anything unexpected panics at
/// build time rather than silently emitting a table entry nothing can look
/// up.
fn openapi_package(proto_package: &str) -> String {
    let mut parts: Vec<&str> = proto_package.split('.').collect();
    assert!(
        parts.len() >= 2 && parts[0] == "k8s" && parts[1] == "io",
        "unexpected proto package {proto_package:?} — expected to start with \"k8s.io\""
    );
    parts.swap(0, 1);
    parts.join(".")
}

/// Strip a leading '.' (proto's fully-qualified-reference marker) — this
/// table stores types as written, so only the display convenience of a
/// leading dot needs stripping, not full resolution.
fn strip_leading_dot(s: &str) -> &str {
    s.strip_prefix('.').unwrap_or(s)
}

fn parse_file(src: &str, fields: &mut Vec<ProtoField>, messages: &mut Vec<String>) {
    // Strip both /* */ block comments and // line comments in one pass —
    // go-to-protobuf never emits a field whose meaning depends on a
    // trailing comment, so this is safe and keeps the line-based tokenizer
    // below simple. Must be one combined pass, not "strip block comments,
    // then strip // per line": several doc comments describe glob patterns
    // like `"/healthz/*"` — a naive raw scan for literal "/*"/"*/" tokens
    // would treat that "/*" inside a `//` comment as a real block-comment
    // opener and could eat real field declarations before the next
    // coincidental "*/". Tracking line-comment state suppresses block-open
    // detection while already inside a `//` comment, which is what avoids
    // that.
    let src = strip_comments(src);
    let lines: Vec<&str> = src.lines().collect();

    let mut package = String::new();
    let mut go_package = String::new();
    for line in &lines {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("package ") {
            package = rest.trim_end_matches(';').trim().to_string();
        } else if let Some(rest) = line.strip_prefix("option go_package") {
            // option go_package = "k8s.io/api/apps/v1";
            if let Some(q) = rest.split('"').nth(1) {
                go_package = q.to_string();
            }
        }
    }
    let openapi_pkg = if !go_package.is_empty() {
        // Prefer go_package (matches upstream's own derivation exactly);
        // fall back to the proto `package` statement if a file is ever
        // missing the option.
        openapi_package_from_go_path(&go_package)
    } else {
        openapi_package(&package)
    };

    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if let Some(rest) = trimmed.strip_prefix("message ") {
            let name = rest.trim_end_matches('{').trim().to_string();
            let qualified = format!("{openapi_pkg}.{name}");
            messages.push(qualified.clone());
            // Find the matching closing brace by depth, collecting the body.
            let mut depth = if trimmed.ends_with('{') { 1i32 } else { 0 };
            let mut body_start = i + 1;
            if depth == 0 {
                // "message Foo" and "{" on separate lines — uncommon in this
                // generator's output, handled defensively anyway.
                while body_start < lines.len() && !lines[body_start].trim().starts_with('{') {
                    body_start += 1;
                }
                depth = 1;
                body_start += 1;
            }
            let mut j = body_start;
            while j < lines.len() && depth > 0 {
                depth += brace_delta(lines[j]);
                if depth == 0 {
                    break;
                }
                j += 1;
            }
            let body = &lines[body_start..j.min(lines.len())];
            parse_message_body(body, &qualified, fields);
            i = j + 1;
            continue;
        }
        i += 1;
    }
}

fn brace_delta(line: &str) -> i32 {
    line.matches('{').count() as i32 - line.matches('}').count() as i32
}

/// Strips `/* */` block comments and `// ...` line comments in a single
/// combined-state pass — see `parse_file`'s call site for why a two-pass
/// (block first, then line) approach is unsafe here. Byte-level, not
/// char-level: the vendored files are confirmed pure ASCII (checked
/// directly), so treating each byte as its own `char` cannot corrupt
/// anything and keeps this a simple linear scan.
fn strip_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut in_block = false;
    let mut in_line = false;
    let mut i = 0;
    while i < bytes.len() {
        if in_block {
            if bytes[i..].starts_with(b"*/") {
                in_block = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if in_line {
            if bytes[i] == b'\n' {
                in_line = false;
                out.push('\n');
            }
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            in_block = true;
            i += 2;
            continue;
        }
        if bytes[i..].starts_with(b"//") {
            in_line = true;
            i += 2;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// `k8s.io/api/apps/v1` -> `io.k8s.api.apps.v1` — same swap as
/// `openapi_package`, just starting from the slash-separated Go import path
/// form instead of the dotted proto-package form.
fn openapi_package_from_go_path(go_path: &str) -> String {
    let dotted: Vec<String> = go_path.split('/').map(|s| s.to_string()).collect();
    let joined = dotted.join(".");
    openapi_package(&joined)
}

/// One-line, semicolon-terminated field declarations only — every field in
/// this generator's output fits on one logical line once comments are
/// stripped (verified across the vendored set; block-spanning field
/// declarations would need `[...]` options split across lines, which
/// go-to-protobuf never emits).
fn parse_message_body(body: &[&str], message: &str, fields: &mut Vec<ProtoField>) {
    let mut depth = 0i32;
    for raw in body {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        depth += brace_delta(line);
        if depth > 0 {
            // Inside a nested message/oneof/enum block that isn't a plain
            // field — the k8s API surface doesn't nest messages, but stay
            // defensive: skip lines until back at this message's own depth
            // rather than mis-parsing an inner block's fields as our own.
            continue;
        }
        if let Some(field) = parse_field_line(line, message) {
            fields.push(field);
        }
    }
}

fn parse_field_line(line: &str, message: &str) -> Option<ProtoField> {
    let line = line.trim_end_matches(';').trim();
    if line.is_empty() {
        return None;
    }
    // Cut any trailing "[...]" field options (rare in this generator's
    // output, but e.g. deprecated-field annotations do appear) before
    // tokenizing on whitespace.
    let line = match line.find('[') {
        Some(idx) => line[..idx].trim(),
        None => line,
    };

    let (label, rest) = if let Some(r) = line.strip_prefix("optional ") {
        (Some("optional"), r)
    } else if let Some(r) = line.strip_prefix("required ") {
        (Some("required"), r)
    } else if let Some(r) = line.strip_prefix("repeated ") {
        (Some("repeated"), r)
    } else {
        (None, line)
    };

    // rest is now "<type> <name> = <number>"
    let eq = rest.rfind('=')?;
    let (type_and_name, number_str) = rest.split_at(eq);
    let number: u32 = number_str[1..].trim().parse().ok()?;
    let type_and_name = type_and_name.trim();
    let last_space = type_and_name.rfind(char::is_whitespace)?;
    let (ty, name) = (type_and_name[..last_space].trim(), type_and_name[last_space..].trim());

    let is_map = ty.starts_with("map<");
    let repeated = label == Some("repeated") || is_map;
    let proto_type = strip_leading_dot(ty).to_string();
    let json_name = real_x_kubernetes_json_name(name).unwrap_or_else(|| name.to_string());

    Some(ProtoField {
        message: message.to_string(),
        json_name,
        number,
        repeated,
        map: is_map,
        proto_type,
    })
}

/// Real upstream's own JSON tag for an `x-kubernetes-*` extension field —
/// see [`ProtoField::json_name`]'s own doc comment for why this exists
/// at all. `proto_field_name` is the raw identifier as written in the
/// `.proto` file (e.g. `xKubernetesListType`); returns `None` for any
/// field that doesn't start with the literal `xKubernetes` prefix (every
/// other field in the whole vendored set), so this never touches an
/// unrelated field that merely happens to start with a lowercase `x`.
/// The transform itself: split on camelCase word boundaries, lowercase,
/// join with `-` — `xKubernetesListMapKeys` -> `x-kubernetes-list-map-keys`,
/// confirmed character-by-character against all seven real fields this
/// pattern covers (`ProtoField::json_name`'s own doc comment names the
/// exact count).
fn real_x_kubernetes_json_name(proto_field_name: &str) -> Option<String> {
    if !proto_field_name.starts_with("xKubernetes") {
        return None;
    }
    let mut out = String::with_capacity(proto_field_name.len() + 4);
    for (i, ch) in proto_field_name.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

/// Renders the parsed table as Rust source: one `ProtoField`-shaped tuple
/// literal per field, grouped under a `pub static PROTO_FIELDS` slice, plus
/// a `pub static PROTO_MESSAGES` slice of every message name seen (message
/// existence matters even with zero fields).
pub fn render(fields: &[ProtoField], messages: &[String]) -> String {
    let mut by_message: BTreeMap<&str, Vec<&ProtoField>> = BTreeMap::new();
    for f in fields {
        by_message.entry(f.message.as_str()).or_default().push(f);
    }

    let mut out = String::new();
    out.push_str("// @generated by build.rs (proto_parse) from vendor/protos — do not edit.\n\n");
    out.push_str("pub struct ProtoField {\n");
    out.push_str("    pub message: &'static str,\n");
    out.push_str("    pub json_name: &'static str,\n");
    out.push_str("    pub number: u32,\n");
    out.push_str("    pub repeated: bool,\n");
    out.push_str("    pub map: bool,\n");
    out.push_str("    pub proto_type: &'static str,\n");
    out.push_str("}\n\n");

    out.push_str("pub static PROTO_FIELDS: &[ProtoField] = &[\n");
    for (message, group) in &by_message {
        for f in group {
            out.push_str(&format!(
                "    ProtoField {{ message: {:?}, json_name: {:?}, number: {}, repeated: {}, map: {}, proto_type: {:?} }},\n",
                message, f.json_name, f.number, f.repeated, f.map, f.proto_type
            ));
        }
    }
    out.push_str("];\n\n");

    let mut sorted_messages: Vec<&str> = messages.iter().map(|s| s.as_str()).collect();
    sorted_messages.sort();
    sorted_messages.dedup();
    out.push_str("pub static PROTO_MESSAGES: &[&str] = &[\n");
    for m in sorted_messages {
        out.push_str(&format!("    {m:?},\n"));
    }
    out.push_str("];\n");

    out
}
