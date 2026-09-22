//! Tool naming and schema normalization.
//!
//! MCP tools are addressed by `(server name, tool name)` where both parts are
//! arbitrary server-chosen strings. LLM tool names, by contrast, must be flat,
//! reasonably short and match `^[a-zA-Z0-9_-]+$` for every provider we care
//! about. The helpers here translate between the two worlds:
//!
//! - [`qualified_tool_name`] builds the exposed `mcp__{server}__{tool}` name,
//!   sanitizing segments and truncating over-long names with a stable hash
//!   suffix.
//! - [`ToolNameMap`] keeps the bidirectional mapping for a whole session
//!   (the agent loop needs to resolve an exposed name back to the original
//!   pair before dispatching a call) and de-duplicates names that collide
//!   after sanitization/truncation.
//! - [`normalize_input_schema`] fills in the JSON-Schema defaults an LLM
//!   provider requires (`type: object`, `properties: {}`) without ever
//!   rewriting what the server actually sent.

use std::collections::HashMap;

use serde_json::Map;
use serde_json::Value;

/// Hard cap on the length of an exposed tool name. Longer names are
/// truncated (keeping the `mcp__` prefix) and suffixed with a stable hash of
/// the full name, so the result stays within the cap, is deterministic, and
/// remains distinguishable from other long names sharing the same prefix.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Length (in hex chars) of the hash suffix appended to truncated names.
const HASH_SUFFIX_LEN: usize = 8;

/// `true` for the only characters allowed in a name segment after
/// sanitization: ASCII alphanumerics, `_` and `-`.
fn is_allowed(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Sanitize one name segment (the `server` or `tool` part):
///
/// - every character outside `[a-zA-Z0-9_-]` becomes `-` (one multi-byte
///   character collapses to a single `-`, not one per byte),
/// - a segment that is empty falls back to `fallback` (`"server"`/`"tool"`)
///   so the two `__` separators never end up adjacent,
/// - a first character that is not an ASCII letter or `_` is prefixed with
///   `_` (providers commonly require `^[a-zA-Z_]`).
///
/// The result is always pure ASCII, which is what makes truncation by chars
/// equivalent to truncation by bytes later.
fn sanitize_segment(segment: &str, fallback: &str) -> String {
    let mut sanitized: String = segment
        .chars()
        .map(|c| if is_allowed(c) { c } else { '-' })
        .collect();
    if sanitized.is_empty() {
        sanitized = fallback.to_string();
    }
    match sanitized.chars().next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => sanitized.insert(0, '_'),
    }
    sanitized
}

/// Length of the stem a truncated name keeps: everything before the `_` +
/// hash suffix, chosen so the assembled name fits [`MAX_TOOL_NAME_LEN`]
/// exactly.
fn stem_budget() -> usize {
    MAX_TOOL_NAME_LEN - HASH_SUFFIX_LEN - 1
}

/// Truncate a name to at most `budget` chars, never splitting a multi-byte
/// character. Sanitized names are ASCII by construction, so this is only a
/// defensive guard.
fn truncate(name: &str, budget: usize) -> String {
    name.chars().take(budget).collect()
}

/// 64-bit FNV-1a digest.
///
/// Implemented by hand instead of using `std`'s `DefaultHasher`: the latter's
/// output is explicitly not guaranteed stable across Rust releases, while
/// exposed tool names must resolve identically for the whole lifetime of a
/// conversation, across process restarts included.
fn fnv1a(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Render the low 32 bits of a digest as 8 lowercase hex chars.
fn hash_suffix(bytes: &[u8]) -> String {
    format!("{:08x}", (fnv1a(bytes) & 0xffff_ffff) as u32)
}

/// The exposed tool name for one `(server, tool)` pair: `mcp__{server}__{tool}`.
///
/// Segments are sanitized as described on [`sanitize_segment`]. If the joined
/// name exceeds [`MAX_TOOL_NAME_LEN`], it is truncated (the `mcp__` prefix is
/// always preserved) and a hash suffix of the FULL name is appended, e.g.
/// `mcp__very-long-…-stem_ab12cd34`. The function is deterministic: the same
/// inputs always produce the same output.
///
/// Uniqueness is "practically unique", not guaranteed — two distinct pairs can
/// still collide after sanitization (e.g. `a.b` and `a-b` both sanitize to
/// `a-b`) or truncation. Session-level deduplication is [`ToolNameMap`]'s job.
pub fn qualified_tool_name(server: &str, tool: &str) -> String {
    let full = format!(
        "mcp__{}__{}",
        sanitize_segment(server, "server"),
        sanitize_segment(tool, "tool")
    );
    if full.chars().count() <= MAX_TOOL_NAME_LEN {
        full
    } else {
        format!(
            "{}_{}",
            truncate(&full, stem_budget()),
            hash_suffix(full.as_bytes())
        )
    }
}

/// Bidirectional map between exposed tool names (`mcp__{server}__{tool}`,
/// sanitized/truncated as produced by [`qualified_tool_name`]) and the
/// `(server name, original tool name)` pairs they came from.
///
/// oct-agent registers every tool of every connected server here, exposes the
/// generated names to the LLM, and resolves a name the model called back to
/// the original pair before dispatching over the matching [`McpConnection`].
///
/// Insertion order matters: the first pair to claim a name keeps it, and any
/// later pair that would produce the same exposed name (possible after
/// sanitization — `a.b` vs `a-b` — or truncation) gets a hash-suffixed
/// variant instead, salted with an increasing counter if even that collides.
/// Deduplication is therefore deterministic for a given registration order.
///
/// [`McpConnection`]: crate::McpConnection
#[derive(Debug, Clone, Default)]
pub struct ToolNameMap {
    exposed_to_origin: HashMap<String, (String, String)>,
    origin_to_exposed: HashMap<(String, String), String>,
}

impl ToolNameMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one `(server, tool)` pair and return the exposed name it is
    /// reachable under. Inserting the same pair twice is idempotent and
    /// returns the name chosen by the first insert.
    pub fn insert(&mut self, server: &str, tool: &str) -> String {
        let key = (server.to_string(), tool.to_string());
        if let Some(existing) = self.origin_to_exposed.get(&key) {
            return existing.clone();
        }

        let plain = qualified_tool_name(server, tool);
        let exposed = if self.exposed_to_origin.contains_key(&plain) {
            // The plain name is already taken by a different pair. Keep the
            // length budget and disambiguate by hashing the ORIGINAL pair
            // (the plain name may already have lost the distinction) with an
            // increasing salt until an unused candidate shows up. The loop
            // terminates in practice after at most one step (32 bits of
            // hash) and cannot run out: only finitely many names are taken.
            let budget = stem_budget();
            let mut salt = 0u64;
            loop {
                let mut input = Vec::with_capacity(server.len() + tool.len() + 9);
                input.extend_from_slice(server.as_bytes());
                input.push(0);
                input.extend_from_slice(tool.as_bytes());
                input.extend_from_slice(&salt.to_le_bytes());
                let candidate = format!("{}_{}", truncate(&plain, budget), hash_suffix(&input));
                if !self.exposed_to_origin.contains_key(&candidate) {
                    break candidate;
                }
                salt += 1;
            }
        } else {
            plain
        };

        self.exposed_to_origin.insert(exposed.clone(), key.clone());
        self.origin_to_exposed.insert(key, exposed.clone());
        exposed
    }

    /// Resolve an exposed name back to its `(server, original tool name)`
    /// pair. Returns `None` for names that were never registered — including
    /// plain qualified names that lost their slot to a colliding pair.
    pub fn resolve(&self, exposed: &str) -> Option<(&str, &str)> {
        let (server, tool) = self.exposed_to_origin.get(exposed)?;
        Some((server.as_str(), tool.as_str()))
    }

    /// The exposed name a registered `(server, tool)` pair is reachable
    /// under. Returns `None` for pairs that were never inserted.
    pub fn exposed_name(&self, server: &str, tool: &str) -> Option<&str> {
        self.origin_to_exposed
            .get(&(server.to_string(), tool.to_string()))
            .map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.exposed_to_origin.len()
    }

    pub fn is_empty(&self) -> bool {
        self.exposed_to_origin.is_empty()
    }

    /// Iterate over every registration as `(exposed, server, tool)`.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, &str)> + '_ {
        self.exposed_to_origin
            .iter()
            .map(|(exposed, (server, tool))| (exposed.as_str(), server.as_str(), tool.as_str()))
    }
}

/// Normalize a server-provided tool input schema for LLM consumption.
///
/// Only defaults are filled in — nothing the server sent is ever rewritten:
///
/// - a schema that is not a JSON object at all is replaced by the safest
///   object schema there is (`{"type":"object","properties":{}}`); servers
///   are supposed to send an object but the spec is not always honored,
/// - an object missing `type` gets `"type": "object"`,
/// - an object missing `properties` gets `"properties": {}`,
/// - everything else (including a `type` whose value we would not have
///   chosen) is passed through verbatim.
///
/// The returned value is always a fresh object; the input is not mutated.
pub fn normalize_input_schema(schema: &Value) -> Value {
    let Some(map) = schema.as_object() else {
        return serde_json::json!({
            "type": "object",
            "properties": {},
        });
    };
    let mut map = map.clone();
    if !map.contains_key("type") {
        map.insert("type".to_string(), Value::String("object".to_string()));
    }
    if !map.contains_key("properties") {
        map.insert("properties".to_string(), Value::Object(Map::new()));
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ------------------------------------------------------------------
    // qualified_tool_name / sanitize_segment
    // ------------------------------------------------------------------

    #[test]
    fn plain_names_pass_through() {
        assert_eq!(
            qualified_tool_name("github", "create_issue"),
            "mcp__github__create_issue"
        );
        assert_eq!(qualified_tool_name("fs", "read"), "mcp__fs__read");
        // Underscores and dashes are already legal.
        assert_eq!(qualified_tool_name("a-b_c", "d-e_f"), "mcp__a-b_c__d-e_f");
    }

    #[test]
    fn illegal_characters_become_dashes() {
        // Dots and spaces are replaced.
        assert_eq!(qualified_tool_name("a.b c", "x"), "mcp__a-b-c__x");
    }

    #[test]
    fn multibyte_characters_collapse_to_single_dashes() {
        // Each multi-byte char (Chinese, emoji) is ONE char -> ONE dash, not
        // one dash per UTF-8 byte. All-dash segments get the leading-underscore
        // prefix like any other non-letter start.
        assert_eq!(qualified_tool_name("数据库", "查询"), "mcp___---___--");
        assert_eq!(qualified_tool_name("srv🎉", "t"), "mcp__srv-__t");
    }

    #[test]
    fn empty_segments_fall_back() {
        assert_eq!(qualified_tool_name("", "tool1"), "mcp__server__tool1");
        assert_eq!(qualified_tool_name("srv", ""), "mcp__srv__tool");
        assert_eq!(qualified_tool_name("", ""), "mcp__server__tool");
        // Note: only a truly EMPTY segment falls back — invalid characters
        // are replaced (not dropped), so "///" sanitizes to a non-empty
        // all-dash segment that then gets the underscore prefix.
        assert_eq!(qualified_tool_name("///", "x"), "mcp___---__x");
    }

    #[test]
    fn leading_digit_or_dash_gets_underscore_prefix() {
        // '9' is allowed by the charset but not valid as a first character.
        assert_eq!(qualified_tool_name("9file", "read"), "mcp___9file__read");
        assert_eq!(qualified_tool_name("srv", "-go"), "mcp__srv___-go");
        // Letters and underscores need no prefix.
        assert_eq!(qualified_tool_name("_x", "y"), "mcp___x__y");
    }

    #[test]
    fn name_at_exactly_the_cap_is_untouched() {
        // 5 ("mcp__") + 55 + 2 ("__") + 2 = 64.
        let server = "s".repeat(55);
        let name = qualified_tool_name(&server, "ab");
        assert_eq!(name.chars().count(), MAX_TOOL_NAME_LEN);
        assert_eq!(name, format!("mcp__{server}__ab"));
    }

    #[test]
    fn over_long_name_is_truncated_with_stable_hash_suffix() {
        // 5 + 55 + 2 + 3 = 65 -> one over the cap.
        let server = "s".repeat(55);
        let name = qualified_tool_name(&server, "abc");
        assert_eq!(name.chars().count(), MAX_TOOL_NAME_LEN);
        assert!(name.starts_with("mcp__"), "prefix must survive truncation");
        // 64 = 55 stem chars + '_' + 8 hex chars.
        let (sep, hex) = name.split_at(55);
        let _ = sep;
        assert_eq!(hex.chars().count(), HASH_SUFFIX_LEN + 1);
        assert!(hex.starts_with('_'));
        assert!(
            hex[1..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );

        // Deterministic: same input, same output; different tool, different
        // hash.
        assert_eq!(name, qualified_tool_name(&server, "abc"));
        assert_ne!(name, qualified_tool_name(&server, "abd"));
    }

    #[test]
    fn distinct_long_names_sharing_a_stem_get_distinct_hashes() {
        // Both truncate to the same 55-char stem but must differ in the hash.
        let a = qualified_tool_name(&"a".repeat(60), "tool-one");
        let b = qualified_tool_name(&"a".repeat(60), "tool-two");
        assert_eq!(a.chars().count(), MAX_TOOL_NAME_LEN);
        assert_eq!(b.chars().count(), MAX_TOOL_NAME_LEN);
        assert_eq!(&a[..55], &b[..55]);
        assert_ne!(a, b);
    }

    // ------------------------------------------------------------------
    // ToolNameMap
    // ------------------------------------------------------------------

    #[test]
    fn map_round_trips_both_directions() {
        let mut map = ToolNameMap::new();
        let exposed = map.insert("github", "create_issue");
        assert_eq!(exposed, "mcp__github__create_issue");

        let (server, tool) = map.resolve("mcp__github__create_issue").unwrap();
        assert_eq!((server, tool), ("github", "create_issue"));
        assert_eq!(
            map.exposed_name("github", "create_issue"),
            Some("mcp__github__create_issue")
        );
        assert_eq!(map.len(), 1);
        assert!(!map.is_empty());
    }

    #[test]
    fn map_reinsert_is_idempotent() {
        let mut map = ToolNameMap::new();
        let first = map.insert("srv", "tool");
        let second = map.insert("srv", "tool");
        assert_eq!(first, second);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn map_resolve_unknown_returns_none() {
        let mut map = ToolNameMap::new();
        map.insert("srv", "tool");
        assert!(map.resolve("mcp__other__tool").is_none());
        assert!(map.resolve("").is_none());
        assert!(map.exposed_name("other", "tool").is_none());
        assert!(ToolNameMap::new().resolve("mcp__srv__tool").is_none());
        assert!(ToolNameMap::new().is_empty());
    }

    #[test]
    fn map_dedupes_sanitization_collisions() {
        // "a-b" and "a.b" both produce mcp__a-b__c: the first keeps the plain
        // name, the second gets a distinct hash-suffixed variant.
        let mut map = ToolNameMap::new();
        let first = map.insert("a-b", "c");
        let second = map.insert("a.b", "c");
        assert_eq!(first, "mcp__a-b__c");
        assert_ne!(first, second);
        assert!(second.chars().count() <= MAX_TOOL_NAME_LEN);

        // Both directions still resolve to their own originals.
        assert_eq!(map.resolve(&first), Some(("a-b", "c")));
        assert_eq!(map.resolve(&second), Some(("a.b", "c")));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn map_dedupes_truncation_collisions() {
        // Two 60-char server names identical in the first 55 chars: plain
        // names exceed the cap and their stems are identical, so the hashes
        // (and only they) tell the candidates apart.
        let mut server_a = "a".repeat(55);
        server_a.push_str("XX");
        let mut server_b = "a".repeat(55);
        server_b.push_str("YY");

        let mut map = ToolNameMap::new();
        let first = map.insert(&server_a, "t");
        let second = map.insert(&server_b, "t");
        assert_ne!(first, second);
        assert_eq!(first.chars().count(), MAX_TOOL_NAME_LEN);
        assert_eq!(second.chars().count(), MAX_TOOL_NAME_LEN);
        assert_eq!(map.resolve(&first), Some((server_a.as_str(), "t")));
        assert_eq!(map.resolve(&second), Some((server_b.as_str(), "t")));
    }

    #[test]
    fn map_many_long_tools_all_get_unique_names() {
        // 64 tools whose plain names all truncate to the same stem: every
        // exposed name must still be unique and resolve back correctly.
        let mut map = ToolNameMap::new();
        let mut seen = std::collections::HashSet::new();
        for i in 0..64 {
            let tool = format!("t{:02}{}", i, "x".repeat(70));
            let exposed = map.insert("srv", &tool);
            assert!(
                seen.insert(exposed.clone()),
                "exposed name {exposed} was handed out twice"
            );
            assert_eq!(map.resolve(&exposed), Some(("srv", tool.as_str())));
        }
        assert_eq!(map.len(), 64);
    }

    #[test]
    fn map_iter_yields_every_registration() {
        let mut map = ToolNameMap::new();
        map.insert("srv", "a");
        map.insert("srv", "b");
        let mut entries: Vec<_> = map
            .iter()
            .map(|(e, s, t)| (e.to_string(), s.to_string(), t.to_string()))
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            vec![
                (
                    "mcp__srv__a".to_string(),
                    "srv".to_string(),
                    "a".to_string()
                ),
                (
                    "mcp__srv__b".to_string(),
                    "srv".to_string(),
                    "b".to_string()
                ),
            ]
        );
    }

    // ------------------------------------------------------------------
    // normalize_input_schema
    // ------------------------------------------------------------------

    #[test]
    fn schema_gets_type_and_properties_defaults() {
        let input = json!({"properties": {"x": {"type": "string"}}});
        let out = normalize_input_schema(&input);
        assert_eq!(
            out,
            json!({
                "type": "object",
                "properties": {"x": {"type": "string"}},
            })
        );
    }

    #[test]
    fn schema_missing_only_properties_gets_just_that_default() {
        let input = json!({"type": "object", "additionalProperties": false});
        assert_eq!(
            normalize_input_schema(&input),
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {},
            })
        );
    }

    #[test]
    fn schema_with_both_fields_already_present_is_untouched() {
        let input = json!({
            "type": "object",
            "properties": {"x": {"type": "number"}},
            "required": ["x"],
            "additionalProperties": false,
        });
        assert_eq!(normalize_input_schema(&input), input);
    }

    #[test]
    fn schema_present_type_is_never_rewritten() {
        // Even when we would not have chosen it: server schemas are passed
        // through verbatim apart from the two defaults.
        let input = json!({"type": "string", "properties": {"x": {}}});
        assert_eq!(normalize_input_schema(&input), input);
        // A non-string type is kept; only the missing `properties` default
        // is added.
        let array_type = json!({"type": ["object", "null"]});
        assert_eq!(
            normalize_input_schema(&array_type),
            json!({"type": ["object", "null"], "properties": {}})
        );
    }

    #[test]
    fn non_object_schema_is_replaced_by_empty_object_schema() {
        for input in [
            json!("string"),
            json!(42),
            json!(null),
            json!(true),
            json!([1, 2]),
        ] {
            assert_eq!(
                normalize_input_schema(&input),
                json!({"type": "object", "properties": {}})
            );
        }
    }

    #[test]
    fn empty_object_gets_both_defaults() {
        assert_eq!(
            normalize_input_schema(&json!({})),
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn schema_normalization_does_not_mutate_input() {
        let input = json!({"properties": {"x": {}}});
        let original = input.clone();
        let _ = normalize_input_schema(&input);
        assert_eq!(input, original);
    }
}
