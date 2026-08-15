//! MCP JSON Schema → [`ToolParameter`].
//!
//! Nested object/array is allowed. `$ref` / `anyOf` / `oneOf` / `allOf` and
//! union `type` arrays are not: skip that tool rather than advertise a lie.

use serde_json::{Map, Value};
use smithy_tools::ToolParameter;

pub fn json_schema_to_parameters(schema: &Value) -> Result<Vec<ToolParameter>, String> {
    let obj = schema
        .as_object()
        .ok_or_else(|| "input schema is not an object".to_string())?;
    reject_combinators(obj)?;
    let ty = obj.get("type").and_then(Value::as_str).unwrap_or("object");
    if ty != "object" {
        return Err(format!("root schema type `{ty}` is not object"));
    }
    properties_to_params(obj)
}

fn properties_to_params(obj: &Map<String, Value>) -> Result<Vec<ToolParameter>, String> {
    let required = required_names(obj);
    let Some(props) = obj.get("properties").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut params = Vec::with_capacity(props.len());
    for (name, spec) in props {
        params.push(property_to_param(
            name,
            spec,
            required.iter().any(|r| r == name),
        )?);
    }
    Ok(params)
}

fn property_to_param(name: &str, spec: &Value, required: bool) -> Result<ToolParameter, String> {
    let obj = spec
        .as_object()
        .ok_or_else(|| format!("property `{name}` schema is not an object"))?;
    reject_combinators(obj)?;
    let ty = json_type(obj).ok_or_else(|| {
        format!("property `{name}` has a union or unknown type and cannot be mapped")
    })?;
    let mut description = obj
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(values) = obj.get("enum").and_then(Value::as_array) {
        let shown: Vec<String> = values
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        if !shown.is_empty() {
            let extra = format!("One of: {}.", shown.join(", "));
            if description.is_empty() {
                description = extra;
            } else {
                description = format!("{description} {extra}");
            }
        }
    }
    let mut param = ToolParameter::new(name, ty, description, required);
    match ty {
        "array" => {
            let items = obj
                .get("items")
                .ok_or_else(|| format!("array `{name}` has no items schema"))?;
            param = param.with_items(property_to_param("item", items, true)?);
        }
        "object" => {
            param = param.with_properties(properties_to_params(obj)?);
        }
        _ => {}
    }
    Ok(param)
}

fn json_type(obj: &Map<String, Value>) -> Option<&'static str> {
    match obj.get("type") {
        None if obj.contains_key("properties") => Some("object"),
        None if obj.contains_key("items") => Some("array"),
        Some(Value::String(s)) => match s.as_str() {
            "string" => Some("string"),
            "integer" => Some("integer"),
            "number" => Some("number"),
            "boolean" => Some("boolean"),
            "array" => Some("array"),
            "object" => Some("object"),
            _ => None,
        },
        _ => None,
    }
}

fn required_names(obj: &Map<String, Value>) -> Vec<String> {
    obj.get("required")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn reject_combinators(obj: &Map<String, Value>) -> Result<(), String> {
    for key in ["$ref", "anyOf", "oneOf", "allOf"] {
        if obj.contains_key(key) {
            return Err(format!("schema uses `{key}`"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_object_schema_is_no_parameters() {
        let params = json_schema_to_parameters(&json!({"type": "object"})).unwrap();
        assert!(params.is_empty());
    }

    #[test]
    fn nested_object_and_array_map() {
        let schema = json!({
            "type": "object",
            "properties": {
                "q": {"type": "string", "description": "query"},
                "page": {"type": "integer"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "meta": {
                    "type": "object",
                    "properties": {
                        "ok": {"type": "boolean"}
                    },
                    "required": ["ok"]
                }
            },
            "required": ["q"]
        });
        let params = json_schema_to_parameters(&schema).unwrap();
        assert_eq!(params.len(), 4);
        let q = params.iter().find(|p| p.name == "q").unwrap();
        assert!(q.required);
        let page = params.iter().find(|p| p.name == "page").unwrap();
        assert!(!page.required);
        let tags = params.iter().find(|p| p.name == "tags").unwrap();
        assert_eq!(tags.param_type, "array");
        let meta = params.iter().find(|p| p.name == "meta").unwrap();
        assert_eq!(meta.param_type, "object");
        assert_eq!(meta.properties.as_ref().unwrap()[0].name, "ok");
        assert!(meta.properties.as_ref().unwrap()[0].required);
    }

    #[test]
    fn ref_and_union_are_refused() {
        assert!(json_schema_to_parameters(&json!({"$ref": "#/defs/x"})).is_err());
        assert!(json_schema_to_parameters(&json!({
            "type": "object",
            "properties": {
                "x": {"anyOf": [{"type": "string"}, {"type": "null"}]}
            }
        }))
        .is_err());
        assert!(json_schema_to_parameters(&json!({
            "type": "object",
            "properties": {
                "x": {"type": ["string", "null"]}
            }
        }))
        .is_err());
    }

    #[test]
    fn enum_lands_in_the_description() {
        let params = json_schema_to_parameters(&json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": ["a", "b"]}
            }
        }))
        .unwrap();
        assert!(params[0].description.contains("a"));
        assert!(params[0].description.contains("b"));
    }
}
