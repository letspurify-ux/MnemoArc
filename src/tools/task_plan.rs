//! Ordered, bounded plans. Recoverable plan conflicts are no-op results, so
//! capacity or stale revisions never terminate the owner's running task.
use super::*;
use crate::session::TodoItem;
use serde::Deserialize;

pub const MAX_PENDING: usize = 100;
pub const MAX_COMPLETED: usize = 5;
const DEFAULT_PAGE: usize = 10;
const MAX_PAGE: usize = 20;
const MAX_TEXT_CHARS: usize = 160;
const MAX_RESULT_CHARS: usize = 240;
const MAX_OPERATIONS: usize = 16;
const MAX_ENCODED_BYTES: usize = 128 * 1024;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Insert {
        texts: Vec<String>,
        before: Option<String>,
    },
    Update {
        id: String,
        text: String,
    },
    Split {
        id: String,
        texts: Vec<String>,
    },
    Move {
        id: String,
        before: Option<String>,
    },
    Remove {
        id: String,
        reason: String,
    },
    Complete {
        id: String,
        result: String,
    },
    Reopen {
        id: String,
        reason: String,
    },
}

pub fn spec() -> ToolSpec {
    let id = json!({"type":"string","minLength":1});
    let short = json!({"type":"string","minLength":1,"maxLength":MAX_TEXT_CHARS});
    let note = json!({"type":"string","minLength":1,"maxLength":MAX_RESULT_CHARS});
    let mut parameters = schema(
        json!({
            "action":action(&["list","apply"]),
            "offset":{"type":"integer","minimum":0,"description":"list only; zero-based item offset, including retained completed items"},
            "limit":{"type":"integer","minimum":1,"maximum":MAX_PAGE,"description":"list only; page size (default 10, maximum 20)"},
            "expected_revision":{"type":"integer","minimum":0,"description":"Required for apply. Copy plan_revision from task state or revision from task_plan list."},
            "operations":{
                "type":"array","minItems":1,"maxItems":MAX_OPERATIONS,
                "description":"Required for apply: a JSON array of operation objects, NOT a JSON string or an object keyed by item ID. Example: [{\"op\":\"insert\",\"texts\":[\"Write the requested section\"]}]",
                // Keep the item type and discriminator explicit even for
                // providers that handle nested union schemas poorly. Rust
                // still enforces each operation's required/allowed fields.
                "items":schema(json!({
                    "op":action(&["insert","update","split","move","remove","complete","reopen"]),
                    "texts":{"type":"array","minItems":1,"maxItems":MAX_PENDING,"items":short,"description":"insert or split; split needs at least two smaller outcomes in execution order"},
                    "before":id,"id":id,"text":short,"result":note,"reason":note
                }), &["op"])
            }
        }),
        &["action"],
    );
    parameters["oneOf"] = json!([
        {"properties":{"action":{"const":"list"}},"required":["action"],"not":{"anyOf":[{"required":["expected_revision"]},{"required":["operations"]}]}},
        {"properties":{"action":{"const":"apply"}},"required":["action","expected_revision","operations"],"not":{"anyOf":[{"required":["offset"]},{"required":["limit"]}]}}
    ]);
    ToolSpec {
        name: "task_plan",
        description: "Manage an ordered to-do list. Read with {\"action\":\"list\"}; use offset/limit to page through the full list (default 10, maximum 20). Change with {\"action\":\"apply\",\"expected_revision\":0,\"operations\":[{\"op\":\"insert\",\"texts\":[\"Write the requested section\"]}]}; copy the actual revision, and keep operations as an array, not quoted JSON. Applies 1..16 operations atomically. insert requires texts; optional before is a pending ID (omit to append). update requires id/text. split requires an unfinished id and 2..100 smaller texts in execution order; it keeps the original ID for the first subtask and creates IDs for the rest. Split a broad item only when its parts together preserve the original outcome. move requires id and optional before. complete requires the CURRENT item's id/result after actually doing the work. remove and reopen require id/reason; reopen puts a completed item before current. Keep at most 100 unfinished small concrete outcomes; split investigation and saving when each is a meaningful result, and move promptly to writing. Retain only the latest 5 completed items and cumulative completed_total. Input/transition/capacity conflicts return applied=false without stopping; follow the correction or continue current work. Plan edits are not actual progress. Preserve user requirements in task_state.completion.",
        optional: false,
        read_only: false,
        parameters,
    }
}

pub fn view(task: &TaskState, offset: usize, limit: usize) -> Value {
    let limit = limit.clamp(1, MAX_PAGE);
    let items: Vec<_> = task.todos.iter().skip(offset).take(limit).collect();
    let end = offset.saturating_add(items.len());
    json!({
        "revision":task.plan_revision,"items":items,"current":task.current_todo(),
        "offset":offset,"total_items":task.todos.len(),
        "next_offset":(end < task.todos.len()).then_some(end),
        "pending_count":task.todos.iter().filter(|item| !item.done).count(),
        "completed_total":task.todos_completed_total,
        "limits":{"pending":MAX_PENDING,"retained_completed":MAX_COMPLETED}
    })
}

fn nonempty(value: &str, limit: usize) -> Result<&str> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > limit {
        bail!("Use nonempty text of at most {limit} characters");
    }
    Ok(value)
}

fn index(task: &TaskState, id: &str) -> Result<usize> {
    task.todos
        .iter()
        .position(|item| item.id == id)
        .ok_or_else(|| anyhow::anyhow!("Unknown item {id}; copy IDs from the returned plan"))
}

fn position(task: &TaskState, before: Option<&str>) -> Result<usize> {
    match before {
        None => Ok(task.todos.len()),
        Some(id) => {
            let at = index(task, id)?;
            if task.todos[at].done {
                bail!("Insert or move before an unfinished item; completed history stays fixed");
            }
            Ok(at)
        }
    }
}

fn unique(task: &TaskState, text: &str, except: Option<&str>) -> Result<()> {
    if task
        .todos
        .iter()
        .any(|item| Some(item.id.as_str()) != except && item.text == text)
    {
        bail!(
            "This item already exists; continue it or explicitly reopen it instead of duplicating work"
        );
    }
    Ok(())
}

fn apply(task: &mut TaskState, operation: Operation) -> Result<()> {
    match operation {
        Operation::Insert { texts, before } => {
            if texts.is_empty() || texts.len() > MAX_PENDING {
                bail!("Insert between 1 and {MAX_PENDING} concise items");
            }
            let mut at = position(task, before.as_deref())?;
            for text in texts {
                let text = nonempty(&text, MAX_TEXT_CHARS)?;
                unique(task, text, None)?;
                task.todo_sequence = task
                    .todo_sequence
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("Plan ID capacity reached"))?;
                task.todos.insert(
                    at,
                    TodoItem {
                        id: format!("T{}", task.todo_sequence),
                        text: text.into(),
                        done: false,
                        result: String::new(),
                    },
                );
                at += 1;
            }
        }
        Operation::Update { id, text } => {
            let at = index(task, &id)?;
            if task.todos[at].done {
                bail!("Reopen a completed item before changing its work");
            }
            let text = nonempty(&text, MAX_TEXT_CHARS)?;
            unique(task, text, Some(&id))?;
            task.todos[at].text = text.into();
        }
        Operation::Split { id, texts } => {
            let at = index(task, &id)?;
            if task.todos[at].done {
                bail!("Reopen a completed item before splitting it");
            }
            if !(2..=MAX_PENDING).contains(&texts.len()) {
                bail!("Split into 2..{MAX_PENDING} smaller unfinished outcomes");
            }
            let original = task.todos[at].text.clone();
            let mut parts = Vec::with_capacity(texts.len());
            let mut seen = std::collections::BTreeSet::new();
            for text in texts {
                let text = nonempty(&text, MAX_TEXT_CHARS)?;
                if text == original.as_str() {
                    bail!("Each split item must be more specific than the original item");
                }
                unique(task, text, Some(&id))?;
                if !seen.insert(text.to_string()) {
                    bail!("Split items must have distinct descriptions");
                }
                parts.push(text.to_string());
            }
            task.todos[at].text = parts.remove(0);
            for (offset, text) in parts.into_iter().enumerate() {
                task.todo_sequence = task
                    .todo_sequence
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("Plan ID capacity reached"))?;
                task.todos.insert(
                    at + offset + 1,
                    TodoItem {
                        id: format!("T{}", task.todo_sequence),
                        text,
                        done: false,
                        result: String::new(),
                    },
                );
            }
        }
        Operation::Move { id, before } => {
            let at = index(task, &id)?;
            if task.todos[at].done {
                bail!("Only unfinished items can move");
            }
            if before.as_deref() == Some(&id) {
                return Ok(());
            }
            let item = task.todos.remove(at);
            let to = position(task, before.as_deref())?;
            task.todos.insert(to, item);
        }
        Operation::Remove { id, reason } => {
            nonempty(&reason, MAX_RESULT_CHARS)?;
            let at = index(task, &id)?;
            task.todos.remove(at);
        }
        Operation::Complete { id, result } => {
            let result = nonempty(&result, MAX_RESULT_CHARS)?;
            let at = index(task, &id)?;
            if task.todos[at].done {
                return Ok(());
            }
            if task.current_todo().is_none_or(|item| item.id != id) {
                // Name the unfinished items ahead of the target so the caller
                // can complete or remove them in the same batch, in order.
                let ahead: Vec<&str> = task.todos[..at]
                    .iter()
                    .filter(|item| !item.done)
                    .map(|item| item.id.as_str())
                    .collect();
                bail!(
                    "Complete the current item first, or insert/move its prerequisite before it; {id} can complete only after {} are completed or removed, in list order",
                    ahead.join(", ")
                );
            }
            task.todos[at].done = true;
            task.todos[at].result = result.into();
            task.todos_completed_total = task.todos_completed_total.saturating_add(1);
        }
        Operation::Reopen { id, reason } => {
            let reason = nonempty(&reason, MAX_RESULT_CHARS)?;
            let at = index(task, &id)?;
            if !task.todos[at].done {
                bail!("The item is already unfinished; continue or move it");
            }
            let mut item = task.todos.remove(at);
            item.done = false;
            item.result = reason.into();
            let to = task
                .todos
                .iter()
                .position(|item| !item.done)
                .unwrap_or(task.todos.len());
            task.todos.insert(to, item);
        }
    }
    Ok(())
}

fn unchanged(task: &TaskState, reason: String) -> Value {
    json!({"applied":false,"reason":reason,"plan":view(task, 0, DEFAULT_PAGE),"guidance":"This result does not stop the task. Continue the current item's concrete work. Use the returned revision/IDs for a necessary plan correction; keep at most 100 unfinished items by completing or removing obsolete work. Do not repeat the unchanged request."})
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

fn decode_json(text: &str) -> Result<Value> {
    if text.len() > MAX_ENCODED_BYTES {
        bail!("Encoded operations exceed {MAX_ENCODED_BYTES} bytes; use a smaller batch");
    }
    // Strict JSON only: never repair partial JSON, evaluate text, or guess
    // operation ordering from a map. Each string wrapper is decoded once.
    serde_json::from_str(text).map_err(|error| {
        anyhow::anyhow!(
            "Invalid JSON: {error}; near {}. Send operations as a JSON array value, not as quoted text, so no brackets or quotes need hand escaping",
            json_error_context(text, error.line(), error.column())
        )
    })
}

/// The text around a JSON error with a marker at the reported position, so a
/// bracket or quote mistake inside hand-written JSON text is visible.
fn json_error_context(text: &str, line: usize, column: usize) -> String {
    let line_text = text.lines().nth(line.saturating_sub(1)).unwrap_or("");
    // serde_json's column is the 1-based byte offset of the offending
    // character; the marker goes just before it.
    let mut at = column.saturating_sub(1).min(line_text.len());
    while !line_text.is_char_boundary(at) {
        at -= 1;
    }
    let (head, tail) = line_text.split_at(at);
    let before: String = {
        let chars: Vec<char> = head.chars().collect();
        chars[chars.len().saturating_sub(40)..].iter().collect()
    };
    let after: String = tail.chars().take(40).collect();
    format!("{before:?} <here> {after:?}")
}

fn parse_operations(value: &Value) -> Result<(Vec<Operation>, bool)> {
    let decoded;
    let mut normalized = false;
    let value = if let Some(text) = value.as_str() {
        decoded = decode_json(text)?;
        normalized = true;
        &decoded
    } else {
        value
    };
    let items: Vec<&Value> = match value {
        Value::Array(items) => {
            if items.is_empty() || items.len() > MAX_OPERATIONS {
                bail!("Use 1..{MAX_OPERATIONS} operation objects in a batch");
            }
            items.iter().collect()
        }
        Value::Object(item) if item.contains_key("op") => {
            normalized = true;
            vec![value]
        }
        _ => bail!(
            "operations must be an array of operation objects; received {}",
            value_type(value)
        ),
    };
    let mut operations = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        let decoded;
        let item = if let Some(text) = item.as_str() {
            decoded =
                decode_json(text).map_err(|error| anyhow::anyhow!("operations[{i}]: {error}"))?;
            normalized = true;
            &decoded
        } else {
            item
        };
        if !item.is_object() {
            bail!(
                "operations[{i}] must be an operation object; received {}",
                value_type(item)
            );
        }
        // Live runs sent insert with a single "text" and an invented "id";
        // the plan assigns IDs, so accept that shape as one new item.
        let mut item = item.clone();
        if item["op"] == "insert" {
            let object = item.as_object_mut().unwrap();
            if let Some(text) = object.remove("text") {
                if !object.contains_key("texts") && text.is_string() {
                    object.insert("texts".into(), json!([text]));
                }
                normalized = true;
            }
            // The public nested schema contains fields for every operation.
            // Some providers fill all of them even for insert; these fields
            // have no meaning for a newly allocated plan item.
            for key in ["id", "reason", "result"] {
                if object.remove(key).is_some() {
                    normalized = true;
                }
            }
            if object
                .get("before")
                .and_then(Value::as_str)
                .is_some_and(|id| {
                    !id.starts_with('T')
                        || id.len() < 2
                        || !id[1..].bytes().all(|byte| byte.is_ascii_digit())
                })
            {
                object.remove("before");
                normalized = true;
            }
        }
        if item["op"] == "complete" {
            let object = item.as_object_mut().unwrap();
            // The shared nested schema offers reason for remove/reopen, so a
            // provider may also send it with complete. The actual completion
            // result remains the authoritative text.
            if let Some(reason) = object.remove("reason") {
                if !object.contains_key("result") && reason.is_string() {
                    object.insert("result".into(), reason);
                }
                normalized = true;
            }
        }
        // The published item schema is shared by all operation variants.
        // Providers can populate every optional field, so discard fields
        // belonging to other variants while retaining strict validation for
        // fields outside that schema.
        let allowed: &[&str] = match item["op"].as_str() {
            Some("insert") => &["texts", "before"],
            Some("update") => &["id", "text"],
            Some("split") => &["id", "texts"],
            Some("move") => &["id", "before"],
            Some("remove" | "reopen") => &["id", "reason"],
            Some("complete") => &["id", "result"],
            _ => &[],
        };
        let object = item.as_object_mut().unwrap();
        for key in ["texts", "before", "id", "text", "result", "reason"] {
            if !allowed.contains(&key) && object.remove(key).is_some() {
                normalized = true;
            }
        }
        let operation = serde_json::from_value(item).map_err(|error| {
            let detail: String = error.to_string().chars().take(240).collect();
            anyhow::anyhow!("operations[{i}]: {detail}")
        })?;
        operations.push(operation);
    }
    Ok((operations, normalized))
}

pub fn execute(s: &mut Session, args: &Value) -> Result<Value> {
    if args["action"] == "list" {
        return Ok(view(
            &s.task,
            n(args, "offset", 0),
            n(args, "limit", DEFAULT_PAGE),
        ));
    }
    if args["expected_revision"].as_u64() != Some(s.task.plan_revision) {
        let revision = s.task.plan_revision;
        let mut reason = match args["expected_revision"].as_u64() {
            Some(sent) => format!(
                "Plan revision changed: expected_revision {sent} but the plan is at revision {revision}; recheck the returned plan, then use {revision}"
            ),
            None => format!("expected_revision is missing; the plan is at revision {revision}"),
        };
        if args.get("operations").is_none() {
            reason.push_str(
                "; operations is also missing: apply needs an array of operation objects",
            );
        }
        return Ok(unchanged(&s.task, reason));
    }
    let (operations, normalized) = match parse_operations(&args["operations"]) {
        Ok(operations) => operations,
        Err(error) => {
            let mut result = unchanged(&s.task, format!("Invalid plan operation: {error}"));
            result["input_error"] = json!({
                "field":"operations", "expected":"array of operation objects",
                "received":args.get("operations").map_or("missing", value_type),
                "example":{"action":"apply","expected_revision":s.task.plan_revision,"operations":[{"op":"insert","texts":["Write the requested section"]}]}
            });
            return Ok(result);
        }
    };
    let mut next = s.task.clone();
    let count = operations.len();
    for (index, operation) in operations.into_iter().enumerate() {
        if let Err(error) = apply(&mut next, operation) {
            // Operations apply in order, so the plan the failing one saw can
            // differ from the one the caller read. Name the operation and the
            // item that was current at that point.
            let current = next
                .current_todo()
                .map_or("none".to_owned(), |item| item.id.clone());
            let reason = if count > 1 {
                format!(
                    "operations[{index}] failed: {error} (current item at that point: {current}). No operation in this batch was applied; earlier operations only take effect together with it"
                )
            } else {
                format!("{error} (current item: {current})")
            };
            return Ok(unchanged(&s.task, reason));
        }
    }
    if next.todos.iter().filter(|item| !item.done).count() > MAX_PENDING {
        return Ok(unchanged(
            &s.task,
            format!(
                "The plan has room for at most {MAX_PENDING} unfinished items; no operations were applied"
            ),
        ));
    }
    while next.todos.iter().filter(|item| item.done).count() > MAX_COMPLETED {
        let at = next.todos.iter().position(|item| item.done).unwrap();
        next.todos.remove(at);
    }
    if next.todos == s.task.todos
        && next.todo_sequence == s.task.todo_sequence
        && next.todos_completed_total == s.task.todos_completed_total
    {
        return Ok(
            json!({"applied":true,"unchanged":true,"input_normalized":normalized,"plan":view(&s.task, 0, DEFAULT_PAGE)}),
        );
    }
    next.plan_revision = next.plan_revision.saturating_add(1);
    next.revision = next.revision.saturating_add(1);
    let compact =
        context::ContextManager::task_snapshot(&next, &s.config.model, s.config.state_tokens);
    if context::count(&compact, &s.config.model) > s.config.state_tokens
        || serde_json::to_vec(&next)?.len() > s.config.memory_bytes
    {
        return Ok(unchanged(&s.task, "Task state budget is full; shorten item text or task_state findings/details before extending the plan".into()));
    }
    s.task = next;
    Ok(json!({"applied":true,"input_normalized":normalized,"plan":view(&s.task, 0, DEFAULT_PAGE)}))
}
