//! Validate the field schemas shared with the model, including nested values.
//! Stateful preconditions and independently applied batches stay with their tools.
use super::*;

pub(super) fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub(super) fn failure(
    name: &str,
    field: &str,
    code: &str,
    expected: Value,
    received: &str,
    detail: &str,
) -> anyhow::Error {
    recovery::DiagnosticError {
        message: format!(
            "{code}: {field} {detail} for {name}; correct this field and resend the intended call"
        ),
        data: json!({"execution":"not_started","input_error":{
            "tool":name,"field":field,"expected":expected,"received":received
        }}),
    }
    .into()
}

fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "null" => value.is_null(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => unreachable!("unsupported tool schema type: {kind}"),
    }
}

/// Only the vocabulary used by ToolRegistry field schemas is evaluated here.
/// Action unions are checked by their tool-specific validators.
pub(super) fn validate_field(name: &str, path: &str, schema: &Value, value: &Value) -> Result<()> {
    let kinds: Vec<_> = match &schema["type"] {
        Value::String(kind) => vec![kind.as_str()],
        Value::Array(kinds) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    if !kinds.is_empty() && !kinds.iter().any(|kind| matches_type(value, kind)) {
        let expected = kinds.join(" or ");
        let hint = if value.is_string() && (kinds.contains(&"array") || kinds.contains(&"object")) {
            "; send structured JSON, not text containing JSON; object keys need double quotes"
        } else {
            ""
        };
        return Err(failure(
            name,
            path,
            "invalid_argument_type",
            json!(expected),
            value_type(value),
            &format!("must be {expected}, got {}{hint}", value_type(value)),
        ));
    }
    if let Some(allowed) = schema["enum"].as_array()
        && !allowed.contains(value)
    {
        return Err(recovery::DiagnosticError {
            message: enum_value_error(name, path, value, allowed),
            data: json!({"execution":"not_started","input_error":{
                "tool":name,"field":path,"expected":{"enum":allowed},"received":value_type(value)
            }}),
        }
        .into());
    }
    let integer = value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from));
    for (key, actual, lower) in [
        ("minimum", integer, true),
        ("maximum", integer, false),
        (
            "minLength",
            value.as_str().map(|v| v.chars().count() as i128),
            true,
        ),
        (
            "maxLength",
            value.as_str().map(|v| v.chars().count() as i128),
            false,
        ),
        ("minItems", value.as_array().map(|v| v.len() as i128), true),
        ("maxItems", value.as_array().map(|v| v.len() as i128), false),
        (
            "minProperties",
            value.as_object().map(|v| v.len() as i128),
            true,
        ),
        (
            "maxProperties",
            value.as_object().map(|v| v.len() as i128),
            false,
        ),
    ] {
        if let (Some(bound), Some(actual)) = (schema[key].as_u64().map(i128::from), actual)
            && (if lower {
                actual < bound
            } else {
                actual > bound
            })
        {
            return Err(failure(
                name,
                path,
                if name == "symbol_read" && matches!(path, "start_line" | "max_lines") {
                    "invalid_symbol_range"
                } else {
                    "invalid_argument_value"
                },
                json!({key:bound}),
                value_type(value),
                &format!("violates {key}={bound}; received {actual}"),
            ));
        }
    }
    if let Some(items) = value.as_array() {
        for (index, item) in items.iter().enumerate() {
            validate_field(name, &format!("{path}[{index}]"), &schema["items"], item)?;
        }
    }
    if let Some(object) = value.as_object() {
        // Each investigation item is independently validated during execution;
        // rejecting the entire object here would discard successful siblings.
        if name == "investigation" && path == "items" {
            return Ok(());
        }
        for required in schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !object.contains_key(required) {
                return Err(failure(
                    name,
                    &format!("{path}.{required}"),
                    "missing_argument",
                    schema["properties"][required].clone(),
                    "missing",
                    "is required",
                ));
            }
        }
        for (key, item) in object {
            let child_path = format!("{path}.{key}");
            if let Some(field) = schema["properties"].get(key) {
                validate_field(name, &child_path, field, item)?;
            } else if schema["additionalProperties"] == false {
                let allowed: Vec<_> = schema["properties"]
                    .as_object()
                    .into_iter()
                    .flat_map(|p| p.keys())
                    .collect();
                return Err(failure(
                    name,
                    &child_path,
                    "unknown_argument",
                    json!({"allowed_fields":allowed}),
                    value_type(item),
                    &format!("is not accepted; allowed fields: {}", json!(allowed)),
                ));
            } else if schema["additionalProperties"].is_object() {
                validate_field(name, &child_path, &schema["additionalProperties"], item)?;
            }
        }
    }
    Ok(())
}

/// Keep specialized explanations, adding the call shape when they only carry
/// prose. Never label runtime failures as unexecuted or overwrite typed data.
pub(super) fn annotate(error: anyhow::Error, name: &str, args: &Value) -> anyhow::Error {
    if error.downcast_ref::<recovery::DiagnosticError>().is_some()
        || recovery::describe(&error.to_string())["class"] != "invalid_input"
    {
        return error;
    }
    let Some(spec) = ToolRegistry::specs()
        .into_iter()
        .find(|spec| spec.name == name)
    else {
        return error;
    };
    let allowed: Vec<_> = spec.parameters["properties"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    let received: Vec<_> = args
        .as_object()
        .into_iter()
        .flat_map(|args| args.keys())
        .take(32)
        .map(|key| key.chars().take(80).collect::<String>())
        .collect();
    let message = error.to_string();
    let (field, expected) = named_field(&message, &spec.parameters, args);
    let mut data = json!({"execution":"not_started","input_error":{
        "tool":name,"field":field,"received":value_type(args),
        "received_fields":received,"required_fields":spec.parameters["required"],"allowed_fields":allowed,
        "guidance":"Follow the error's action-specific requirements; preserve other intended arguments. No tool operation was executed."
    }});
    if field != "arguments" {
        data["input_error"]["received"] = json!(lookup(args, &field).map_or("missing", value_type));
    }
    if let Some(expected) = expected {
        data["input_error"]["expected"] = expected;
    }
    recovery::DiagnosticError { message, data }.into()
}

/// Name the field a prose validator rejected ("missing_argument: ids for ...")
/// when it is a declared or received argument path. Otherwise the whole
/// argument object stays the subject; a guessed field would mislead repair.
fn named_field(message: &str, parameters: &Value, args: &Value) -> (String, Option<Value>) {
    let (code, rest) = message.split_once(':').unwrap_or((message, ""));
    let token: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '[' | ']'))
        .collect();
    let token = token.trim_end_matches('.');
    let top = token.split(['.', '[']).next().unwrap_or("");
    let known =
        !top.is_empty() && (parameters["properties"].get(top).is_some() || args.get(top).is_some());
    if !matches!(
        code,
        "missing_argument"
            | "unknown_argument"
            | "invalid_argument_value"
            | "invalid_argument_type"
    ) || !known
    {
        return ("arguments".into(), None);
    }
    let expected = (code == "missing_argument" && token == top)
        .then(|| parameters["properties"].get(top).cloned())
        .flatten();
    (token.to_owned(), expected)
}

fn lookup<'a>(args: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = args;
    for part in path.split('.') {
        let (key, indexes) = part.split_once('[').unwrap_or((part, ""));
        if !key.is_empty() {
            current = current.get(key)?;
        }
        for index in indexes.split('[').filter(|i| !i.is_empty()) {
            current = current.get(index.trim_end_matches(']').parse::<usize>().ok()?)?;
        }
    }
    Some(current)
}

pub(super) fn compact_data(data: &Value) -> Option<Value> {
    let input = &data["input_error"];
    // The compact form deliberately omits the tool name. Recognize it again
    // when the agent applies its smaller per-batch budget to the same result.
    (data["execution"] == "not_started" && input["field"].is_string()).then(|| {
        let mut compact = json!({"execution":"not_started","input_error":{
            "field":input["field"],"received":input["received"]
        }});
        if !input["expected"].is_null() {
            compact["input_error"]["expected"] = input["expected"].clone();
        }
        compact
    })
}
