//! Tool input schemas every MCP client can read.
//!
//! schemars describes an `Option<T>` parameter as `"type": ["integer", "null"]`, or as
//! `anyOf: [T, {"type": "null"}]` for an enum. Both are legal JSON Schema, but clients that map
//! tool schemas onto a single-`type` dialect (such as the subset of `OpenAPI` used by Gemini
//! function declarations) may reject the tool. Every optional parameter here means "use the default"
//! when it's left out, so the `null` adds nothing: [`without_null`] drops it, leaving a plain
//! type that isn't `required`. A client that still sends `null` is accepted, since serde reads
//! it as `None`.

use rmcp::schemars::Schema;
use serde_json::{Map, Value};

/// Removes the `null` alternative from every property of a parameters schema. Use as
/// `#[schemars(transform = crate::schema::without_null)]` on the parameters struct.
pub fn without_null(schema: &mut Schema) {
    let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
        return;
    };
    for property in properties.values_mut() {
        if let Some(property) = property.as_object_mut() {
            drop_null_type(property);
            drop_null_branch(property);
        }
    }
}

/// `"type": ["integer", "null"]` → `"type": "integer"`.
fn drop_null_type(property: &mut Map<String, Value>) {
    let Some(Value::Array(types)) = property.get("type") else {
        return;
    };
    let mut rest = types.iter().filter(|t| t.as_str() != Some("null"));
    if let (Some(only), None) = (rest.next(), rest.next()) {
        let only = only.clone();
        property.insert("type".to_string(), only);
    }
}

/// `anyOf: [X, {"type": "null"}]` → the keys of `X`, next to the property's own (description,
/// default, …).
fn drop_null_branch(property: &mut Map<String, Value>) {
    let Some(Value::Array(branches)) = property.get("anyOf") else {
        return;
    };
    let is_null = |branch: &&Value| branch.get("type").and_then(Value::as_str) == Some("null");
    let mut rest = branches.iter().filter(|b| !is_null(b));
    let (Some(Value::Object(only)), None) = (rest.next(), rest.next()) else {
        return;
    };
    let only = only.clone();
    property.remove("anyOf");
    for (key, value) in only {
        property.entry(key).or_insert(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn transformed(value: Value) -> Value {
        let mut schema = Schema::try_from(value).unwrap_or_default();
        without_null(&mut schema);
        schema.to_value()
    }

    #[test]
    fn test_optional_types_become_plain() {
        let schema = transformed(json!({
            "type": "object",
            "properties": {
                "rows": {"type": ["integer", "null"], "format": "uint16", "description": "Rows."},
                "patterns": {"type": ["array", "null"], "items": {"type": "string"}},
                "command": {"type": "string"},
                "either": {"type": ["string", "integer"]},
            },
            "required": ["command"],
        }));
        assert_eq!(
            schema["properties"]["rows"],
            json!({"type": "integer", "format": "uint16", "description": "Rows."})
        );
        assert_eq!(schema["properties"]["patterns"]["type"], "array");
        assert_eq!(schema["properties"]["command"], json!({"type": "string"}));
        // Not a nullable: left alone
        assert_eq!(
            schema["properties"]["either"]["type"],
            json!(["string", "integer"])
        );
        assert_eq!(schema["required"], json!(["command"]));
    }

    #[test]
    fn test_optional_enum_loses_its_null_branch() {
        let schema = transformed(json!({
            "type": "object",
            "properties": {
                "syntax": {
                    "description": "How patterns are read.",
                    "anyOf": [{"$ref": "#/$defs/PatternSyntax"}, {"type": "null"}],
                },
            },
        }));
        assert_eq!(
            schema["properties"]["syntax"],
            json!({"description": "How patterns are read.", "$ref": "#/$defs/PatternSyntax"})
        );
    }
}
