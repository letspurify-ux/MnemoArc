//! Memory-specific diagnostics. Recovery explains a correction, but never
//! guesses content/source IDs, refreshes references, or retries a write.
use super::*;
use crate::memory::MemoryRevisionConflict;
use recovery::DiagnosticError;

fn input<'a>(name: &str, args: &'a Value) -> Option<(&'static str, &'a Value)> {
    match name {
        "memory_write" => Some(("arguments", args)),
        "memory_manage" if args["action"] == "replace" => {
            Some(("replacement", &args["replacement"]))
        }
        _ => None,
    }
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn native_markup(field: &str, value: &Value) -> bool {
    // Body/metadata may legitimately document the provider's markup. Detect
    // protocol-shaped field names and leaked parameter boundaries in header
    // fields, rather than rejecting arbitrary XML in reusable content.
    field.contains("</arg_key>")
        || field.contains("<arg_key>")
        || field.starts_with("parameter name=")
        || field.contains("<parameter")
        || (matches!(field, "title" | "summary" | "kind")
            && value.as_str().is_some_and(|text| {
                (text.contains("<parameter name=") || text.contains("</parameter>"))
                    && (text.contains("</summary>")
                        || text.contains("</title>")
                        || text.contains("</description>"))
            }))
}

fn diagnostics(name: &str, args: &Value) -> Option<Value> {
    let (field, value) = input(name, args)?;
    let schema = memory_input_schema();
    let properties = schema["properties"].as_object().unwrap();
    let object = value.as_object();
    let mut received: Vec<_> = object.into_iter().flat_map(|o| o.keys()).collect();
    // Dependency features can change JSON map storage; diagnostics retain
    // their original alphabetical field order either way.
    received.sort_unstable();
    let unknown: Vec<_> = received
        .iter()
        .filter(|key| !properties.contains_key(**key))
        .collect();
    let missing: Vec<_> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|key| value.get(key.as_str().unwrap()).is_none())
        .collect();
    let mut invalid = Vec::new();
    if let Some(object) = object {
        for &key in &received {
            let value = &object[key];
            let Some(spec) = properties.get(key) else {
                continue;
            };
            let valid = match spec["type"].as_str() {
                Some("string") => value.is_string(),
                Some("integer") => value.as_u64().is_some(),
                Some("boolean") => value.is_boolean(),
                Some("array") => value
                    .as_array()
                    .is_some_and(|a| a.iter().all(Value::is_string)),
                _ => true,
            };
            if !valid {
                invalid.push(
                    json!({"field":key,"expected":spec["type"],"received":value_type(value)}),
                );
            } else if let Some(values) = spec["enum"].as_array()
                && !values.contains(value)
            {
                invalid.push(json!({"field":key,"allowed":values}));
            } else if matches!(key.as_str(), "key" | "title" | "summary" | "body")
                && value.as_str().is_some_and(|v| v.trim().is_empty())
            {
                invalid.push(json!({"field":key,"expected":"non-empty string"}));
            }
        }
    }
    // Do not echo long bodies or arbitrary field names into an error result.
    let bounded = |keys: Vec<&String>| {
        keys.into_iter()
            .take(24)
            .map(|key| key.chars().take(80).collect::<String>())
            .collect::<Vec<_>>()
    };
    let markup = object.is_some_and(|o| o.iter().any(|(k, v)| native_markup(k, v)));
    Some(json!({
        "field":field,"received_type":value_type(value),
        "received_fields":bounded(received.clone()),"received_field_count":received.len(),
        "required_fields":schema["required"],"missing_fields":missing,
        "unknown_fields":bounded(unknown.iter().map(|key| **key).collect()),
        "unknown_field_count":unknown.len(),"invalid_fields":invalid,
        "native_markup":markup
    }))
}

fn argument_failure(name: &str, args: &Value, message: String, diagnostic: Value) -> anyhow::Error {
    // This is a complete shape example, not content to save. In particular,
    // source IDs and kinds must be copied from the intended, observed claim.
    let example = json!({"title":"<intended title>","summary":"<intended summary>","body":"<intended body>","kind":"fact","source_ids":["<copy an observed source ID>"]});
    let example = if name == "memory_manage" {
        json!({"action":"replace","ids":args["ids"],"replacement":example})
    } else {
        example
    };
    DiagnosticError {
        message,
        data:json!({"input_error":diagnostic,"correction":{
            "tool":name,"example_only":true,"arguments":example,
            "guidance":"Resend every intended field as plain JSON, without native parameter/arg_key tags. Replace example placeholders with the intended values and actual kind. Never guess item/title/kind/source IDs or remove observed source_ids to pass validation. Keep a reused key and its expected_revision."
        }}),
    }.into()
}

pub(super) fn argument_error(error: anyhow::Error, name: &str, args: &Value) -> anyhow::Error {
    let message = error.to_string();
    if (message.starts_with("invalid_")
        || message.starts_with("missing_argument:")
        || message.starts_with("unknown_argument:")
        || message.starts_with("conflicting_arguments:"))
        && let Some(mut diagnostic) = diagnostics(name, args)
    {
        // Keep the precise shared-schema violation, including errors outside
        // replacement (e.g. ids[0]) and numeric bounds. The memory shape alone
        // can be valid even though the surrounding call failed validation.
        if (message.starts_with("invalid_argument_type:")
            || message.starts_with("invalid_argument_value:"))
            && let Some(details) = error.downcast_ref::<DiagnosticError>()
            && !details.data["input_error"]["expected"].is_null()
        {
            let input = &details.data["input_error"];
            let mut invalid = diagnostic["invalid_fields"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            invalid.retain(|field| field["field"] != input["field"]);
            invalid.insert(
                0,
                json!({
                    "field":input["field"],"expected":input["expected"],"received":input["received"]
                }),
            );
            diagnostic["invalid_fields"] = json!(invalid);
        }
        return argument_failure(name, args, message, diagnostic);
    }
    error
}

pub(super) fn validate_input(name: &str, args: &Value) -> Result<()> {
    let Some(diagnostic) = diagnostics(name, args) else {
        return Ok(());
    };
    let field = diagnostic["field"].as_str().unwrap();
    let code = if diagnostic["received_type"] != "object" {
        Some("invalid_argument_type")
    } else if diagnostic["unknown_field_count"].as_u64().unwrap() > 0 {
        Some("unknown_argument")
    } else if !diagnostic["missing_fields"].as_array().unwrap().is_empty() {
        Some("missing_argument")
    } else if !diagnostic["invalid_fields"].as_array().unwrap().is_empty() {
        Some("invalid_argument_value")
    } else if diagnostic["native_markup"] == true {
        Some("invalid_memory_markup")
    } else {
        None
    };
    if let Some(code) = code {
        return Err(argument_failure(
            name,
            args,
            format!(
                "{code}: {name} {field} failed validation; inspect input_error and resend complete plain JSON"
            ),
            diagnostic,
        ));
    }
    Ok(())
}

pub(super) fn revision_error(error: anyhow::Error, name: &str) -> anyhow::Error {
    let Some(conflict) = error.downcast_ref::<MemoryRevisionConflict>() else {
        return error;
    };
    let memory = &conflict.memory;
    let patch = if name == "memory_manage" {
        json!({"replacement":{"expected_revision":memory.revision}})
    } else {
        json!({"expected_revision":memory.revision})
    };
    DiagnosticError {
        message:error.to_string(),
        data:json!({
            "memory":{"id":memory.id,"key":memory.key,"status":memory.status,"current_revision":memory.revision,"expected_revision":conflict.expected_revision},
            "required_field":if name == "memory_manage" { "replacement.expected_revision" } else { "expected_revision" },
            "refresh_call":{"tool":"memory_read","arguments":{"id":memory.id}},
            "retry":{"tool":name,"argument_patch":patch,"merge_fields":true,"review_required":true,
                "guidance":"Review the current memory, then merge this revision into the original intended arguments (including all replacement fields for memory_manage). Resupply observed source_ids; this patch alone is not a write. If the memory changes again, refresh its revision again."}
        }),
    }.into()
}

pub(super) fn compact_data(name: &str, data: &Value) -> Option<Value> {
    if !matches!(name, "memory_write" | "memory_manage") {
        return None;
    }
    if data["memory"].is_object() {
        let mut memory = data["memory"].clone();
        // A long user key is still recoverable from the archive; the canonical
        // ID remains usable for the refresh call without inventing a short key.
        if memory["key"].as_str().is_some_and(|key| key.len() > 80) {
            memory.as_object_mut().unwrap().remove("key");
        }
        return Some(json!({"memory":memory,"required_field":data["required_field"]}));
    }
    if let Some(input) = data.get("input_error") {
        let mut compact = json!({});
        for field in ["field", "missing_fields", "unknown_fields", "native_markup"] {
            compact[field] = input[field].clone();
        }
        if input["unknown_field_count"]
            .as_u64()
            .is_some_and(|n| n > 24)
        {
            compact["unknown_field_count"] = input["unknown_field_count"].clone();
        }
        if !input["invalid_fields"].as_array().is_none_or(Vec::is_empty) {
            compact["invalid_fields"] = json!(
                input["invalid_fields"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .take(2)
                    .collect::<Vec<_>>()
            );
        }
        return Some(json!({"input_error":compact}));
    }
    if let Some(issues) = data["memory_issues"].as_array() {
        let issues: Vec<Value> = issues
            .iter()
            .take(1)
            .map(|issue| {
                let mut issue = issue.clone();
                issue.as_object_mut().unwrap().remove("read_call");
                issue.as_object_mut().unwrap().remove("key");
                issue
            })
            .collect();
        return Some(
            json!({"item_id":data["item_id"],"memory_issue_count":data["memory_issue_count"],"memory_issues":issues}),
        );
    }
    None
}
