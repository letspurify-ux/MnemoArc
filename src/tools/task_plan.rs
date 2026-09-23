//! Ordered, bounded plans. Recoverable plan conflicts are no-op results, so
//! capacity or stale revisions never terminate the owner's running task.
use super::*;
use crate::session::TodoItem;
use serde::Deserialize;

pub const MAX_PENDING: usize = 8;
pub const MAX_COMPLETED: usize = 5;
const MAX_TEXT_CHARS: usize = 160;
const MAX_RESULT_CHARS: usize = 240;

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
    ToolSpec {
        name: "task_plan",
        description: "Manage an ordered to-do list for multi-step work. list returns revision, stable item IDs and current (first unfinished item). apply atomically applies 1..16 operations using expected_revision. insert takes texts and optional before pending-item ID; omit before to append. update changes a pending item's text. move reorders a pending item before another pending ID, or to the end. complete requires the CURRENT item's ID and a specific result after actually doing the work. remove needs a reason; preserve user requirements in task_state.completion. reopen needs a reason and puts a completed item before the current item. Maximum 8 unfinished items; plan the next few concrete outcomes, not every read. Keep source investigation and writing in the same section-sized item. Only the latest 5 completed items are retained; completed_total is preserved. A stale revision, full list or invalid transition returns applied=false with the unchanged plan; continue the current work, then retry only if needed. Plan edits do not count as actual work progress.",
        optional: false,
        read_only: false,
        parameters: schema(
            json!({
                "action":action(&["list","apply"]),
                "expected_revision":number(),
                "operations":{"type":"array","minItems":1,"maxItems":16,"items":{"oneOf":[
                    schema(json!({"op":{"const":"insert"},"texts":{"type":"array","minItems":1,"maxItems":MAX_PENDING,"items":short},"before":id}), &["op","texts"]),
                    schema(json!({"op":{"const":"update"},"id":id,"text":short}), &["op","id","text"]),
                    schema(json!({"op":{"const":"move"},"id":id,"before":id}), &["op","id"]),
                    schema(json!({"op":{"const":"remove"},"id":id,"reason":note}), &["op","id","reason"]),
                    schema(json!({"op":{"const":"complete"},"id":id,"result":note}), &["op","id","result"]),
                    schema(json!({"op":{"const":"reopen"},"id":id,"reason":note}), &["op","id","reason"])
                ]}}
            }),
            &["action"],
        ),
    }
}

pub fn view(task: &TaskState) -> Value {
    json!({
        "revision":task.plan_revision,"items":task.todos,"current":task.current_todo(),
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
                bail!("Complete the current item first, or insert/move its prerequisite before it");
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
    json!({"applied":false,"reason":reason,"plan":view(task),"guidance":"The task is still running. Continue the current item's concrete work. Use the returned revision/IDs for a necessary plan correction; keep at most 8 unfinished items by completing, merging or removing obsolete work. Do not repeat the unchanged request."})
}

pub fn execute(s: &mut Session, args: &Value) -> Result<Value> {
    if args["action"] == "list" {
        return Ok(view(&s.task));
    }
    if args["expected_revision"].as_u64() != Some(s.task.plan_revision) {
        return Ok(unchanged(
            &s.task,
            "Plan revision changed or expected_revision is missing".into(),
        ));
    }
    let operations: Vec<Operation> = match serde_json::from_value(args["operations"].clone()) {
        Ok(operations) => operations,
        Err(error) => {
            return Ok(unchanged(
                &s.task,
                format!("Invalid plan operation: {error}"),
            ));
        }
    };
    if operations.is_empty() || operations.len() > 16 {
        return Ok(unchanged(&s.task, "Use 1..16 operations in a batch".into()));
    }
    let mut next = s.task.clone();
    for operation in operations {
        if let Err(error) = apply(&mut next, operation) {
            return Ok(unchanged(&s.task, error.to_string()));
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
        return Ok(json!({"applied":true,"unchanged":true,"plan":view(&s.task)}));
    }
    next.plan_revision = next.plan_revision.saturating_add(1);
    next.revision = next.revision.saturating_add(1);
    let mut compact = json!(next);
    compact.as_object_mut().unwrap().remove("details");
    if context::count(&compact, &s.config.model) > s.config.state_tokens
        || serde_json::to_vec(&next)?.len() > s.config.memory_bytes
    {
        return Ok(unchanged(&s.task, "Task state budget is full; shorten item text or task_state findings/details before extending the plan".into()));
    }
    s.task = next;
    Ok(json!({"applied":true,"plan":view(&s.task)}))
}
