//! Ordered, bounded plans. Recoverable plan conflicts are no-op results, so
//! capacity or stale revisions never terminate the owner's running task.
use super::*;
use crate::session::TodoItem;
use serde::Deserialize;

pub const MAX_PENDING: usize = 100;
pub const MAX_COMPLETED: usize = 5;
const DEFAULT_PAGE: usize = 10;
const MAX_PAGE: usize = 20;
/// A plan item names one section or area with what to read and write; a
/// live model wrote 237- and 244-character items and its plan was refused.
const MAX_TEXT_CHARS: usize = 500;
/// A live model's 382-character result was refused and it wrote the result
/// into the item text instead of completing the item.
const MAX_RESULT_CHARS: usize = 500;
const MAX_OPERATIONS: usize = 16;
const MAX_ENCODED_BYTES: usize = 128 * 1024;

#[derive(Clone, Deserialize)]
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

impl Operation {
    fn identity(&self) -> (&'static str, Option<&str>) {
        match self {
            Self::Insert { .. } => ("insert", None),
            Self::Update { id, .. } => ("update", Some(id)),
            Self::Split { id, .. } => ("split", Some(id)),
            Self::Move { id, .. } => ("move", Some(id)),
            Self::Remove { id, .. } => ("remove", Some(id)),
            Self::Complete { id, .. } => ("complete", Some(id)),
            Self::Reopen { id, .. } => ("reopen", Some(id)),
        }
    }
}

fn operation_error(message: String, details: Value) -> anyhow::Error {
    recovery::DiagnosticError {
        message,
        data: details,
    }
    .into()
}

fn error_details(error: &anyhow::Error, code: &str) -> Value {
    error
        .downcast_ref::<recovery::DiagnosticError>()
        .map_or_else(|| json!({"code":code}), |error| error.data.clone())
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
        description: "Manage an ordered to-do list; the first unfinished item is current. list accepts offset/limit (default 10, maximum 20). apply requires expected_revision copied from the returned plan and an operations array of 1..16 objects, applied atomically. insert: texts, optional before; update: id/text; split: unfinished id and 2..100 specific texts preserving its goal, keeping its ID for the first part; move: unfinished id, optional before. Omit before to append. complete: CURRENT id/result after actually doing the work. remove: unfinished id/reason; reopen: completed id/reason, placed before current. Keep at most 100 unfinished small outcomes and the latest 5 completed items plus completed_total. Recoverable conflicts return applied=false; the whole batch remains unapplied. conflict reports code, zero-based operation_index, target/current IDs, blocking_ids, expected/current revisions and correction. Continue current work.",
        optional: false,
        read_only: false,
        parameters,
    }
}

pub fn view(task: &TaskState, offset: usize, limit: usize) -> Value {
    let limit = limit.clamp(1, MAX_PAGE);
    let items: Vec<_> = task.todos.iter().skip(offset).take(limit).collect();
    let end = offset.saturating_add(items.len());
    let mut page = json!({
        "revision":task.plan_revision,"items":items,"current":task.current_todo(),
        "offset":offset,"total_items":task.todos.len(),
        "next_offset":(end < task.todos.len()).then_some(end),
        "pending_count":task.todos.iter().filter(|item| !item.done).count(),
        "completed_total":task.todos_completed_total,
        "limits":{"pending":MAX_PENDING,"retained_completed":MAX_COMPLETED}
    });
    // An empty page past the end is not an empty plan.
    if offset >= task.todos.len() && !task.todos.is_empty() {
        page["notice"] = json!(format!(
            "offset {offset} is past the {} plan items; list from offset 0",
            task.todos.len()
        ));
    }
    page
}

fn nonempty(value: &str, limit: usize) -> Result<&str> {
    let value = value.trim();
    if value.is_empty() {
        bail!("Use nonempty text of at most {limit} characters");
    }
    // Say how far over the limit it is, or the model trims a little and
    // resends an oversized text again.
    let count = value.chars().count();
    if count > limit {
        bail!(
            "Use nonempty text of at most {limit} characters; this text has {count}. Keep one short sentence and drop hashes, line lists and audit details"
        );
    }
    Ok(value)
}

fn index(task: &TaskState, id: &str) -> Result<usize> {
    task.todos
        .iter()
        .position(|item| item.id == id)
        .ok_or_else(|| {
            operation_error(
                format!("Unknown item {id}; copy IDs from the returned plan"),
                json!({"code":"unknown_item","target_id":id}),
            )
        })
}

fn position(task: &TaskState, before: Option<&str>) -> Result<usize> {
    match before {
        None => Ok(task.todos.len()),
        Some(id) => {
            let at = index(task, id)?;
            if task.todos[at].done {
                // Name the way out: with no unfinished item left, a live
                // model kept naming a completed item for three requests.
                let hint = match task.current_todo() {
                    Some(current) => format!(
                        "omit before to place them at the end, or use before {:?} (the current item) to put them ahead of the remaining work",
                        current.id
                    ),
                    None => {
                        "no unfinished item is left, so omit before to append at the end".into()
                    }
                };
                return Err(operation_error(
                    format!(
                        "Insert or move before an unfinished item; completed history stays fixed. {id} is completed: {hint}"
                    ),
                    json!({"code":"completed_anchor","before_id":id}),
                ));
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
            let start = position(task, before.as_deref())?;
            for (at, text) in (start..).zip(texts) {
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
                        reopen_reason: String::new(),
                    },
                );
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
                        reopen_reason: String::new(),
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
            if task.todos[at].done {
                bail!(
                    "Completed history stays fixed and drops out on its own; remove only unfinished items, or reopen this one with a reason"
                );
            }
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
                // can complete or remove them in the same batch.
                let ahead: Vec<&str> = task.todos[..at]
                    .iter()
                    .filter(|item| !item.done)
                    .map(|item| item.id.as_str())
                    .collect();
                return Err(operation_error(
                    format!(
                        "Complete the current item first, or insert/move its prerequisite before it; {id} can complete only after {} are completed or removed, in this batch or before it",
                        ahead.join(", ")
                    ),
                    json!({"code":"out_of_order","target_id":id,"blocking_ids":ahead,"blocking_count":ahead.len()}),
                ));
            }
            task.todos[at].done = true;
            task.todos[at].result = result.into();
            task.todos[at].reopen_reason.clear();
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
            item.result.clear();
            item.reopen_reason = reason.into();
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

fn unchanged(
    task: &TaskState,
    reason: String,
    expected_revision: Option<u64>,
    mut conflict: Value,
) -> Value {
    conflict["expected_revision"] = json!(expected_revision);
    conflict["current_revision"] = json!(task.plan_revision);
    if conflict.get("current_id").is_none() {
        conflict["current_id"] = json!(task.current_todo().map(|item| &item.id));
    }
    conflict["correction"] = json!(match conflict["code"].as_str() {
        Some("out_of_order") =>
            "No operations were committed. Continue the blocking items' actual work first; after completing it, resend the whole batch with those items completed too, using current_revision. Remove only work that is actually obsolete, with a reason.",
        Some("missing_revision" | "revision_conflict") =>
            "Review the returned plan, copy current_revision into expected_revision, and combine necessary changes into one apply call.",
        Some("pending_limit" | "state_limit") =>
            "Shorten the request or complete/remove obsolete work, then resend a smaller batch using current_revision.",
        Some("completed_anchor") =>
            "No operations were committed. Completed items stay where they are: omit before to append at the end, or set before to an unfinished item such as current_id, then resend the whole batch using current_revision.",
        Some("closing_mode") =>
            "No operations were committed. Complete each remaining item with its actual result, or remove it with a reason when it will not be done, then give the final answer; the runtime reports unfinished work.",
        _ =>
            "Correct the reported operation using the returned IDs and current_revision, then resend the whole batch. No earlier operation was committed.",
    });
    json!({"applied":false,"reason":reason,"conflict":conflict,"plan":view(task, 0, DEFAULT_PAGE),"guidance":"This result does not stop the task. Continue the current item's concrete work. Use the returned revision/IDs for a necessary plan correction; keep at most 100 unfinished items by completing or removing obsolete work. Do not repeat the unchanged request."})
}

pub(super) fn compact_conflict(conflict: &Value) -> Value {
    let mut compact = json!({});
    for field in [
        "code",
        "operation_index",
        "target_id",
        "current_id",
        "expected_revision",
        "current_revision",
    ] {
        if let Some(value) = conflict.get(field) {
            compact[field] = value.clone();
        }
    }
    if let Some(ids) = conflict["blocking_ids"].as_array() {
        compact["blocking_ids"] = json!(ids.iter().take(3).collect::<Vec<_>>());
        compact["blocking_count"] = json!(ids.len());
        if ids.len() > 3 {
            compact["blocking_ids_truncated"] = json!(true);
        }
    }
    compact
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
        // At the end of the text serde_json reports the last character, so
        // the marker sat before a final "}" that was fine. A live model sent
        // text missing only its last "]" six times; name what is missing.
        let near = if error.classify() == serde_json::error::Category::Eof {
            let tail: Vec<char> = text.trim_end().chars().collect();
            let tail: String = tail[tail.len().saturating_sub(40)..].iter().collect();
            format!(
                "{tail:?} <here> (end of text), which still needs {}",
                unfinished_json(text)
            )
        } else {
            json_error_context(text, error.line(), error.column())
        };
        anyhow::anyhow!(
            "Invalid JSON: {error}; near {near}. Send operations as a JSON array value, not as quoted text, so no brackets or quotes need hand escaping"
        )
    })
}

/// What an unfinished JSON text still needs at its end: a value after a
/// trailing ":" or ",", then the quote and brackets it left open, in order.
fn unfinished_json(text: &str) -> String {
    let mut open = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut last = None;
    for c in text.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '[' => open.push(']'),
            '{' => open.push('}'),
            ']' | '}' => {
                open.pop();
            }
            _ => {}
        }
        if !c.is_whitespace() {
            last = Some(c);
        }
    }
    let mut closing = String::new();
    if in_string {
        closing.push('"');
    }
    closing.extend(open.iter().rev());
    let value = !in_string && matches!(last, Some(':' | ','));
    match (value, closing.is_empty()) {
        (true, true) => "a value".into(),
        (true, false) => format!("a value and then its closing {closing:?}"),
        (false, true) => "the rest of its last value".into(),
        (false, false) => format!("its closing {closing:?}"),
    }
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

/// Parsed operations, whether their encoding was normalized, and notices for
/// normalizations that changed what the caller asked for.
type Parsed = (Vec<Operation>, bool, Vec<String>);

fn parse_operations(value: &Value) -> Result<Parsed> {
    let decoded;
    let mut normalized = false;
    let mut notices = Vec::new();
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
            decoded = decode_json(text).map_err(|error| {
                operation_error(
                    format!("operations[{i}]: {error}"),
                    json!({"code":"invalid_operations","operation_index":i}),
                )
            })?;
            normalized = true;
            &decoded
        } else {
            item
        };
        if !item.is_object() {
            return Err(operation_error(
                format!(
                    "operations[{i}] must be an operation object; received {}",
                    value_type(item)
                ),
                json!({"code":"invalid_operations","operation_index":i}),
            ));
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
            // Providers also fill before with placeholders. Append those
            // items, but say so: a prerequisite meant for an earlier
            // position now sits at the end and needs a move.
            if let Some(id) = object
                .get("before")
                .and_then(Value::as_str)
                .filter(|id| {
                    !id.starts_with('T')
                        || id.len() < 2
                        || !id[1..].bytes().all(|byte| byte.is_ascii_digit())
                })
                .map(str::to_owned)
            {
                object.remove("before");
                normalized = true;
                notices.push(format!(
                    "operations[{i}]: before {id:?} is not a plan ID, so the items were appended at the end; move them before the intended item if order matters"
                ));
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
        if item["op"] == "update" {
            let object = item.as_object_mut().unwrap();
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("the item")
                .to_owned();
            // A live model "completed" an item five times with update and a
            // real result: the result was discarded as a schema placeholder
            // and the reply only said unchanged, so the item stayed open.
            if object.contains_key("done") {
                return Err(operation_error(
                    format!(
                        "operations[{i}]: update has no done field and cannot finish an item; to finish {id} after doing its work, send {{\"op\":\"complete\",\"id\":\"{id}\",\"result\":\"<observed result>\"}}"
                    ),
                    json!({"code":"invalid_operations","operation_index":i,"operation":"update","target_id":id}),
                ));
            }
            if object
                .get("result")
                .and_then(Value::as_str)
                .is_some_and(|result| result.trim().chars().count() >= 8)
            {
                notices.push(format!(
                    "operations[{i}]: update changes only an item's text, so result was ignored and {id} is still unfinished; to finish it after doing its work, send {{\"op\":\"complete\",\"id\":\"{id}\",\"result\":...}}"
                ));
            }
        }
        if matches!(item["op"].as_str(), Some("remove" | "reopen")) {
            let object = item.as_object_mut().unwrap();
            // The mirror of complete: the shared schema also offers result,
            // and a provider may send it as the remove/reopen reason.
            if let Some(result) = object.remove("result") {
                if !object.contains_key("reason") && result.is_string() {
                    object.insert("reason".into(), result);
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
        let operation_name = item["op"].clone();
        let target_id = item.get("id").cloned();
        let shape = item.clone();
        let operation = serde_json::from_value(item).map_err(|error| {
            let detail: String = error.to_string().chars().take(240).collect();
            let hint = operation_hint(&detail, &shape, allowed);
            operation_error(format!("operations[{i}]: {detail}{hint}"),
                json!({"code":"invalid_operations","operation_index":i,"operation":operation_name,"target_id":target_id}))
        })?;
        operations.push(operation);
    }
    Ok((operations, normalized, notices))
}

/// The operation or field a rejected plan operation most likely meant
/// ("add" for insert, "items" for texts).
fn operation_hint(detail: &str, operation: &Value, allowed: &[&str]) -> String {
    let named = |marker: &str| {
        let rest = detail.split_once(marker)?.1;
        rest.split_once('`').map(|(name, _)| name.to_owned())
    };
    if let Some(sent) = named("unknown variant `") {
        let ops: Vec<Value> = [
            "insert", "update", "split", "move", "remove", "complete", "reopen",
        ]
        .iter()
        .map(|op| json!(op))
        .collect();
        return super::suggest::value("task_plan", "op", &json!(sent), &ops, operation)
            .map_or_else(String::new, |s| s.text);
    }
    if let Some(field) = named("unknown field `") {
        let mut fields = vec!["op"];
        fields.extend_from_slice(allowed);
        return super::suggest::field("task_plan", "operations", &field, &fields, operation)
            .map_or_else(String::new, |s| s.text);
    }
    String::new()
}

/// A value a schema-filling provider sends for a field it does not use.
fn filler(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => matches!(text.trim(), "" | "x" | "X" | "..." | "…" | "string"),
        Value::Array(items) => items.iter().all(filler),
        _ => false,
    }
}

/// Operations that name an actual change. Some providers fill every field
/// of the shared operation schema with "x" on each list call, often with
/// the current item's real id; those carry nothing to apply and need no
/// notice, which could prompt applying a completion with an "x" result.
fn intended_operations(value: &Value) -> bool {
    let decoded;
    let value = match value.as_str() {
        Some(text) => match serde_json::from_str(text) {
            Ok(parsed) => {
                decoded = parsed;
                &decoded
            }
            Err(_) => return !filler(value),
        },
        None => value,
    };
    // Only the content the operation itself uses: a provider can copy a real
    // item id into any filled field, such as before of an update.
    let names_change = |item: &Value| {
        let fields: &[&str] = match item["op"].as_str() {
            Some("insert" | "split") => &["texts"],
            Some("update") => &["text"],
            Some("move") => &["before"],
            Some("complete") => &["result"],
            Some("remove" | "reopen") => &["reason"],
            _ => &["texts", "text", "result", "reason", "before"],
        };
        fields
            .iter()
            .any(|key| item.get(*key).is_some_and(|value| !filler(value)))
    };
    match value {
        Value::Array(items) => items.iter().any(names_change),
        Value::Object(_) => names_change(value),
        _ => false,
    }
}

/// Why a batch cannot apply: its reason and conflict details, plus the input
/// error with an example when the operations value cannot be read.
struct Rejection {
    reason: String,
    conflict: Value,
    input_error: Option<Value>,
}

/// The unfinished items ahead of a complete that was refused for its order.
fn blocked_by(error: &anyhow::Error) -> Option<Vec<String>> {
    let details = error_details(error, "");
    (details["code"] == "out_of_order").then(|| {
        details["blocking_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    })
}

fn out_of_order(result: &Result<()>) -> bool {
    result
        .as_ref()
        .is_err_and(|error| blocked_by(error).is_some())
}

/// Whether an operation completes or removes the item.
fn finishes(operation: &Operation, item: &str) -> bool {
    matches!(operation, Operation::Complete { id, .. } | Operation::Remove { id, .. } if id == item)
}

/// The plan after applying every operation to a copy, whether the encoding
/// was normalized, and notices; or why the batch cannot apply. Nothing is
/// committed either way.
fn trial(
    task: &TaskState,
    operations: Option<&Value>,
) -> std::result::Result<(TaskState, bool, Vec<String>), Box<Rejection>> {
    let (operations, normalized, notices) =
        parse_operations(operations.unwrap_or(&Value::Null)).map_err(|error| {
            Box::new(Rejection {
                reason: format!("Invalid plan operation: {error}"),
                conflict: error_details(&error, "invalid_operations"),
                input_error: Some(json!({
                    "field":"operations", "expected":"array of operation objects",
                    "received":operations.map_or("missing", value_type),
                    "example":{"action":"apply","expected_revision":task.plan_revision,"operations":[{"op":"insert","texts":["Write the requested section"]}]}
                })),
            })
        })?;
    let mut next = task.clone();
    let count = operations.len();
    // Operations apply in order, so the plan the failing one saw can differ
    // from the one the caller read. Name the operation and the item that was
    // current at that point.
    let failure = |index: usize, operation: &Operation, current: Option<String>, error| {
        let (operation_name, target_id) = operation.identity();
        let current_text = current.as_deref().unwrap_or("none");
        let reason = if count > 1 {
            format!(
                "operations[{index}] failed: {error} (current item at that point: {current_text}). No operation in this batch was applied; earlier operations only take effect together with it"
            )
        } else {
            format!("{error} (current item: {current_text})")
        };
        let mut conflict = error_details(&error, "invalid_operation");
        conflict["operation_index"] = json!(index);
        conflict["operation"] = json!(operation_name);
        conflict["target_id"] = json!(target_id);
        conflict["current_id"] = json!(current);
        Box::new(Rejection {
            reason,
            conflict,
            input_error: None,
        })
    };
    // A live batch completed T6, T13, T7 and T14 while T7 came before T13 in
    // the plan, and was refused for that order alone. A complete whose
    // earlier unfinished items the same batch also completes or removes waits
    // for them and applies right after them, as in plan order.
    let mut waiting: Vec<(usize, &Operation)> = Vec::new();
    for (index, operation) in operations.iter().enumerate() {
        let current = next.current_todo().map(|item| item.id.clone());
        match apply(&mut next, operation.clone()) {
            Ok(()) => {
                while let Some(at) = waiting.iter().position(|(_, waiting)| {
                    !out_of_order(&apply(&mut next.clone(), (*waiting).clone()))
                }) {
                    let (waiting_index, waiting) = waiting.remove(at);
                    let current = next.current_todo().map(|item| item.id.clone());
                    if let Err(error) = apply(&mut next, waiting.clone()) {
                        return Err(failure(waiting_index, waiting, current, error));
                    }
                }
            }
            Err(error)
                if blocked_by(&error).is_some_and(|blockers| {
                    blockers.iter().all(|id| {
                        operations[index + 1..]
                            .iter()
                            .chain(waiting.iter().map(|(_, waiting)| *waiting))
                            .any(|later| finishes(later, id))
                    })
                }) =>
            {
                waiting.push((index, operation));
            }
            Err(error) => return Err(failure(index, operation, current, error)),
        }
    }
    // Whatever still waits names the items the batch left unfinished.
    for (index, operation) in waiting {
        let current = next.current_todo().map(|item| item.id.clone());
        if let Err(error) = apply(&mut next, operation.clone()) {
            return Err(failure(index, operation, current, error));
        }
    }
    let pending = next.todos.iter().filter(|item| !item.done).count();
    if pending > MAX_PENDING {
        return Err(Box::new(Rejection {
            reason: format!(
                "The plan has room for at most {MAX_PENDING} unfinished items; no operations were applied"
            ),
            conflict: json!({"code":"pending_limit","limit":MAX_PENDING,"requested_pending":pending}),
            input_error: None,
        }));
    }
    while next.todos.iter().filter(|item| item.done).count() > MAX_COMPLETED {
        let at = next.todos.iter().position(|item| item.done).unwrap();
        next.todos.remove(at);
    }
    Ok((next, normalized, notices))
}

pub fn execute(s: &mut Session, args: &Value) -> Result<Value> {
    if args["action"] == "list" {
        let page = view(
            &s.task,
            n(args, "offset", 0),
            n(args, "limit", DEFAULT_PAGE),
        );
        // A live run sent a real update with action=list; it was dropped
        // without a word and the model had to guess why nothing changed.
        let notices = if intended_operations(&args["operations"]) {
            vec![format!(
                "action list only reads the plan, so these operations were not applied and the plan is still at revision {}; to make the change, send the operations with action \"apply\" and expected_revision {}",
                s.task.plan_revision, s.task.plan_revision
            )]
        } else {
            vec![]
        };
        return Ok(with_notices(page, notices));
    }
    // The first plan has nothing to conflict with, so a missing revision
    // means 0; a later apply still has to name the revision it changes.
    let expected_revision = args["expected_revision"]
        .as_u64()
        .or_else(|| (s.task.plan_revision == 0 && s.task.todos.is_empty()).then_some(0));
    if expected_revision != Some(s.task.plan_revision) {
        let revision = s.task.plan_revision;
        let mut reason = match args["expected_revision"].as_u64() {
            Some(sent) => format!(
                "Plan revision changed: expected_revision {sent} but the plan is at revision {revision}; recheck the returned plan, then use {revision}. Each applied change advances the revision, so put all plan changes of one response in a single apply instead of several calls"
            ),
            None => format!("expected_revision is missing; the plan is at revision {revision}"),
        };
        let mut rejected = None;
        if args.get("operations").is_none() {
            reason.push_str(
                "; operations is also missing: apply needs an array of operation objects",
            );
        } else if expected_revision.is_none() {
            // Check the operations too, so one reply names every problem: a
            // live model learned of a missing revision, then of JSON text
            // missing its last bracket, then of a completed anchor, one
            // request each.
            match trial(&s.task, args.get("operations")) {
                Ok(_) => reason.push_str(&format!(
                    "; the operations are otherwise valid, so resend them unchanged with expected_revision {revision}"
                )),
                Err(rejection) => {
                    reason.push_str(&format!(
                        "; the operations would also fail: {}",
                        rejection.reason
                    ));
                    rejected = Some(rejection);
                }
            }
        }
        let mut result = unchanged(
            &s.task,
            reason,
            expected_revision,
            json!({"code":if expected_revision.is_some() { "revision_conflict" } else { "missing_revision" }}),
        );
        if let Some(rejection) = rejected {
            result["conflict"]["operations_conflict"] = rejection.conflict;
            result["conflict"]["correction"] = json!(
                "No operations were committed. Copy current_revision into expected_revision and correct the operation the reason names, then resend the whole batch in one apply call."
            );
            if let Some(input_error) = rejection.input_error {
                result["input_error"] = input_error;
            }
        }
        return Ok(result);
    }
    let (mut next, normalized, notices) = match trial(&s.task, args.get("operations")) {
        Ok(trial) => trial,
        Err(rejection) => {
            let mut result = unchanged(
                &s.task,
                rejection.reason,
                expected_revision,
                rejection.conflict,
            );
            if let Some(input_error) = rejection.input_error {
                result["input_error"] = input_error;
            }
            return Ok(result);
        }
    };
    // Closing mode finishes from gathered evidence, and open to-dos refuse
    // the final answer, so the plan may only shrink there. A live model split
    // one item into six in closing mode (eight open items became twelve,
    // several repeating open ones) and the run ended at the closing limit
    // with every one of them reported unfinished.
    let open = |task: &TaskState| task.todos.iter().filter(|item| !item.done).count();
    if s.progress_recovery.closing.is_some() && open(&next) > open(&s.task) {
        return Ok(unchanged(
            &s.task,
            format!(
                "closing_mode: this change would raise the open to-dos from {} to {}; closing mode adds none",
                open(&s.task),
                open(&next)
            ),
            expected_revision,
            json!({"code":"closing_mode"}),
        ));
    }
    if next.todos == s.task.todos
        && next.todo_sequence == s.task.todo_sequence
        && next.todos_completed_total == s.task.todos_completed_total
    {
        return Ok(with_notices(
            json!({"applied":true,"unchanged":true,"input_normalized":normalized,"plan":view(&s.task, 0, DEFAULT_PAGE),
                "guidance":"Nothing changed: the plan already matched this request, so no item was added, updated or completed. Do not resend it; continue the current item's work, or use the operation that makes the intended change (complete finishes an item)."}),
            notices,
        ));
    }
    next.plan_revision = next.plan_revision.saturating_add(1);
    next.revision = next.revision.saturating_add(1);
    let compact =
        context::ContextManager::task_snapshot(&next, &s.config.model, s.config.state_tokens);
    if context::count(&compact, &s.config.model) > s.config.state_tokens
        || serde_json::to_vec(&next)?.len() > s.config.memory_bytes
    {
        return Ok(unchanged(&s.task, "Task state budget is full; shorten item text or task_state findings/details before extending the plan".into(), expected_revision, json!({"code":"state_limit"})));
    }
    s.task = next;
    Ok(with_notices(
        json!({"applied":true,"input_normalized":normalized,"plan":view(&s.task, 0, DEFAULT_PAGE)}),
        notices,
    ))
}

fn with_notices(mut result: Value, notices: Vec<String>) -> Value {
    if !notices.is_empty() {
        result["notices"] = json!(notices);
    }
    result
}
