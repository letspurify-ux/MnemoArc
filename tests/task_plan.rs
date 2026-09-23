use anyhow::Result;
use async_trait::async_trait;
use mnemoarc::{
    agent::run_session,
    config::{Config, Project},
    context::ContextManager,
    llm::{Completion, LlmClient, ToolCall},
    session::{Session, TaskState},
    tools,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn session(root: &std::path::Path) -> Session {
    Session::new(
        Project {
            root: root.into(),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            source_answer_review: false,
            ..Default::default()
        },
    )
}

fn apply(s: &mut Session, operations: Value) -> Value {
    tools::execute(
        s,
        "task_plan",
        json!({"action":"apply","expected_revision":s.task.plan_revision,"operations":operations}),
    )
    .unwrap()
}

#[test]
fn prerequisites_insert_move_remove_and_complete_in_order_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.task.completion = vec!["Write and verify requested output".into()];
    apply(
        &mut s,
        json!([{"op":"insert","texts":["Write section","Verify output","Unused work"]}]),
    );
    let write = s.task.todos[0].id.clone();
    let verify = s.task.todos[1].id.clone();
    let unused = s.task.todos[2].id.clone();
    apply(
        &mut s,
        json!([{"op":"insert","texts":["Read missing declaration"],"before":write}]),
    );
    let prerequisite = s.task.current_todo().unwrap().id.clone();
    let before = serde_json::to_value(&s.task).unwrap();
    let refused = apply(
        &mut s,
        json!([
            {"op":"remove","id":unused,"reason":"Outside requested scope"},
            {"op":"complete","id":verify,"result":"Tried to skip the prerequisite"}
        ]),
    );
    assert_eq!(refused["applied"], false);
    assert_eq!(serde_json::to_value(&s.task).unwrap(), before);
    apply(
        &mut s,
        json!([
            {"op":"move","id":unused,"before":write},
            {"op":"remove","id":unused,"reason":"Outside requested scope"},
            {"op":"complete","id":prerequisite,"result":"Read the exact declaration"},
            {"op":"complete","id":write,"result":"Saved the section"}
        ]),
    );
    assert_eq!(s.task.current_todo().unwrap().id, verify);
    assert_eq!(s.task.completion, ["Write and verify requested output"]);
    apply(
        &mut s,
        json!([{"op":"reopen","id":write,"reason":"Review found a factual error"}]),
    );
    assert_eq!(s.task.current_todo().unwrap().id, write);
    assert!(
        !s.task
            .todos
            .iter()
            .find(|item| item.id == write)
            .unwrap()
            .done
    );
}

#[test]
fn full_plan_and_stale_revision_do_not_fail_or_lose_work() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.status = "running".into();
    let texts: Vec<_> = (1..=8).map(|i| format!("Outcome {i}")).collect();
    apply(&mut s, json!([{"op":"insert","texts":texts}]));
    let original = s.task.todos.clone();
    let refused = apply(&mut s, json!([{"op":"insert","texts":["Later work"]}]));
    assert_eq!(refused["applied"], false);
    assert_eq!(s.task.todos, original);
    assert_eq!(s.status, "running");
    let stale = tools::execute(&mut s, "task_plan", json!({"action":"apply","expected_revision":0,"operations":[{"op":"remove","id":"T1","reason":"Stale plan"}]})).unwrap();
    assert_eq!(stale["applied"], false);
    apply(
        &mut s,
        json!([
            {"op":"complete","id":"T1","result":"Produced outcome 1"},
            {"op":"insert","texts":["Later work"]}
        ]),
    );
    assert_eq!(s.task.todos.iter().filter(|item| !item.done).count(), 8);
    assert_eq!(s.task.current_todo().unwrap().id, "T2");
}

#[test]
fn completed_history_is_bounded_and_checkpoint_preserves_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    for i in 0..12 {
        apply(
            &mut s,
            json!([{"op":"insert","texts":[format!("Outcome {i}")]}]),
        );
        let id = s.task.current_todo().unwrap().id.clone();
        apply(
            &mut s,
            json!([{"op":"complete","id":id,"result":format!("Saved result {i}")} ]),
        );
    }
    assert_eq!(s.task.todos.len(), 5);
    assert_eq!(s.task.todos_completed_total, 12);
    apply(&mut s, json!([{"op":"insert","texts":["Continue work"]}]));
    let plan = s.task.todos.clone();
    s.add_user("Continue current plan".into());
    ContextManager::prepare(&mut s, 60000).unwrap();
    let id = s.checkpoint.as_ref().unwrap().id.clone();
    tools::execute(&mut s,"checkpoint_complete",json!({"id":id,"progress":"Necessary facts are stored","next":"Legacy summary must not overwrite the list","no_save_reason":"No new facts"})).unwrap();
    ContextManager::commit(&mut s).unwrap();
    assert_eq!(s.task.todos, plan);
    s.add_user("계속".into());
    assert_eq!(s.task.todos, plan);
    s.add_user("A new task".into());
    assert!(s.task.todos.is_empty());
    assert_eq!(s.task.todos_completed_total, 0);
}

#[test]
fn legacy_progress_is_migrated_once_and_no_longer_serialized() {
    let mut task: TaskState = serde_json::from_value(
        json!({"current":"Read source","next":"Write section","done":["Located entry"]}),
    )
    .unwrap();
    task.migrate_legacy_plan();
    let once = task.todos.clone();
    task.migrate_legacy_plan();
    assert_eq!(task.todos, once);
    assert_eq!(task.current_todo().unwrap().text, "Write section");
    let serialized = json!(task);
    assert!(serialized.get("current").is_none());
    assert!(serialized.get("next").is_none());
    assert!(serialized.get("done").is_none());
}

struct PlanChurn(Mutex<usize>);
#[async_trait]
impl LlmClient for PlanChurn {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let state: Value = serde_json::from_str(
            request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap()
                .1,
        )
        .unwrap();
        let mut step = self.0.lock().unwrap();
        let plan_revision = state["task"]["plan_revision"].clone();
        let id = state["run_guidance"]["current_todo"]["id"].clone();
        let calls = if *step < 3 {
            vec![ToolCall { id:format!("plan-{step}"),name:"task_plan".into(),arguments:json!({"action":"apply","expected_revision":plan_revision,"operations":[{"op":"update","id":id,"text":format!("Save section, plan revision {step}")}]}).to_string() }]
        } else if *step == 3 {
            assert_eq!(state["run_guidance"]["progress_recovery"]["active"], true);
            vec![ToolCall {
                id: "write".into(),
                name: "document_edit".into(),
                arguments:
                    json!({"action":"create","text":"# Result\nSaved the requested section.\n"})
                        .to_string(),
            }]
        } else if (4..=12).contains(&*step) {
            if *step == 4 {
                assert_eq!(state["run_guidance"]["progress_recovery"]["active"], false);
            }
            // Even more than the evidence retry allowance must not stop the
            // task just because the plan has not been marked complete yet.
            *step += 1;
            return Ok(Completion {
                text: "Premature completion".into(),
                ..Default::default()
            });
        } else if *step == 13 {
            assert!(
                state["run_guidance"]["completion_error"]
                    .as_str()
                    .unwrap()
                    .starts_with("task_plan_pending")
            );
            vec![ToolCall { id:"finish-item".into(),name:"task_plan".into(),arguments:json!({"action":"apply","expected_revision":plan_revision,"operations":[{"op":"complete","id":id,"result":"Saved docs/source-summary.md"}]}).to_string() }]
        } else if *step == 14 {
            // Advancing the list must discard the previous item's pending
            // warning, otherwise the next request can repeat completed work.
            assert!(state["run_guidance"]["completion_error"].is_null());
            assert_eq!(
                state["run_guidance"]["current_todo"]["text"],
                "Confirm the saved result"
            );
            vec![ToolCall { id:"finish-confirmation".into(),name:"task_plan".into(),arguments:json!({"action":"apply","expected_revision":plan_revision,"operations":[{"op":"complete","id":id,"result":"Confirmed the successful save result from document_edit"}]}).to_string() }]
        } else {
            *step += 1;
            return Ok(Completion {
                text: "Saved the completed document.".into(),
                ..Default::default()
            });
        };
        *step += 1;
        Ok(Completion {
            calls,
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn plan_churn_cannot_reset_recovery_and_pending_items_resume_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.stall_round_limit = 3;
    s.task.workflow = "document_edit".into();
    s.active_tools.insert("document_edit".into());
    s.add_user("Write the section".into());
    apply(
        &mut s,
        json!([{"op":"insert","texts":["Save the requested section", "Confirm the saved result"]}]),
    );
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    });
    let result = run_session(
        s,
        Arc::new(PlanChurn(Mutex::new(0))),
        CancellationToken::new(),
        tx,
    )
    .await;
    let events = drain.await.unwrap();
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert!(result.task.current_todo().is_none());
    assert_eq!(result.task.todos_completed_total, 2);
    assert!(dir.path().join("docs/source-summary.md").exists());
    assert!(!events.iter().any(|event|matches!(event,mnemoarc::agent::AgentEvent::Delta{text,..} if text == "Premature completion")));
}

struct EmptyPlanChurn(Mutex<usize>);
#[async_trait]
impl LlmClient for EmptyPlanChurn {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let state: Value = serde_json::from_str(
            request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap()
                .1,
        )
        .unwrap();
        let mut step = self.0.lock().unwrap();
        if *step == 3 {
            assert_eq!(state["run_guidance"]["progress_recovery"]["active"], true);
            assert_eq!(
                state["run_guidance"]["progress_recovery"]["rounds_without_progress"],
                3
            );
            return Ok(Completion {
                text: "Answer from existing information.".into(),
                ..Default::default()
            });
        }
        let id = format!("T{}", *step + 1);
        *step += 1;
        Ok(Completion {
            calls: vec![ToolCall {
                id: format!("churn-{step}"), name: "task_plan".into(),
                arguments: json!({"action":"apply","expected_revision":state["task"]["plan_revision"],"operations":[
                    {"op":"insert","texts":["Rephrase existing evidence"]},
                    {"op":"complete","id":id,"result":"Updated the wording"},
                    {"op":"remove","id":id,"reason":"Remove temporary plan entry"}
                ]}).to_string(),
            }], ..Default::default()
        })
    }
}

#[tokio::test]
async fn completing_and_removing_items_does_not_masquerade_as_actual_progress() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.stall_round_limit = 3;
    s.add_user("Explain the known information".into());
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        s,
        Arc::new(EmptyPlanChurn(Mutex::new(0))),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert!(result.task.todos.is_empty());
    assert_eq!(result.task.todos_completed_total, 3);
}
