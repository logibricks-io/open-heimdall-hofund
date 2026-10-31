use std::collections::BTreeSet;

use serde_json::{Map, Value};

/// Cuts every Code Assist function declaration's parameters down to the
/// OpenAPI-style schema subset the upstream accepts.
pub fn plain_code_assist_tools(body: &mut Map<String, Value>) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        let Some(declarations) = tool
            .get_mut("functionDeclarations")
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for declaration in declarations {
            let Some(declaration) = declaration.as_object_mut() else {
                continue;
            };
            if let Some(parameters) = declaration.get_mut("parameters") {
                *parameters = plain_schema(parameters);
            }
        }
    }
}

/// Converts one JSON Schema to the plain schema subset Code Assist accepts.
#[must_use]
pub fn plain_schema(schema: &Value) -> Value {
    if !schema.is_object() {
        return schema.clone();
    }
    Walker {
        root: schema,
        expanding: BTreeSet::new(),
    }
    .walk(schema, 0, "")
    .unwrap_or(Value::Null)
}

struct Walker<'a> {
    root: &'a Value,
    expanding: BTreeSet<String>,
}

impl<'a> Walker<'a> {
    fn walk(&mut self, value: &Value, depth: u32, path: &str) -> Option<Value> {
        match value {
            Value::Bool(true) => Some(Value::Object(Map::new())),
            Value::Bool(false) => None,
            Value::Array(items) => Some(Value::Array(
                items
                    .iter()
                    .map(|item| self.walk(item, depth, path).unwrap_or(Value::Null))
                    .collect(),
            )),
            Value::Object(object) => self.walk_object(object, depth, path),
            _ => Some(value.clone()),
        }
    }

    fn walk_object(
        &mut self,
        object: &Map<String, Value>,
        depth: u32,
        path: &str,
    ) -> Option<Value> {
        if depth > 32 {
            return Some(typed("object"));
        }
        if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
            let Some(target) = self.local_ref(reference) else {
                return Some(typed("object"));
            };
            if self.expanding.contains(reference) || self_ref(reference, path) {
                return Some(match target.as_object() {
                    Some(target) => typed(schema_type_of(target)),
                    None => typed("object"),
                });
            }
            self.expanding.insert(reference.to_owned());
            let target_path = ref_path(reference, path);
            let out = self.walk(target, depth + 1, &target_path);
            self.expanding.remove(reference);
            return out;
        }
        if let Some(all) = object.get("allOf").and_then(Value::as_array) {
            let mut merged = object.clone();
            merged.remove("allOf");
            let mut required = required_names(object.get("required"));
            for member in all {
                if let Some(resolved) = self.resolve(member) {
                    required.extend(required_names(resolved.get("required")));
                }
                let member_path = format!("{path}/allOf");
                let Some(Value::Object(member)) = self.walk(member, depth + 1, &member_path) else {
                    continue;
                };
                for (key, value) in member {
                    if key == "required" {
                        continue;
                    }
                    if key == "properties" {
                        let properties = merged
                            .entry(key)
                            .or_insert_with(|| Value::Object(Map::new()));
                        if let (Some(properties), Some(value)) =
                            (properties.as_object_mut(), value.as_object())
                        {
                            for (name, schema) in value {
                                properties.insert(name.clone(), schema.clone());
                            }
                        }
                    } else {
                        merged.entry(key).or_insert(value);
                    }
                }
            }
            if !required.is_empty() {
                merged.insert("required".to_owned(), strings(distinct(required)));
            }
            return self.walk(&Value::Object(merged), depth + 1, path);
        }
        if (object.contains_key("anyOf") || object.contains_key("oneOf")) && own_shape(object) {
            let folded = self.fold_union(object);
            return self.walk(&folded, depth + 1, path);
        }
        for key in ["anyOf", "oneOf"] {
            let Some(alternatives) = object.get(key).and_then(Value::as_array) else {
                continue;
            };
            let mut pick = None;
            let mut nullable = false;
            for alternative in alternatives {
                let Some(alternative) = alternative.as_object() else {
                    continue;
                };
                if alternative.get("type").and_then(Value::as_str) == Some("null") {
                    nullable = true;
                } else if pick.is_none() {
                    pick = Some(alternative);
                }
            }
            let mut out = Map::new();
            if let Some(pick) = pick {
                let pick_path = format!("{path}/{key}");
                if let Some(Value::Object(pick)) = self.walk_object(pick, depth + 1, &pick_path) {
                    out = pick;
                }
            }
            if let Some(description) = object.get("description").and_then(Value::as_str)
                && !out.contains_key("description")
            {
                out.insert(
                    "description".to_owned(),
                    Value::String(description.to_owned()),
                );
            }
            if nullable {
                out.insert("nullable".to_owned(), Value::Bool(true));
            }
            out.entry("type".to_owned())
                .or_insert_with(|| Value::String("string".to_owned()));
            return Some(Value::Object(out));
        }

        let mut out = Map::new();
        for (key, value) in object {
            match key.as_str() {
                "description" | "nullable" | "required" | "pattern" | "minimum" | "maximum"
                | "minLength" | "maxLength" | "minItems" | "maxItems" | "minProperties"
                | "maxProperties" | "propertyOrdering" => {
                    out.insert(key.clone(), value.clone());
                }
                "exclusiveMinimum" | "exclusiveMaximum" => {
                    if value.is_number() {
                        let bound = if key == "exclusiveMinimum" {
                            "minimum"
                        } else {
                            "maximum"
                        };
                        if !object.contains_key(bound) {
                            out.insert(bound.to_owned(), value.clone());
                        }
                    }
                }
                "type" => {
                    if let Some(types) = value.as_array() {
                        for kind in types {
                            if kind == "null" {
                                out.insert("nullable".to_owned(), Value::Bool(true));
                            } else if !out.contains_key("type") {
                                out.insert("type".to_owned(), kind.clone());
                            }
                        }
                        for kind in types {
                            if (kind == "array" && object.contains_key("items"))
                                || (kind == "object" && object.contains_key("properties"))
                            {
                                out.insert("type".to_owned(), kind.clone());
                                break;
                            }
                        }
                    } else {
                        out.insert(key.clone(), value.clone());
                    }
                }
                "properties" => {
                    let mut properties = Map::new();
                    if let Some(values) = value.as_object() {
                        for (name, schema) in values {
                            let escaped = name.replace('~', "~0").replace('/', "~1");
                            let property_path = format!("{path}/properties/{escaped}");
                            if let Some(schema) = self.walk(schema, depth + 1, &property_path) {
                                properties.insert(name.clone(), schema);
                            }
                        }
                    }
                    out.insert(key.clone(), Value::Object(properties));
                }
                "items" => {
                    let item_path = format!("{path}/items");
                    if let Some(schema) = self.walk(value, depth + 1, &item_path) {
                        out.insert(key.clone(), schema);
                    } else if !object.contains_key("maxItems") {
                        out.insert(key.clone(), Value::Object(Map::new()));
                        out.insert("maxItems".to_owned(), Value::from(0));
                    }
                }
                "enum" => {
                    if let Some(values) = value.as_array()
                        && values.iter().all(Value::is_string)
                    {
                        out.insert(key.clone(), value.clone());
                    }
                }
                _ => {}
            }
        }
        if let Some(constant) = object.get("const").and_then(Value::as_str) {
            out.insert(
                "enum".to_owned(),
                Value::Array(vec![Value::String(constant.to_owned())]),
            );
        }
        if !out.contains_key("type") {
            if out.contains_key("properties") {
                out.insert("type".to_owned(), Value::String("object".to_owned()));
            } else if out.contains_key("items") {
                out.insert("type".to_owned(), Value::String("array".to_owned()));
            }
        }
        if out.get("type").and_then(Value::as_str) == Some("array") && !out.contains_key("items") {
            out.insert("items".to_owned(), Value::Object(Map::new()));
        }
        let kind = out.get("type").and_then(Value::as_str).map(str::to_owned);
        if let Some(kind) = kind {
            if kind != "array" {
                for key in ["items", "minItems", "maxItems"] {
                    out.remove(key);
                }
            }
            if kind != "object" {
                for key in [
                    "properties",
                    "required",
                    "minProperties",
                    "maxProperties",
                    "propertyOrdering",
                ] {
                    out.remove(key);
                }
            }
        }
        if let Some(required) = out.get("required").and_then(Value::as_array) {
            let empty = Map::new();
            let properties = out
                .get("properties")
                .and_then(Value::as_object)
                .unwrap_or(&empty);
            let required: Vec<Value> = required
                .iter()
                .filter(|name| {
                    name.as_str()
                        .is_some_and(|name| properties.get(name).is_some_and(|v| !v.is_null()))
                })
                .cloned()
                .collect();
            if required.is_empty() {
                out.remove("required");
            } else {
                out.insert("required".to_owned(), Value::Array(required));
            }
        }
        Some(Value::Object(out))
    }

    fn local_ref(&self, reference: &str) -> Option<&'a Value> {
        if reference == "#" {
            return Some(self.root);
        }
        if let Some(pointer) = reference.strip_prefix("#/") {
            let mut current = self.root;
            for token in pointer.split('/') {
                let token = token.replace("~1", "/").replace("~0", "~");
                let next = match current {
                    Value::Object(object) => object.get(&token),
                    Value::Array(items) => token.parse::<usize>().ok().and_then(|at| items.get(at)),
                    _ => None,
                };
                let Some(next) = next else {
                    return self.definition(reference);
                };
                current = next;
                if current.is_null() {
                    return self.definition(reference);
                }
            }
            return Some(current);
        }
        self.definition(reference)
    }

    fn definition(&self, reference: &str) -> Option<&'a Value> {
        let name = reference.rsplit('/').next()?;
        self.root
            .get("$defs")
            .into_iter()
            .chain(self.root.get("definitions"))
            .filter_map(Value::as_object)
            .find_map(|definitions| definitions.get(name))
    }

    fn resolve<'b>(&'b self, value: &'b Value) -> Option<&'b Map<String, Value>> {
        let object = value.as_object()?;
        match object.get("$ref").and_then(Value::as_str) {
            Some(reference) => self.local_ref(reference)?.as_object(),
            None => Some(object),
        }
    }

    fn fold_union(&self, object: &Map<String, Value>) -> Value {
        let mut out = object.clone();
        out.remove("anyOf");
        out.remove("oneOf");
        let is_object = object.get("type").and_then(Value::as_str) == Some("object")
            || object.contains_key("properties")
            || object
                .get("type")
                .and_then(Value::as_array)
                .is_some_and(|types| types.iter().any(|kind| kind == "object"));
        let mut properties = object
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut common = Vec::new();
        let mut branches = 0;
        for key in ["anyOf", "oneOf"] {
            for alternative in object
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(alternative) = self.resolve(alternative) else {
                    continue;
                };
                if alternative.get("type").and_then(Value::as_str) == Some("null") {
                    out.insert("nullable".to_owned(), Value::Bool(true));
                    continue;
                }
                if !is_object {
                    continue;
                }
                if let Some(branch) = alternative.get("properties").and_then(Value::as_object) {
                    for (name, schema) in branch {
                        properties
                            .entry(name.clone())
                            .or_insert_with(|| schema.clone());
                    }
                }
                let required = required_names(alternative.get("required"));
                if branches == 0 {
                    common = required;
                } else {
                    common.retain(|name| required.contains(name));
                }
                branches += 1;
            }
        }
        if is_object {
            if !properties.is_empty() {
                out.insert("properties".to_owned(), Value::Object(properties));
            }
            let mut required = required_names(object.get("required"));
            required.extend(common);
            if !required.is_empty() {
                out.insert("required".to_owned(), strings(distinct(required)));
            }
        }
        Value::Object(out)
    }
}

fn self_ref(reference: &str, path: &str) -> bool {
    let Some(reference) = reference.strip_prefix('#') else {
        return false;
    };
    reference.is_empty()
        || path == reference
        || path
            .strip_prefix(reference)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn ref_path(reference: &str, path: &str) -> String {
    reference
        .strip_prefix('#')
        .map_or_else(|| path.to_owned(), str::to_owned)
}

fn schema_type_of(object: &Map<String, Value>) -> &str {
    if let Some(kind) = object.get("type").and_then(Value::as_str) {
        return kind;
    }
    if object.contains_key("properties") {
        return "object";
    }
    if object.contains_key("items") {
        return "array";
    }
    "object"
}

fn own_shape(object: &Map<String, Value>) -> bool {
    if object.contains_key("properties") || object.contains_key("items") {
        return true;
    }
    match object.get("type") {
        Some(Value::String(kind)) => kind != "null",
        Some(Value::Array(types)) => types.iter().any(|kind| kind != "null"),
        _ => false,
    }
}

fn required_names(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn distinct(names: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    names
        .into_iter()
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

fn strings(names: Vec<String>) -> Value {
    Value::Array(names.into_iter().map(Value::String).collect())
}

fn typed(kind: &str) -> Value {
    serde_json::json!({"type": kind})
}
