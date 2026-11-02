//! Rename provably disjoint `anyOf` unions to `oneOf` in place.

use serde_json::Value;

use crate::json::{self, Edit, Node};

/// Renames every provably disjoint `anyOf` under tool schemas to `oneOf`.
#[must_use]
pub fn rename_disjoint_any_of(body: &[u8]) -> Vec<u8> {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let Some(root) = json::parse(body) else {
        return body.to_vec();
    };
    let mut edits = Vec::new();
    collect_edits(&root, &value, false, &mut edits);
    if edits.is_empty() {
        return body.to_vec();
    }
    json::apply(body, edits)
}

fn collect_edits(node: &Node, value: &Value, in_schema: bool, edits: &mut Vec<Edit>) {
    match value {
        Value::Array(items) => {
            if let Some(nodes) = node.items() {
                for (child, item) in nodes.iter().zip(items) {
                    collect_edits(child, item, in_schema, edits);
                }
            }
        }
        Value::Object(map) => {
            if in_schema
                && let Some(any_of) = map.get("anyOf").and_then(Value::as_array)
                && any_of.len() >= 2
                && branches_disjoint(any_of)
                && let Some(members) = node.members()
                && let Some(member) = members.iter().find(|member| member.key == "anyOf")
            {
                edits.push(Edit::new(
                    member.key_start,
                    member.key_end,
                    br#""oneOf""#.to_vec(),
                ));
            }
            if let Some(members) = node.members() {
                for member in members {
                    let child = map.get(&member.key);
                    let next_schema = in_schema
                        || matches!(
                            member.key.as_str(),
                            "parameters" | "input_schema" | "function" | "properties" | "items"
                                | "$defs" | "definitions"
                        );
                    if let Some(child) = child {
                        collect_edits(&member.value, child, next_schema, edits);
                    }
                }
            }
        }
        _ => {}
    }
}

fn branches_disjoint(branches: &[Value]) -> bool {
    let parsed: Vec<BranchInfo> = branches.iter().map(parse_branch).collect();
    if parsed.iter().any(|branch| branch.nullable) {
        return false;
    }
    for left in 0..parsed.len() {
        for right in left + 1..parsed.len() {
            if !pair_disjoint(&parsed[left], &parsed[right]) {
                return false;
            }
        }
    }
    true
}

#[derive(Clone, Debug, Default)]
struct BranchInfo {
    nullable: bool,
    fields: std::collections::BTreeMap<String, Vec<String>>,
    forbidden: std::collections::BTreeSet<String>,
    loose: bool,
}

fn parse_branch(value: &Value) -> BranchInfo {
    let mut info = BranchInfo::default();
    scan_branch(value, &mut info);
    info
}

fn scan_branch(value: &Value, info: &mut BranchInfo) {
    let Some(map) = value.as_object() else {
        return;
    };
    if map.get("type").and_then(Value::as_str) == Some("null") {
        info.nullable = true;
    }
    if map.contains_key("patternProperties") {
        info.loose = true;
    }
    if map.keys().any(|key| matches!(key.as_str(), "anyOf" | "oneOf" | "allOf" | "not")) {
        info.loose = true;
    }
    if map.get("additionalProperties").and_then(Value::as_bool) == Some(false)
        && let Some(required) = map.get("required").and_then(Value::as_array)
    {
        for name in required.iter().filter_map(Value::as_str) {
            info.forbidden.insert(name.to_owned());
        }
    }
    if let Some(properties) = map.get("properties").and_then(Value::as_object) {
        for (name, schema) in properties {
            if let Some(values) = string_enum(schema) {
                info.fields.insert(name.clone(), values);
            }
            scan_branch(schema, info);
        }
    }
    if let Some(items) = map.get("items") {
        scan_branch(items, info);
    }
    if let Some(defs) = map.get("$defs").or_else(|| map.get("definitions")) {
        scan_branch(defs, info);
    }
}

fn string_enum(schema: &Value) -> Option<Vec<String>> {
    let values = schema.get("enum")?.as_array()?;
    let mut out = Vec::new();
    for value in values {
        out.push(value.as_str()?.to_owned());
    }
    Some(out)
}

fn pair_disjoint(left: &BranchInfo, right: &BranchInfo) -> bool {
    if left.nullable || right.nullable || left.loose || right.loose {
        return false;
    }
    if left.fields.is_empty()
        && right.fields.is_empty()
        && left.forbidden.is_empty()
        && right.forbidden.is_empty()
    {
        return false;
    }
    if enum_disjoint(left, right) {
        return true;
    }
    additional_properties_disjoint(left, right)
}

fn enum_disjoint(left: &BranchInfo, right: &BranchInfo) -> bool {
    for (name, left_values) in &left.fields {
        if left_values.is_empty() {
            continue;
        }
        let Some(right_values) = right.fields.get(name) else {
            continue;
        };
        if right_values.is_empty() {
            continue;
        }
        if left_values
            .iter()
            .all(|value| !right_values.iter().any(|other| other == value))
        {
            return true;
        }
    }
    false
}

fn additional_properties_disjoint(left: &BranchInfo, right: &BranchInfo) -> bool {
    for name in &left.forbidden {
        if right.fields.contains_key(name) {
            return true;
        }
    }
    for name in &right.forbidden {
        if left.fields.contains_key(name) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::rename_disjoint_any_of;

    #[test]
    fn d49_1_enum_disjoint_renames_any_of_to_one_of() {
        let body = br#"{"tools":[{"type":"function","function":{"name":"t","parameters":{"anyOf":[{"type":"object","properties":{"kind":{"type":"string","enum":["a"]}},"required":["kind"],"additionalProperties":false},{"type":"object","properties":{"kind":{"type":"string","enum":["b"]}},"required":["kind"],"additionalProperties":false}]}}}]}"#;
        let out = rename_disjoint_any_of(body);
        assert!(out.windows(7).any(|window| window == br#""oneOf""#));
        assert!(!out.windows(7).any(|window| window == br#""anyOf""#));
        assert_eq!(out.len(), body.len());
    }

    #[test]
    fn d49_2_nullable_union_stays_any_of() {
        let body = br#"{"parameters":{"anyOf":[{"type":"string","enum":["x"]},{"type":"null"}]}}"#;
        let out = rename_disjoint_any_of(body);
        assert!(out.windows(7).any(|w| w == br#""anyOf""#));
    }

    #[test]
    fn d49_3_unproven_union_stays_any_of() {
        let body = br#"{"parameters":{"anyOf":[{"type":"string"},{"type":"integer"}]}}"#;
        let out = rename_disjoint_any_of(body);
        assert!(out.windows(7).any(|w| w == br#""anyOf""#));
    }

    #[test]
    fn nullable_union_is_not_disjoint() {
        let branches: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"type":"string","enum":["x"]},{"type":"null"}]"#,
        )
        .expect("json");
        assert!(!super::branches_disjoint(&branches));
    }
}
