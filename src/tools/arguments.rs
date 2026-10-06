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

/// An argument the schema does not accept, with the accepted name the call
/// most likely meant (`suggest::field`). `path` is the full argument path and
/// `siblings` the object holding it.
pub(super) fn unknown_field(
    name: &str,
    path: &str,
    value: &Value,
    properties: &Value,
    siblings: &Value,
) -> anyhow::Error {
    let (scope, key) = match path.rsplit_once('.') {
        Some((parent, key)) => (scope_name(parent), key),
        None => ("", path),
    };
    let fields = properties.as_object();
    let allowed: Vec<&str> = fields
        .into_iter()
        .flat_map(|fields| fields.keys())
        .map(String::as_str)
        .collect();
    // Prefer a field whose type accepts the value sent ("querys":[...]
    // means queries, not query).
    let compatible: Vec<&str> = fields
        .into_iter()
        .flatten()
        .filter(|(_, schema)| accepts_type(schema, value))
        .map(|(key, _)| key.as_str())
        .collect();
    let allowed = &allowed[..];
    let suggestion = suggest::field(name, scope, key, &compatible, siblings)
        .or_else(|| suggest::field(name, scope, key, allowed, siblings));
    let noun = if scope.is_empty() {
        "arguments"
    } else {
        "fields"
    };
    let mut data = json!({"execution":"not_started","input_error":{
        "tool":name,"field":path,"expected":{"allowed_fields":allowed},"received":value_type(value)
    }});
    if let Some(target) = suggestion.as_ref().and_then(|s| s.target.as_ref()) {
        data["input_error"]["did_you_mean"] = json!(target);
    }
    recovery::DiagnosticError {
        message: format!(
            "unknown_argument: {path} is not accepted{}; allowed {noun}: {} for {name}; correct this field and resend the intended call",
            suggestion.map_or_else(String::new, |s| s.text),
            json!(allowed)
        ),
        data,
    }
    .into()
}

fn accepts_type(schema: &Value, value: &Value) -> bool {
    match &schema["type"] {
        Value::String(kind) => matches_type(value, kind),
        Value::Array(kinds) => kinds
            .iter()
            .filter_map(Value::as_str)
            .any(|kind| matches_type(value, kind)),
        _ => true,
    }
}

/// The argument a nested path belongs to: "edits[2]" -> "edits".
fn scope_name(parent: &str) -> &str {
    parent.split(['[', '.']).next().unwrap_or(parent)
}

/// Only the vocabulary used by ToolRegistry field schemas is evaluated here.
/// Action unions are checked by their tool-specific validators. `parent` is
/// the value holding this field (the whole arguments at the top level); it
/// lets a rejected value be compared with its sibling arguments.
pub(super) fn validate_field(
    name: &str,
    path: &str,
    schema: &Value,
    value: &Value,
    parent: &Value,
) -> Result<()> {
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
        let (message, target) = enum_value_error(name, path, value, allowed, parent);
        let mut data = json!({"execution":"not_started","input_error":{
            "tool":name,"field":path,"expected":{"enum":allowed},"received":value_type(value)
        }});
        if let Some(target) = target {
            data["input_error"]["did_you_mean"] = json!(target);
        }
        return Err(recovery::DiagnosticError { message, data }.into());
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
            validate_field(
                name,
                &format!("{path}[{index}]"),
                &schema["items"],
                item,
                value,
            )?;
        }
    }
    if let Some(object) = value.as_object() {
        for required in schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !object.contains_key(required) {
                let field = format!("{path}.{required}");
                // A missing field next to an unaccepted one is usually a
                // misnamed field (file_patch "op" for "action"); name both.
                let unknown: Vec<_> = if schema["additionalProperties"] == false {
                    object
                        .keys()
                        .filter(|key| schema["properties"].get(key.as_str()).is_none())
                        .take(8)
                        .collect()
                } else {
                    vec![]
                };
                if unknown.is_empty() {
                    return Err(failure(
                        name,
                        &field,
                        "missing_argument",
                        schema["properties"][required].clone(),
                        "missing",
                        "is required",
                    ));
                }
                // One unaccepted field, or the one whose name means this
                // required field, likely carries its value.
                let accepted: Vec<&str> = schema["properties"]
                    .as_object()
                    .into_iter()
                    .flat_map(|p| p.keys())
                    .map(String::as_str)
                    .collect();
                let meant = unknown.iter().find(|key| {
                    suggest::field(name, scope_name(path), key, &accepted, value)
                        .and_then(|s| s.target)
                        .is_some_and(|target| target == required)
                });
                let rename = match (meant, unknown.as_slice()) {
                    (Some(only), _) | (None, [only]) => {
                        format!("; if {only} carries this value, send it as {required}")
                    }
                    _ => String::new(),
                };
                return Err(recovery::DiagnosticError {
                    message: format!(
                        "missing_argument: {field} is required for {name}; {path} also has fields that are not accepted: {}{rename}; correct the field names and resend the intended call",
                        json!(unknown)
                    ),
                    data: json!({"execution":"not_started","input_error":{
                        "tool":name,"field":field,"expected":schema["properties"][required],
                        "received":"missing","unknown_fields":unknown
                    }}),
                }
                .into());
            }
        }
        for (key, item) in object {
            let child_path = format!("{path}.{key}");
            if let Some(field) = schema["properties"].get(key) {
                validate_field(name, &child_path, field, item, value)?;
            } else if schema["additionalProperties"] == false {
                return Err(unknown_field(
                    name,
                    &child_path,
                    item,
                    &schema["properties"],
                    value,
                ));
            } else if schema["additionalProperties"].is_object() {
                validate_field(
                    name,
                    &child_path,
                    &schema["additionalProperties"],
                    item,
                    value,
                )?;
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

/// Name the field a prose error is about ("missing_argument: ids for ...")
/// when it is a declared or received argument path and the sentence treats it
/// as the subject. Otherwise the whole argument object stays the subject; a
/// guessed field ("query mode requires ...") would mislead repair.
fn named_field(message: &str, parameters: &Value, args: &Value) -> (String, Option<Value>) {
    let (code, rest) = message.split_once(':').unwrap_or((message, ""));
    let rest = rest.trim_start();
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '[' | ']'))
        .collect();
    let token = token.trim_end_matches('.');
    let follower = &rest[token.len()..];
    let subject = follower.is_empty()
        || [":", ";", " is ", " must ", " for ", " \""]
            .iter()
            .any(|next| follower.starts_with(next));
    let top = token.split(['.', '[']).next().unwrap_or("");
    let known =
        !top.is_empty() && (parameters["properties"].get(top).is_some() || args.get(top).is_some());
    if !subject || !known {
        return ("arguments".into(), None);
    }
    let expected = (code == "missing_argument" && token == top)
        .then(|| parameters["properties"].get(top).cloned())
        .flatten();
    (token.to_owned(), expected)
}

/// Field structure without string contents, so the arguments can be moved
/// into the tool and a runtime input error can still name the field type.
pub(super) fn shape(value: &Value) -> Value {
    match value {
        Value::String(_) => json!(""),
        Value::Array(items) => Value::Array(items.iter().take(32).map(shape).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .take(64)
                .map(|(key, value)| (key.clone(), shape(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Input errors found while a tool runs keep their prose and gain the field
/// diagnosis validator errors carry. They are not labelled unexecuted: only a
/// read-only tool or an explicit "no changes" statement proves nothing changed.
pub(super) fn annotate_runtime(error: anyhow::Error, name: &str, shape: &Value) -> anyhow::Error {
    let message = format!("{error:#}");
    if error.downcast_ref::<recovery::DiagnosticError>().is_some()
        || recovery::describe(&message)["class"] != "invalid_input"
    {
        return error;
    }
    let Some(spec) = ToolRegistry::specs()
        .into_iter()
        .find(|spec| spec.name == name)
    else {
        return error;
    };
    let (field, expected) = named_field(&message, &spec.parameters, shape);
    if field == "arguments" {
        return error;
    }
    let mut data = json!({"input_error":{
        "tool":name,"field":field,"received":lookup(shape, &field).map_or("missing", value_type)
    }});
    if let Some(expected) = expected {
        data["input_error"]["expected"] = expected;
    }
    if spec.read_only
        || ["no changes persisted", "state unchanged", "task preserved"]
            .iter()
            .any(|claim| message.contains(claim))
    {
        data["execution"] = json!("rejected_without_changes");
    }
    recovery::DiagnosticError { message, data }.into()
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
    // The compact form drops the tool name when the execution state already
    // marks an input diagnosis; recognize either form on a later, smaller
    // budget. Ordinary results that only describe input (task_plan) carry
    // neither marker and keep their own data.
    let execution = data["execution"].as_str();
    let marked = matches!(execution, Some("not_started" | "rejected_without_changes"))
        || input["tool"].is_string();
    (marked && input["field"].is_string()).then(|| {
        let mut compact = json!({"input_error":{
            "field":input["field"],"received":input["received"]
        }});
        match execution {
            Some(state) => compact["execution"] = json!(state),
            None => compact["input_error"]["tool"] = input["tool"].clone(),
        }
        if !input["expected"].is_null() {
            compact["input_error"]["expected"] = input["expected"].clone();
        }
        if input["did_you_mean"].is_string() {
            compact["input_error"]["did_you_mean"] = input["did_you_mean"].clone();
        }
        compact
    })
}
