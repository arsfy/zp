//! Lenient validation of tool arguments against a JSON Schema subset.
//!
//! Supported keywords: `type` (string or list), `properties`, `required`,
//! `additionalProperties` (schema form), `items`, `enum`, `const`, `anyOf` and `oneOf`
//! (first match wins). Other keywords are ignored.
//!
//! Models often send scalars as strings or nested objects as JSON text, so values are
//! coerced when the schema asks for another type and the conversion is lossless:
//! `"5"` → `5`, `"true"` → `true`, `3` → `"3"`, `"{\"a\":1}"` → `{"a":1}`.

use serde_json::{Map, Value};

/// Validates `args` against `schema`, returning the (possibly coerced) arguments or a
/// list of problems, one per line.
pub fn validate_arguments(schema: &Value, args: Value) -> Result<Value, String> {
    // Tools without parameters are often called with no arguments at all.
    let args = match args {
        Value::Null if expects(schema, "object") => Value::Object(Map::new()),
        args => args,
    };
    let mut errors = Vec::new();
    let value = check(schema, args, "", &mut errors);
    if errors.is_empty() {
        Ok(value)
    } else {
        Err(errors.join("\n"))
    }
}

fn expects(schema: &Value, kind: &str) -> bool {
    types(schema).is_some_and(|types| types.contains(&kind))
}

fn types(schema: &Value) -> Option<Vec<&str>> {
    match schema.get("type")? {
        Value::String(kind) => Some(vec![kind.as_str()]),
        Value::Array(kinds) => Some(kinds.iter().filter_map(Value::as_str).collect()),
        _ => None,
    }
}

fn display(path: &str) -> String {
    if path.is_empty() {
        "arguments".to_owned()
    } else {
        path.to_owned()
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "number" => value.is_number(),
        "integer" => match value {
            Value::Number(n) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            _ => false,
        },
        _ => true,
    }
}

fn coerce(value: &Value, kind: &str) -> Option<Value> {
    match (kind, value) {
        ("integer", Value::String(s)) => s.trim().parse::<i64>().ok().map(Value::from),
        ("integer", Value::Number(n)) => {
            let f = n.as_f64()?;
            (f.fract() == 0.0 && f.abs() < 9.0e15).then(|| Value::from(f as i64))
        }
        ("number", Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .and_then(|f| serde_json::Number::from_f64(f).map(Value::Number)),
        ("boolean", Value::String(s)) => match s.trim() {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        ("string", Value::Number(n)) => Some(Value::String(n.to_string())),
        ("string", Value::Bool(b)) => Some(Value::String(b.to_string())),
        ("object" | "array", Value::String(s)) => serde_json::from_str::<Value>(s)
            .ok()
            .filter(|parsed| matches_type(parsed, kind)),
        ("null", Value::String(s)) if s.trim() == "null" => Some(Value::Null),
        _ => None,
    }
}

fn check(schema: &Value, value: Value, path: &str, errors: &mut Vec<String>) -> Value {
    let Some(rules) = schema.as_object() else {
        return value;
    };

    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(options)) = rules.get(key) {
            for option in options {
                let mut option_errors = Vec::new();
                let candidate = check(option, value.clone(), path, &mut option_errors);
                if option_errors.is_empty() {
                    return candidate;
                }
            }
            errors.push(format!(
                "{}: does not match any allowed schema",
                display(path)
            ));
            return value;
        }
    }

    let mut value = value;
    if let Some(kinds) = types(schema)
        && !kinds.iter().any(|kind| matches_type(&value, kind))
    {
        match kinds.iter().find_map(|kind| coerce(&value, kind)) {
            Some(coerced) => value = coerced,
            None => {
                errors.push(format!(
                    "{}: expected {}, got {}",
                    display(path),
                    kinds.join(" or "),
                    type_name(&value)
                ));
                return value;
            }
        }
    }
    if expects(schema, "integer")
        && !expects(schema, "number")
        && value.is_f64()
        && let Some(integer) = coerce(&value, "integer")
    {
        // `5.0` → `5`
        value = integer;
    }

    if let Some(Value::Array(allowed)) = rules.get("enum")
        && !allowed.contains(&value)
    {
        let allowed: Vec<String> = allowed.iter().map(Value::to_string).collect();
        errors.push(format!(
            "{}: must be one of {}",
            display(path),
            allowed.join(", ")
        ));
    }
    if let Some(expected) = rules.get("const")
        && expected != &value
    {
        errors.push(format!("{}: must be {expected}", display(path)));
    }

    match value {
        Value::Object(mut map) => {
            if let Some(Value::Array(required)) = rules.get("required") {
                for name in required.iter().filter_map(Value::as_str) {
                    if !map.contains_key(name) {
                        errors.push(format!("{}: missing required property", join(path, name)));
                    }
                }
            }
            let properties = rules.get("properties").and_then(Value::as_object);
            let additional = rules
                .get("additionalProperties")
                .filter(|schema| schema.is_object());
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                let sub = properties.and_then(|p| p.get(&key)).or(additional);
                if let Some(sub) = sub
                    && let Some(item) = map.remove(&key)
                {
                    let checked = check(sub, item, &join(path, &key), errors);
                    map.insert(key, checked);
                }
            }
            Value::Object(map)
        }
        Value::Array(items) => match rules.get("items") {
            Some(item_schema) => Value::Array(
                items
                    .into_iter()
                    .enumerate()
                    .map(|(i, item)| {
                        check(
                            item_schema,
                            item,
                            &format!("{}[{i}]", display(path)),
                            errors,
                        )
                    })
                    .collect(),
            ),
            None => Value::Array(items),
        },
        value => value,
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer"},
                "ratio": {"type": "number"},
                "force": {"type": "boolean"},
                "mode": {"type": "string", "enum": ["a", "b"]},
                "edits": {"type": "array", "items": {
                    "type": "object",
                    "properties": {"old": {"type": "string"}, "new": {"type": "string"}},
                    "required": ["old", "new"]
                }},
                "target": {"anyOf": [{"type": "integer"}, {"type": "string", "enum": ["all"]}]}
            },
            "required": ["path"]
        })
    }

    #[test]
    fn accepts_and_coerces() {
        let value = validate_arguments(
            &schema(),
            json!({
                "path": 42,
                "limit": "10",
                "ratio": "0.5",
                "force": "true",
                "mode": "b",
                "edits": "[{\"old\": \"x\", \"new\": \"y\"}]",
                "target": "all",
                "extra": "kept"
            }),
        )
        .unwrap();
        assert_eq!(
            value,
            json!({
                "path": "42", "limit": 10, "ratio": 0.5, "force": true, "mode": "b",
                "edits": [{"old": "x", "new": "y"}], "target": "all", "extra": "kept"
            })
        );
        assert_eq!(
            validate_arguments(&schema(), json!({"path": "p", "limit": 3.0})).unwrap()["limit"],
            json!(3)
        );
    }

    #[test]
    fn reports_every_problem_with_paths() {
        let error = validate_arguments(
            &schema(),
            json!({
                "limit": "many",
                "mode": "c",
                "edits": [{"old": "x"}],
                "target": 1.5
            }),
        )
        .unwrap_err();
        let lines: Vec<&str> = error.lines().collect();
        assert!(
            lines.contains(&"path: missing required property"),
            "{error}"
        );
        assert!(
            lines.contains(&"limit: expected integer, got string"),
            "{error}"
        );
        assert!(
            lines.contains(&"mode: must be one of \"a\", \"b\""),
            "{error}"
        );
        assert!(
            lines.contains(&"edits[0].new: missing required property"),
            "{error}"
        );
        assert!(
            lines.contains(&"target: does not match any allowed schema"),
            "{error}"
        );
    }

    #[test]
    fn root_handling() {
        let empty = json!({"type": "object", "properties": {}});
        assert_eq!(validate_arguments(&empty, Value::Null).unwrap(), json!({}));
        assert_eq!(
            validate_arguments(&schema(), json!("{\"path\": \"a\"}")).unwrap(),
            json!({"path": "a"})
        );
        assert_eq!(
            validate_arguments(&schema(), json!([1])).unwrap_err(),
            "arguments: expected object, got array"
        );
        // Schemas that say nothing accept anything.
        assert_eq!(validate_arguments(&json!({}), json!(1)).unwrap(), json!(1));
    }
}
