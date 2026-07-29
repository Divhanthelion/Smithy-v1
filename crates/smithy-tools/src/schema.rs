//! Tool schema types.
//!
//! Structure follows forge's `ToolDefinition`/`ToolParameter` (which model the
//! OpenAI function-calling schema properly, including nested array/object
//! types). Serialization order is deterministic: the `tools` array a provider
//! sends must be **byte-identical every turn**, because it sits at the head of
//! the model's cached prefix and any variation forces a full cold prefill.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A single parameter in a tool's schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolParameter {
    pub name: String,
    /// JSON schema type: `string`, `integer`, `number`, `boolean`, `array`, `object`.
    pub param_type: String,
    pub description: String,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// For `array` types, the element schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<ToolParameter>>,
    /// For `object` types, the property schemas. `Vec` rather than `HashMap` so
    /// serialization order is stable — a `HashMap` would reorder between runs
    /// and silently invalidate the prefix cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<Vec<ToolParameter>>,
}

impl ToolParameter {
    pub fn new(
        name: impl Into<String>,
        param_type: impl Into<String>,
        description: impl Into<String>,
        required: bool,
    ) -> Self {
        Self {
            name: name.into(),
            param_type: param_type.into(),
            description: description.into(),
            required,
            default: None,
            items: None,
            properties: None,
        }
    }

    pub fn string(name: impl Into<String>, description: impl Into<String>, required: bool) -> Self {
        Self::new(name, "string", description, required)
    }

    pub fn integer(
        name: impl Into<String>,
        description: impl Into<String>,
        required: bool,
    ) -> Self {
        Self::new(name, "integer", description, required)
    }

    pub fn boolean(
        name: impl Into<String>,
        description: impl Into<String>,
        required: bool,
    ) -> Self {
        Self::new(name, "boolean", description, required)
    }

    pub fn with_items(mut self, items: ToolParameter) -> Self {
        self.items = Some(Box::new(items));
        self
    }

    pub fn with_properties(mut self, properties: Vec<ToolParameter>) -> Self {
        self.properties = Some(properties);
        self
    }

    /// Render this parameter as a JSON-schema property value.
    fn to_json_schema(&self) -> Value {
        let mut prop = Map::new();
        prop.insert("type".into(), Value::String(self.param_type.clone()));
        prop.insert(
            "description".into(),
            Value::String(self.description.clone()),
        );

        if let Some(items) = &self.items {
            prop.insert("items".into(), items.to_json_schema());
        }
        if let Some(props) = &self.properties {
            let mut nested = Map::new();
            let mut required = Vec::new();
            for p in props {
                nested.insert(p.name.clone(), p.to_json_schema());
                if p.required {
                    required.push(Value::String(p.name.clone()));
                }
            }
            prop.insert("properties".into(), Value::Object(nested));
            if !required.is_empty() {
                prop.insert("required".into(), Value::Array(required));
            }
        }
        Value::Object(prop)
    }
}

/// A tool as advertised to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Vec<ToolParameter>,
}

impl ToolDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Vec<ToolParameter>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    /// Serialize into an OpenAI `tools` array entry:
    /// `{"type": "function", "function": {name, description, parameters}}`.
    ///
    /// `serde_json::Map` preserves insertion order by default, so repeated calls
    /// on the same definition produce byte-identical output.
    pub fn to_openai(&self) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();

        for param in &self.parameters {
            properties.insert(param.name.clone(), param.to_json_schema());
            if param.required {
                required.push(Value::String(param.name.clone()));
            }
        }

        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": {
                    "type": "object",
                    "properties": Value::Object(properties),
                    "required": Value::Array(required),
                }
            }
        })
    }
}

/// A tool invocation requested by the model.
///
/// `arguments` is kept as the raw JSON string exactly as the model (or the XML
/// fallback parser) produced it, so a malformed payload can be reported back
/// verbatim instead of being lost to a failed deserialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    /// Parse `arguments` into a JSON object. An empty string means "no args".
    pub fn parsed_arguments(&self) -> Result<Value, String> {
        let trimmed = self.arguments.trim();
        if trimmed.is_empty() {
            return Ok(Value::Object(Map::new()));
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(v) if v.is_object() => Ok(v),
            Ok(_) => Err(format!(
                "tool_call `{}` arguments were valid JSON but not an object: {trimmed}",
                self.name
            )),
            Err(e) => Err(format!(
                "tool_call `{}` arguments were not valid JSON ({e}): {trimmed}",
                self.name
            )),
        }
    }
}

/// The outcome of running a tool.
///
/// `is_error` is advisory — it drives UI colouring and hook decisions. The
/// content is fed back to the model either way, because a tool error the model
/// can read and recover from is far more useful than a silent failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub name: String,
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: call.id.clone(),
            name: call.name.clone(),
            content: content.into(),
            is_error: false,
        }
    }

    pub fn err(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: call.id.clone(),
            name: call.name.clone(),
            content: content.into(),
            is_error: true,
        }
    }
}

/// What a [`crate::Tool`] returns before the registry attaches call identity.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

// --- argument helpers shared by every tool ---

pub fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing required string argument `{key}`"))
}

pub fn arg_str_opt<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

pub fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}

pub fn arg_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(|v| v.as_bool())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_schema_is_byte_stable() {
        let def = ToolDefinition::new(
            "read",
            "Read a file",
            vec![
                ToolParameter::string("path", "File path", true),
                ToolParameter::integer("offset", "Start line", false),
                ToolParameter::integer("limit", "Max lines", false),
            ],
        );
        let a = serde_json::to_string(&def.to_openai()).unwrap();
        let b = serde_json::to_string(&def.to_openai()).unwrap();
        assert_eq!(a, b, "schema serialization must be deterministic");
        assert!(a.contains(r#""required":["path"]"#));
    }

    #[test]
    fn parses_object_arguments() {
        let call = ToolCall::new("1", "read", r#"{"path":"a.rs"}"#);
        let v = call.parsed_arguments().unwrap();
        assert_eq!(v["path"], "a.rs");
    }

    #[test]
    fn empty_arguments_mean_no_args() {
        let call = ToolCall::new("1", "ls", "");
        assert!(call
            .parsed_arguments()
            .unwrap()
            .as_object()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn rejects_non_object_arguments() {
        let call = ToolCall::new("1", "read", "[1,2,3]");
        assert!(call
            .parsed_arguments()
            .unwrap_err()
            .contains("not an object"));
    }

    #[test]
    fn rejects_malformed_json() {
        let call = ToolCall::new("1", "read", "{not json");
        assert!(call
            .parsed_arguments()
            .unwrap_err()
            .contains("not valid JSON"));
    }
}
