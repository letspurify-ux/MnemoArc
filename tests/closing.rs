//! Closing mode: document work converges on a finished result, reporting
//! unresolved items instead of repeating until the run budget is spent.
use crate::support;
use anyhow::Result;
use async_trait::async_trait;
use mnemoarc::{
    agent::{AgentEvent, run_session},
    config::{Config, Project},
    llm::{Completion, LlmClient, ToolCall, Usage},
    session::{Closing, Session},
    tools::{self, ToolRegistry},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn fixture() -> (tempfile::TempDir, Session, Value) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.js"),
        "function run(history) {\n  const turns = normalize(history);\n  for (let i = 0; i < 5; i++) {\n    work(turns);\n  }\n}\n",
    )
    .unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            ..support::compact_config()
        },
    );
    s.add_user("Write a source document covering the loop and history handling.".into());
    s.select_workflow("source_document").unwrap();
    let read = tools::execute(&mut s, "file_read", json!({"path":"main.js"})).unwrap();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Flow\nA for loop runs work five times. main.js:3-5\n\n# History\nHistory is normalized first. main.js:2-2\n"}),
    )
    .unwrap();
    (dir, s, read["source"]["id"].clone())
}

fn tool_names(s: &Session) -> Vec<String> {
    ToolRegistry::definitions(s)
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn closing_mode_and_second_stall_stage_withhold_discovery_tools() {
    let (_dir, mut s, _) = fixture();
    s.active_tools = ToolRegistry::optional_names();
    let names = tool_names(&s);
    assert!(names.iter().any(|name| name == "source_search"));

    // Twice the stall limit without a better result narrows discovery.
    s.progress_recovery.rounds_since_best = s.config.stall_round_limit * 2;
    let names = tool_names(&s);
    assert!(!names.iter().any(|name| name == "source_search"));
    assert!(names.iter().any(|name| name == "file_read"));
    s.progress_recovery.rounds_since_best = 0;

    s.progress_recovery.closing = Some(Closing::default());
    let names = tool_names(&s);
    for blocked in [
        "file_list",
        "source_search",
        "symbol_search",
        "code_outline",
        "memory_write",
    ] {
        assert!(!names.iter().any(|name| name == blocked), "{blocked}");
    }
    for kept in ["file_read", "document_edit", "task_plan"] {
        assert!(names.iter().any(|name| name == kept), "{kept}");
    }
    let error = tools::execute(&mut s, "source_search", json!({"query":"normalize"})).unwrap_err();
    assert!(error.to_string().starts_with("closing_mode"));
    let result = tools::run_call(
        &mut s,
        &ToolCall {
            id: "closing-search".into(),
            name: "source_search".into(),
            arguments: json!({"query":"normalize"}).to_string(),
        },
    );
    assert_eq!(result["recovery"]["class"], "unavailable");
    assert!(tools::recovery::correctable_document_error(&result));
}

/// Appends a new paragraph each request (real progress) until closing mode is
/// announced, then answers. Records the tools offered during closing.
struct BudgetWriter {
    calls: Mutex<usize>,
    closing_tools: Mutex<Option<Vec<String>>>,
    path: std::path::PathBuf,
}

#[async_trait]
impl LlmClient for BudgetWriter {
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
        )?;
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        assert!(*calls < 80, "closing did not finish the run");
        let usage = Some(Usage {
            input: 20_000,
            output: 10,
            cached: None,
        });
        if state["run_guidance"]["closing"]["active"] == true {
            assert_eq!(state["run_guidance"]["closing"]["reason"], "budget");
            assert!(
                state["run_guidance"]["instruction"]
                    .as_str()
                    .unwrap()
                    .contains("Closing mode")
            );
            *self.closing_tools.lock().unwrap() = Some(
                request["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
                    .collect(),
            );
            return Ok(Completion {
                text: "Saved the document.".into(),
                usage,
                ..Default::default()
            });
        }
        let previous = std::fs::read(&self.path)?;
        Ok(Completion {
            calls: vec![ToolCall {
                id: format!("append-{calls}"),
                name: "document_edit".into(),
                arguments: json!({"action":"append","expected_hash":tools::hash(&previous),"text":format!("\nParagraph {calls}.\n")}).to_string(),
            }],
            usage,
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn closing_reserve_finishes_steady_work_before_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.md");
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: output.clone(),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            context_tokens: 128_000,
            output_tokens: 1024,
            run_tokens: 1_000_000,
            ..support::compact_config()
        },
    );
    s.add_user("Write out.md".into());
    s.active_tools = ToolRegistry::optional_names();
    s.select_workflow("source_document").unwrap();
    // A finished source document cites a source it read.
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    tools::execute(&mut s, "file_read", json!({"path":"main.rs"})).unwrap();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Result\nIntro. main.rs:1\n"}),
    )
    .unwrap();
    let client = Arc::new(BudgetWriter {
        calls: Mutex::new(0),
        closing_tools: Mutex::new(None),
        path: output,
    });
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move {
        let mut deltas = vec![];
        while let Some(event) = rx.recv().await {
            if let AgentEvent::Delta { text, .. } = event {
                deltas.push(text);
            }
        }
        deltas
    });
    let result = run_session(s, client.clone(), CancellationToken::new(), tx).await;
    let deltas = drain.await.unwrap();
    // Steady progress never trips the stall ladder; the 10% reserve starts
    // closing, and a clean final there completes normally.
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert!(result.completion_gaps.is_empty());
    assert_eq!(deltas, ["Saved the document."]);
    let closing_tools = client.closing_tools.lock().unwrap().clone().unwrap();
    assert!(!closing_tools.iter().any(|name| name == "source_search"));
    assert!(closing_tools.iter().any(|name| name == "document_edit"));
    let spent = result.input_tokens + result.output_tokens;
    assert!((900_000..1_000_000).contains(&spent), "{spent}");
}

#[test]
fn cleanup_output_reservation_is_bounded_so_large_outputs_keep_input_room() {
    use mnemoarc::context::{CLEANUP_OUTPUT_CAP, ContextManager};
    let large = Config {
        context_tokens: 230_000,
        output_tokens: 32_000,
        ..support::compact_config()
    };
    assert_eq!(
        ContextManager::cleanup_output_tokens(&large),
        CLEANUP_OUTPUT_CAP
    );
    // One full response plus three bounded cleanup rounds, not four outputs.
    let budget = ContextManager::input_budget(&large);
    assert!(budget > 130_000, "{budget}");
    let small = Config {
        context_tokens: 64_000,
        output_tokens: 8_000,
        ..support::compact_config()
    };
    assert_eq!(ContextManager::cleanup_output_tokens(&small), 8_000);
}

/// Scripted document run: two identical outline requests, then a final.
/// Records each request's run_guidance for assertions.
struct Scripted {
    steps: Mutex<Vec<Completion>>,
    guidance: Mutex<Vec<Value>>,
    tools: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl LlmClient for Scripted {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let state: Value = request["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .and_then(|message| message["content"].as_str())
            .and_then(|content| content.split_once('\n'))
            .and_then(|(_, json)| serde_json::from_str(json).ok())
            .unwrap_or(Value::Null);
        self.guidance
            .lock()
            .unwrap()
            .push(state["run_guidance"].clone());
        self.tools.lock().unwrap().push(
            request["tools"]
                .as_array()
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        );
        let mut steps = self.steps.lock().unwrap();
        if steps.is_empty() {
            anyhow::bail!("script_exhausted");
        }
        Ok(steps.remove(0))
    }
}

fn call(id: &str, name: &str, args: Value) -> Completion {
    Completion {
        calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.to_string(),
        }],
        ..Default::default()
    }
}

/// The fixture without the source ID of its read.
fn saved_fixture() -> (tempfile::TempDir, Session) {
    let (dir, s, _) = fixture();
    (dir, s)
}

async fn run_scripted(s: Session, steps: Vec<Completion>) -> (Session, Vec<Value>) {
    let (session, guidance, _) = run_scripted_tools(s, steps).await;
    (session, guidance)
}

async fn run_scripted_tools(
    s: Session,
    steps: Vec<Completion>,
) -> (Session, Vec<Value>, Vec<Vec<String>>) {
    let client = Arc::new(Scripted {
        steps: Mutex::new(steps),
        guidance: Mutex::new(vec![]),
        tools: Mutex::new(vec![]),
    });
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, client.clone(), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    let guidance = client.guidance.lock().unwrap().clone();
    let tools = client.tools.lock().unwrap().clone();
    (result, guidance, tools)
}

async fn run_scripted_notices(s: Session, steps: Vec<Completion>) -> (Session, Vec<String>) {
    let client = Arc::new(Scripted {
        steps: Mutex::new(steps),
        guidance: Mutex::new(vec![]),
        tools: Mutex::new(vec![]),
    });
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move {
        let mut notices = Vec::new();
        while let Some(event) = rx.recv().await {
            if let mnemoarc::agent::AgentEvent::Notice { text, .. } = event {
                notices.push(text);
            }
        }
        notices
    });
    let result = run_session(s, client, CancellationToken::new(), tx).await;
    (result, drain.await.unwrap())
}

#[tokio::test]
async fn unchanged_outline_repeat_returns_a_short_marker() {
    let (_dir, s) = saved_fixture();
    let (result, _) = run_scripted(
        s,
        vec![
            call("inspect-1", "document_inspect", json!({})),
            call("inspect-2", "document_inspect", json!({})),
            Completion {
                text: "Saved out.md".into(),
                ..Default::default()
            },
        ],
    )
    .await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    let outputs: Vec<Value> = result
        .history
        .bundles
        .iter()
        .flat_map(|bundle| &bundle.messages)
        .filter(|message| message["role"] == "tool")
        .map(|message| serde_json::from_str(message["content"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(outputs.len(), 2);
    assert!(outputs[0]["data"]["outline"].is_array());
    assert_eq!(outputs[1]["data"]["unchanged"], true);
    assert!(outputs[1]["data"]["outline"].is_null());
}

#[tokio::test]
async fn finished_bookkeeping_leaves_the_final_answer_to_the_model() {
    // A "ready to finish" instruction as soon as the citations were read and
    // the to-dos closed let concise models stop after their first sections.
    // Outside closing the model judges when the document is complete.
    let (_dir, s) = saved_fixture();
    let (result, guidance) = run_scripted(
        s,
        vec![Completion {
            text: "Saved out.md".into(),
            ..Default::default()
        }],
    )
    .await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert!(guidance[0]["ready_for_final"].is_null());
    let instruction = guidance[0]["instruction"].as_str().unwrap();
    assert!(!instruction.contains("Ready to finish"), "{instruction}");
    assert!(
        instruction.contains("when the document is complete"),
        "{instruction}"
    );

    // A cited range that was never read keeps the ordinary guidance.
    let (_dir, mut s, _) = fixture();
    std::fs::write(s.project.root.join("extra.js"), "export const x = 1;\n").unwrap();
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":hash,"text":"\n# Extra\nUnread. extra.js:1\n"}),
    )
    .unwrap();
    let (_, guidance) = run_scripted(
        s,
        vec![Completion {
            text: "Saved out.md".into(),
            ..Default::default()
        }],
    )
    .await;
    assert!(guidance[0]["ready_for_final"].is_null());
}

#[tokio::test]
async fn open_todos_on_a_finished_document_are_closed_in_one_batch() {
    let (_dir, mut s) = saved_fixture();
    // The live shape: the document is done but three plan items remain,
    // which previously cost a request each.
    tools::execute(
        &mut s,
        "task_plan",
        json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Read sources","Write sections","Verify sections"]}]}),
    )
    .unwrap();
    let operations: Vec<Value> = ["T1", "T2", "T3"]
        .iter()
        .map(|id| json!({"op":"complete","id":id,"result":"Done in out.md"}))
        .collect();
    let (result, guidance) = run_scripted(
        s,
        vec![
            call(
                "closeout",
                "task_plan",
                json!({"action":"apply","expected_revision":1,"operations":operations}),
            ),
            Completion {
                text: "Saved out.md".into(),
                ..Default::default()
            },
        ],
    )
    .await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    let closeout = &guidance[0]["plan_closeout"];
    assert_eq!(closeout["expected_revision"], 1);
    assert_eq!(closeout["pending_count"], 3);
    assert_eq!(closeout["items"][0]["id"], "T1");
    assert!(guidance[0]["ready_for_final"].is_null());
    let instruction = guidance[0]["instruction"].as_str().unwrap();
    assert!(instruction.contains("close them in ONE task_plan apply"));
    // Settled items cover only registered work; an unstarted to-do such as a
    // missing section must not be removed as obsolete.
    assert!(instruction.contains("NOT obsolete"), "{instruction}");
    // One batched update was enough; the model then answers on its own.
    assert!(guidance[1]["ready_for_final"].is_null());
    assert!(guidance[1]["plan_closeout"].is_null());
    assert!(result.task.current_todo().is_none());
}

#[test]
fn provider_usage_calibrates_estimated_token_counts() {
    use mnemoarc::context::ContextManager;
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "unknown-provider-model".into(),
            model_context: Some(140_000),
            context_tokens: 140_000,
            output_tokens: 12_000,
            ..support::compact_config()
        },
    );
    assert_eq!(ContextManager::token_ratio(&s), 1.0);
    // Enough complete history groups to evict.
    for i in 0..6 {
        s.history.push(
            vec![json!({"role":"user","content":format!("{i} {}", "context ".repeat(2000))})],
            true,
        );
    }
    let budget = ContextManager::input_budget(&s.config);
    let request = budget * 85 / 100;
    // Above the 80% high-water mark by the uncalibrated estimate.
    let mut uncalibrated = s.clone();
    assert!(ContextManager::prepare(&mut uncalibrated, request).unwrap());
    // The provider measured 20% fewer tokens: the same request fits.
    for _ in 0..3 {
        ContextManager::record_usage(&mut s, 10_000, 8_000);
    }
    assert!((ContextManager::token_ratio(&s) - 0.8).abs() < 1e-9);
    assert_eq!(ContextManager::calibrated(&s, 10_000), 8_000);
    assert!(!ContextManager::prepare(&mut s, request).unwrap());
    // The highest recent ratio wins, so a larger measurement is never hidden.
    ContextManager::record_usage(&mut s, 10_000, 11_000);
    assert!((ContextManager::token_ratio(&s) - 1.1).abs() < 1e-9);
    // Known tokenizers are exact and never calibrated.
    s.config.model = "gpt-4o".into();
    ContextManager::record_usage(&mut s, 10_000, 5_000);
    assert_eq!(ContextManager::token_ratio(&s), 1.0);
}

#[test]
fn closing_without_a_document_withholds_reading_until_it_is_written() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.js"), "run();\n").unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            ..support::compact_config()
        },
    );
    s.add_user("Write out.md about main.js".into());
    s.active_tools = ToolRegistry::optional_names();
    s.progress_recovery.closing = Some(Closing::default());
    let names = tool_names(&s);
    for withheld in ["file_read", "symbol_read", "source_search"] {
        assert!(!names.iter().any(|name| name == withheld), "{withheld}");
    }
    assert!(names.iter().any(|name| name == "document_edit"));
    let error = tools::execute(&mut s, "file_read", json!({"path":"main.js"}))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("withheld until the document is saved"),
        "{error}"
    );
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Main\nIt runs. main.js:1-1\n"}),
    )
    .unwrap();
    // With a saved document, a targeted read of a cited range is allowed again.
    assert!(tool_names(&s).iter().any(|name| name == "file_read"));
    tools::execute(&mut s, "file_read", json!({"path":"main.js"})).unwrap();
}

#[tokio::test]
async fn reading_new_sources_before_the_first_write_is_progress() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.js"), numbered(40)).unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            context_tokens: 128_000,
            output_tokens: 1_024,
            stall_round_limit: 2,
            ..support::compact_config()
        },
    );
    s.add_user("Document main.js with source evidence".into());
    s.select_workflow("source_document").unwrap();
    // Twelve distinct ranges: four times the closing threshold (3 x 2).
    let mut steps: Vec<Completion> = (0..12)
        .map(|i| {
            call(
                &format!("read-{i}"),
                "file_read",
                json!({"path":"main.js","start_line":i * 3 + 1,"max_lines":3}),
            )
        })
        .collect();
    steps.push(call(
        "write",
        "document_edit",
        json!({"action":"create","text":"# Main\nForty values. main.js:1-40\n"}),
    ));
    let (result, guidance) = run_scripted(s, steps).await;
    assert!(
        result.document_written,
        "{:?} {:?}",
        result.status, result.last_error
    );
    assert!(
        guidance.iter().take(13).all(|g| g["closing"].is_null()),
        "{:?}",
        guidance.iter().map(|g| &g["closing"]).collect::<Vec<_>>()
    );
    // Nor did the reads move the guidance to drafting or stall recovery.
    assert!(guidance.iter().take(13).all(|g| g["phase"] != "draft"));
    assert!(
        guidance
            .iter()
            .take(13)
            .all(|g| g["progress_recovery"]["active"] == false)
    );
}

fn numbered(lines: usize) -> String {
    (1..=lines).map(|i| format!("let v{i} = {i};\n")).collect()
}

#[tokio::test]
async fn discovery_tools_stay_available_until_the_document_exists() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("backend/src")).unwrap();
    std::fs::write(dir.path().join("backend/src/agent.js"), numbered(40)).unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            context_tokens: 128_000,
            output_tokens: 1_024,
            stall_round_limit: 2,
            ..support::compact_config()
        },
    );
    s.add_user("Document backend/src/agent.js with source evidence".into());
    s.active_tools = ToolRegistry::optional_names();
    s.select_workflow("source_document").unwrap();
    // Wrong guesses first (no new evidence), then real distinct reads: both
    // stretches exceed the stall limit before any document is written.
    let mut steps: Vec<Completion> = (0..5)
        .map(|i| {
            call(
                &format!("guess-{i}"),
                "file_read",
                json!({"path":format!("backend/agent{i}.py")}),
            )
        })
        .collect();
    steps.extend((0..6).map(|i| {
        call(
            &format!("read-{i}"),
            "file_read",
            json!({"path":"backend/src/agent.js","start_line":i * 5 + 1,"max_lines":5}),
        )
    }));
    let (_, _, offered) = run_scripted_tools(s, steps).await;
    assert!(offered.len() >= 11);
    for (round, names) in offered.iter().take(11).enumerate() {
        for discovery in ["file_list", "source_search"] {
            assert!(
                names.iter().any(|name| name == discovery),
                "{discovery} withheld at request {round} before the document existed"
            );
        }
    }
}

#[test]
fn missing_paths_suggest_similar_project_files_and_directories_list_entries() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("backend/src")).unwrap();
    std::fs::write(dir.path().join("backend/src/agent.js"), "run();\n").unwrap();
    std::fs::write(dir.path().join("backend/src/server.js"), "listen();\n").unwrap();
    std::fs::write(dir.path().join("README.md"), "# App\n").unwrap();
    std::fs::write(dir.path().join(".env"), "SECRET=1\n").unwrap();
    std::fs::create_dir_all(dir.path().join("backend/src/routes")).unwrap();
    std::fs::write(dir.path().join("backend/src/routes/chat.js"), "route();\n").unwrap();
    std::fs::write(dir.path().join("backend/src/app.credentials.json"), "{}\n").unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            exclude: vec![".env*".into(), "*.credentials.json".into()],
            ..Default::default()
        },
        support::compact_config(),
    );
    // Same stem, different extension.
    let error = tools::execute(&mut s, "file_read", json!({"path":"backend/agent.py"}))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("file_not_found"), "{error}");
    assert!(error.contains("backend/src/agent.js"), "{error}");
    // Same file name under a wrong prefix.
    let error = tools::execute(&mut s, "file_read", json!({"path":"llm_agent/README.md"}))
        .unwrap_err()
        .to_string();
    assert!(error.contains("README.md. Copy one exactly"), "{error}");
    // Nothing similar: point to file_list. Excluded files are never offered.
    let error = tools::execute(&mut s, "file_read", json!({"path":"main.py"}))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("No project file or directory has this name"),
        "{error}"
    );
    let error = tools::execute(&mut s, "file_read", json!({"path":"config/.env"}))
        .unwrap_err()
        .to_string();
    assert!(!error.contains("Copy one exactly"), "{error}");
    // A directory read shows what it contains.
    let error = tools::execute(&mut s, "file_read", json!({"path":"backend/src"}))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("path_is_directory"), "{error}");
    assert!(
        error.contains("containing agent.js, routes/, server.js;"),
        "{error}"
    );
    // Excluded files are not named either, whichever tool hit the directory.
    s.active_tools = ToolRegistry::optional_names();
    for (tool, args) in [
        ("file_read", json!({"path":"backend/src"})),
        ("document_inspect", json!({"path":"backend/src"})),
        ("code_outline", json!({"path":"backend/src"})),
    ] {
        let error = tools::execute(&mut s, tool, args).unwrap_err().to_string();
        assert!(error.starts_with("path_is_directory"), "{tool}: {error}");
        assert!(error.contains("agent.js"), "{tool}: {error}");
        assert!(!error.contains("credentials"), "{tool}: {error}");
    }
}

#[test]
fn source_search_accepts_a_directory_as_its_scope() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("backend/src")).unwrap();
    std::fs::write(
        dir.path().join("backend/src/server.js"),
        "app.post('/api/chat', chat);\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("other.js"),
        "app.post('/api/chat', other);\n",
    )
    .unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        support::compact_config(),
    );
    let result = tools::execute(
        &mut s,
        "source_search",
        json!({"path":"backend","query":"/api/chat"}),
    )
    .unwrap();
    let matches = result["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "{result}");
    assert!(
        matches[0]["path"]
            .as_str()
            .unwrap()
            .ends_with("backend/src/server.js")
    );
}

#[test]
fn list_cursor_keeps_its_scope_when_the_glob_is_omitted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    for i in 0..120 {
        std::fs::write(dir.path().join(format!("src/m{i:03}.js")), "x();\n").unwrap();
    }
    std::fs::write(dir.path().join("README.md"), "# App\n").unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        support::compact_config(),
    );
    let first = tools::execute(
        &mut s,
        "file_list",
        json!({"mode":"paths","path_glob":"src/**"}),
    )
    .unwrap();
    assert_eq!(first["total_files"], 120);
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    // The continuation drops path_glob, as the live model did.
    let second =
        tools::execute(&mut s, "file_list", json!({"mode":"paths","cursor":cursor})).unwrap();
    let rest = second["paths"].as_array().unwrap();
    assert_eq!(rest.len(), 20);
    assert!(
        rest.iter()
            .all(|path| path.as_str().unwrap().starts_with("src/"))
    );
    // A different explicit scope is a real mismatch, explained as such.
    let error = tools::execute(
        &mut s,
        "file_list",
        json!({"mode":"paths","path_glob":"**","cursor":cursor}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("issued for different arguments"), "{error}");
}

#[test]
fn a_truncated_checkpoint_id_still_acknowledges_the_checkpoint() {
    use mnemoarc::context::ContextManager;
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(64_000),
            context_tokens: 64_000,
            output_tokens: 4_000,
            ..support::compact_config()
        },
    );
    s.add_user("Work".into());
    for i in 0..6 {
        s.history.push(
            vec![json!({"role":"user","content":format!("{i} {}", "context ".repeat(2500))})],
            true,
        );
    }
    let budget = ContextManager::input_budget(&s.config);
    assert!(ContextManager::prepare(&mut s, budget).unwrap());
    let id = s.checkpoint.as_ref().unwrap().id.clone();
    let ack = |id: &str| json!({"id":id,"progress":"Continue the work","no_save_reason":"Nothing new to save"});
    // Too short to identify anything.
    let error = tools::execute(&mut s, "checkpoint_complete", ack(&id[..6]))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("checkpoint_id_mismatch"), "{error}");
    // The model cut the last characters of the 36-character ID.
    let result = tools::execute(&mut s, "checkpoint_complete", ack(&id[..30])).unwrap();
    assert_eq!(result["acknowledged"], true, "{result}");
}

#[test]
fn a_checkpoint_id_with_a_one_character_typo_still_acknowledges_the_checkpoint() {
    use mnemoarc::context::ContextManager;
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(64_000),
            context_tokens: 64_000,
            output_tokens: 4_000,
            ..support::compact_config()
        },
    );
    s.add_user("Work".into());
    for i in 0..6 {
        s.history.push(
            vec![json!({"role":"user","content":format!("{i} {}", "context ".repeat(2500))})],
            true,
        );
    }
    let budget = ContextManager::input_budget(&s.config);
    assert!(ContextManager::prepare(&mut s, budget).unwrap());
    let id = s.checkpoint.as_ref().unwrap().id.clone();
    let ack = |id: &str| json!({"id":id,"progress":"Continue the work","no_save_reason":"Nothing new to save"});
    let flip = |id: &str, at: &[usize]| -> String {
        id.char_indices()
            .map(|(i, c)| match (at.contains(&i), c) {
                (false, c) => c,
                (true, '0') => '1',
                (true, '-') => '-',
                (true, _) => '0',
            })
            .collect()
    };
    // Three differing characters is another ID, not a typo.
    let error = tools::execute(&mut s, "checkpoint_complete", ack(&flip(&id, &[0, 1, 2])))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("checkpoint_id_mismatch"), "{error}");
    // Live run: the model kept copying ...-482f-... for ...-482c-....
    let result = tools::execute(&mut s, "checkpoint_complete", ack(&flip(&id, &[15]))).unwrap();
    assert_eq!(result["acknowledged"], true, "{result}");
}

#[tokio::test]
async fn transient_empty_replies_are_retried_before_any_workflow_is_set() {
    let plain = || {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("out.md"),
                ..Default::default()
            },
            Config {
                model: "gpt-4o".into(),
                model_context: Some(128_000),
                context_tokens: 128_000,
                output_tokens: 1_024,
                ..support::compact_config()
            },
        );
        s.add_user("What does this project do?".into());
        (dir, s)
    };
    let (_dir, s) = plain();
    let (result, _) = run_scripted(
        s,
        vec![
            Completion::default(),
            Completion::default(),
            Completion {
                text: "It serves an API.".into(),
                ..Default::default()
            },
        ],
    )
    .await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    // Persistent empty replies still stop the run instead of looping.
    let (_dir, s) = plain();
    let (result, _) = run_scripted(s, vec![Completion::default(); 3]).await;
    assert_eq!(result.status, "blocked");
    assert!(
        result
            .last_error
            .as_deref()
            .unwrap()
            .starts_with("empty_completion")
    );
}

#[tokio::test]
async fn an_output_limit_truncation_withholds_whole_document_writes() {
    let truncated_then_final = || {
        vec![
            Completion {
                length_limited: true,
                discarded_tool_calls: true,
                ..Default::default()
            },
            Completion {
                text: "Saved out.md".into(),
                ..Default::default()
            },
        ]
    };
    // A document small relative to the output limit cannot be the cause:
    // whole writes stay available.
    let (_small_dir, small) = saved_fixture();
    let (result, _) = run_scripted(small, truncated_then_final()).await;
    assert!(!result.progress_recovery.whole_write_withheld);

    let (dir, mut s) = saved_fixture();
    let filler: String = (1..=600)
        .map(|i| format!("Detail line {i} explains one more step of the flow.\n"))
        .collect();
    let hash = tools::hash(&std::fs::read(dir.path().join("out.md")).unwrap());
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":hash,"text":format!("\n# Details\n{filler}")}),
    )
    .unwrap();
    let (result, _) = run_scripted(
        s,
        vec![
            Completion {
                length_limited: true,
                discarded_tool_calls: true,
                ..Default::default()
            },
            Completion {
                text: "Saved out.md".into(),
                ..Default::default()
            },
        ],
    )
    .await;
    assert!(result.progress_recovery.whole_write_withheld);
    let mut s = result;
    // The schema no longer offers write, and a write call is refused.
    let edit = ToolRegistry::definitions(&s)
        .into_iter()
        .find(|tool| tool["function"]["name"] == "document_edit")
        .unwrap();
    let actions = edit["function"]["parameters"]["properties"]["action"]["enum"].clone();
    assert!(
        !actions.as_array().unwrap().iter().any(|a| a == "write"),
        "{actions}"
    );
    assert!(actions.as_array().unwrap().iter().any(|a| a == "section"));
    let path = dir.path().join("out.md");
    let hash = tools::hash(&std::fs::read(&path).unwrap());
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"write","expected_hash":hash,"text":"# Flow\\nRewritten.\\n"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("whole_write_withheld"), "{error}");
    let error = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":hash,"edits":[{"action":"write","text":"# Flow\\nRewritten.\\n"}]}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("whole_write_withheld"), "{error}");
    // A section-sized edit succeeds and restores whole writes.
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":hash,"old_text":"History is normalized first.","text":"History is normalized before the loop."}),
    )
    .unwrap();
    assert!(!s.progress_recovery.whole_write_withheld);
}

#[tokio::test]
async fn progress_recovery_verify_does_not_persist_into_the_task_phase() {
    let (_dir, mut s) = saved_fixture();
    // Ten idle rounds sit near the default 64K high-water mark; a memory
    // checkpoint would suspend progress recovery, which is not under test.
    s.config.context_tokens = 128_000;
    let steps = (0..10)
        .map(|i| call(&format!("idle-{i}"), "task_state", json!({"action":"read"})))
        .collect();
    let (result, guidance) = run_scripted(s, steps).await;
    // Idle rounds trigger recovery, which steers requests toward verification.
    assert!(
        guidance
            .iter()
            .any(|step| step["phase"] == "verify" && step["progress_recovery"]["active"] == true),
        "{guidance:?}"
    );
    // The stored task phase keeps the budget phase, so the next request after
    // recovery can register and investigate a new section again.
    assert_ne!(result.task.phase, "verify");
}

#[tokio::test]
async fn closing_on_a_finished_result_asks_for_the_final_answer() {
    let (_dir, mut s) = saved_fixture();
    s.config.stall_round_limit = 2;
    let steps = (0..10)
        .map(|i| {
            call(
                &format!("audit-{i}"),
                "task_state",
                json!({"action":"read"}),
            )
        })
        .collect();
    let (_, guidance) = run_scripted(s, steps).await;
    let closing = guidance
        .iter()
        .find(|step| step["closing"]["active"] == true)
        .expect("stalled idle rounds enter closing mode");
    assert_eq!(closing["ready_for_final"], true, "{closing}");
    assert!(
        closing["instruction"]
            .as_str()
            .unwrap()
            .contains("this is the time to answer"),
        "{closing}"
    );
}

/// Returns `empties` empty replies, then a final answer; records requests.
struct EmptyThenFinal {
    empties: usize,
    requests: Mutex<Vec<Value>>,
}

#[async_trait]
impl LlmClient for EmptyThenFinal {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(request);
        if requests.len() <= self.empties {
            return Ok(Completion::default());
        }
        Ok(Completion {
            text: "Saved out.md".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn repeated_empty_document_replies_change_the_request_and_close() {
    // A live run repeated one identical request for 24 rounds: document
    // recovery retried empty replies without changing anything until the
    // stall ladder closed the run.
    let (_dir, mut s) = saved_fixture();
    s.progress_recovery.action_required = true;
    let client = Arc::new(EmptyThenFinal {
        empties: 3,
        requests: Mutex::new(vec![]),
    });
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move {
        let mut notices = Vec::new();
        while let Some(event) = rx.recv().await {
            if let mnemoarc::agent::AgentEvent::Notice { text, .. } = event {
                notices.push(text);
            }
        }
        notices
    });
    let result = run_session(s, client.clone(), CancellationToken::new(), tx).await;
    let notices = drain.await.unwrap();
    let requests = client.requests.lock().unwrap();
    assert_eq!(requests.len(), 4, "{:?}", result.last_error);
    assert!(
        result.status.starts_with("complete"),
        "{:?}",
        result.last_error
    );
    // The first request requires a tool; after an empty reply the model may
    // answer either way, and each retry differs from the previous request.
    assert_eq!(requests[0]["tool_choice"], "required");
    for pair in requests.windows(2) {
        assert!(pair[1].get("tool_choice").is_none());
        assert_ne!(pair[0]["messages"], pair[1]["messages"]);
    }
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("returned 1 empty response(s) in a row")
    );
    assert_eq!(
        result
            .progress_recovery
            .closing
            .as_ref()
            .map(|c| c.reason.as_str()),
        Some("empty_response")
    );
    assert!(
        notices.iter().any(|n| n.contains("빈 응답을 반복해")),
        "{notices:?}"
    );
    assert!(
        notices.iter().any(|n| n.contains("저장된 문서로 마무리")),
        "{notices:?}"
    );
}

#[tokio::test]
async fn repeated_empty_replies_without_a_document_report_creation_retry() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            ..support::compact_config()
        },
    );
    s.add_user("Write a source manual.".into());
    s.select_workflow("source_document").unwrap();
    let (result, notices) = run_scripted_notices(s, vec![Completion::default(); 16]).await;

    assert_eq!(result.status, "blocked");
    assert_eq!(
        result.run_history.back().unwrap().reason,
        "closing_round_limit"
    );
    assert!(!result.document_written);
    assert!(!result.project.output.exists());
    let notice = notices
        .iter()
        .find(|n| n.contains("빈 응답을 반복해"))
        .expect("empty replies enter closing mode");
    assert!(notice.contains("문서 생성을 재시도"), "{notice}");
    assert!(!notice.contains("저장된 문서"), "{notice}");
}

#[tokio::test]
async fn reading_new_sources_before_the_first_save_is_not_a_stall() {
    // Requests without an output change used to switch the run to a result
    // focus with a stall notice, while a live model was still reading a new
    // file in each of those requests.
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            ..support::compact_config()
        },
    );
    s.add_user("Write a source manual.".into());
    s.select_workflow("source_document").unwrap();
    // Below the compact context, where a checkpoint starts after six reads.
    s.config.stall_round_limit = 3;
    let limit = s.config.stall_round_limit;
    let steps = (0..limit)
        .map(|i| {
            std::fs::write(
                dir.path().join(format!("f{i}.rs")),
                format!("fn f{i}() {{}}\n"),
            )
            .unwrap();
            call(
                &format!("read-{i}"),
                "file_read",
                json!({"path":format!("f{i}.rs")}),
            )
        })
        .collect();
    let (_, notices) = run_scripted_notices(s, steps).await;
    assert!(
        !notices
            .iter()
            .any(|n| n.contains(&format!("요청 {limit}번")) || n.contains("repeating")),
        "{notices:?}"
    );
}

#[tokio::test]
async fn new_sources_are_progress_after_the_first_save_in_any_phase() {
    // New reads counted only while investigating, before the first save,
    // during a repair or while citations were unread. Reading more of the
    // project in the verify phase of a saved document looked like a stall.
    let (dir, mut s) = saved_fixture();
    s.task.phase = "verify".into();
    let steps = (0..4)
        .map(|i| {
            std::fs::write(
                dir.path().join(format!("more{i}.js")),
                format!("export const more{i} = {i};\n"),
            )
            .unwrap();
            call(
                &format!("read-{i}"),
                "file_read",
                json!({"path":format!("more{i}.js")}),
            )
        })
        .collect();
    let (result, guidance) = run_scripted(s, steps).await;
    assert!(result.document_written);
    assert_eq!(guidance[0]["phase"], "verify", "{}", guidance[0]);
    assert_eq!(guidance[0]["unread_citation_count"], 0, "{}", guidance[0]);
    for g in &guidance[1..4] {
        assert_eq!(g["progress_recovery"]["rounds_since_progress"], 0, "{g}");
    }
}

#[tokio::test]
async fn rereading_delivered_lines_or_the_output_is_not_new_evidence() {
    // Each new view of a file counted as new evidence: a live run re-read
    // its own output and subranges of files it had read for 25 requests,
    // and every stall check was reset each time.
    let (dir, mut s) = saved_fixture();
    s.task.phase = "verify".into();
    std::fs::write(dir.path().join("more.js"), "export const more = 1;\n").unwrap();
    let steps = vec![
        call("output", "file_read", json!({"path":"out.md"})),
        call(
            "subrange",
            "file_read",
            json!({"path":"main.js","start_line":2,"max_lines":3}),
        ),
        call(
            "output-line",
            "file_read",
            json!({"path":"out.md","start_line":2,"max_lines":1}),
        ),
        call("new", "file_read", json!({"path":"more.js"})),
    ];
    let (result, guidance) = run_scripted(s, steps).await;
    assert!(result.document_written);
    let counts = |field: &str| -> Vec<Value> {
        guidance
            .iter()
            .take(5)
            .map(|g| g["progress_recovery"][field].clone())
            .collect()
    };
    assert_eq!(
        counts("rounds_since_progress"),
        [json!(0), json!(1), json!(2), json!(3), json!(0)]
    );
    assert_eq!(
        counts("rounds_without_substantive_progress"),
        [json!(0), json!(1), json!(2), json!(3), json!(0)]
    );
}

#[tokio::test]
async fn edits_grounded_in_new_sources_are_not_edits_without_progress() {
    // Same-length refinements counted as edits without progress unless they
    // added a section or lines, even right after reading a new source; the
    // sixteenth sent the run into focused recovery.
    let (dir, mut s) = saved_fixture();
    // Room for eighteen read-and-edit requests without a checkpoint.
    s.config.model_context = Some(400_000);
    s.config.context_tokens = 300_000;
    let output = s.project.output.clone();
    let hash = tools::hash(&std::fs::read(&output).unwrap());
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":hash,"text":"\nVersion 0.\n"}),
    )
    .unwrap();
    let mut doc = std::fs::read_to_string(&output).unwrap();
    let mut steps = Vec::new();
    for i in 1..=18 {
        std::fs::write(
            dir.path().join(format!("more{i}.js")),
            format!("export const more{i} = {i};\n"),
        )
        .unwrap();
        let hash = tools::hash(doc.as_bytes());
        let mut step = call(
            &format!("read-{i}"),
            "file_read",
            json!({"path":format!("more{i}.js")}),
        );
        step.calls.push(mnemoarc::llm::ToolCall {
            id: format!("refine-{i}"),
            name: "document_edit".into(),
            arguments: json!({"action":"replace_text","expected_hash":hash,
                "old_text":format!("Version {}.", i - 1),"text":format!("Version {i}.")})
            .to_string(),
        });
        steps.push(step);
        doc = doc.replace(&format!("Version {}.", i - 1), &format!("Version {i}."));
    }
    let (_, guidance) = run_scripted(s, steps).await;
    assert!(guidance.len() > 17, "{}", guidance.len());
    for g in &guidance {
        assert_eq!(
            g["progress_recovery"]["artifact_edits_without_milestone"], 0,
            "{g}"
        );
        assert_eq!(g["progress_recovery"]["focused"], false, "{g}");
    }
}

/// Requests that read a new file each, after the given steps.
fn new_file_reads(dir: &std::path::Path, from: usize, count: usize) -> Vec<Completion> {
    (from..from + count)
        .map(|i| {
            std::fs::write(
                dir.join(format!("extra{i}.js")),
                format!("export const extra{i} = {i};\n"),
            )
            .unwrap();
            call(
                &format!("read-extra-{i}"),
                "file_read",
                json!({"path":format!("extra{i}.js")}),
            )
        })
        .collect()
}

/// Indices of the requests whose run_guidance.plan_check starts with `text`.
fn plan_checks(guidance: &[Value], text: &str) -> Vec<usize> {
    guidance
        .iter()
        .enumerate()
        .filter(|(_, g)| {
            g["plan_check"]
                .as_str()
                .is_some_and(|c| c.starts_with(text))
        })
        .map(|(i, _)| i)
        .collect()
}

#[tokio::test]
async fn an_empty_plan_of_document_work_is_pointed_out_once() {
    // A live run's first plan call was rejected and it worked for 100
    // requests without a plan; nothing pointed that out.
    let (dir, s) = saved_fixture();
    let steps = new_file_reads(dir.path(), 0, 6);
    let (_, guidance) = run_scripted(s, steps).await;
    let shown = plan_checks(&guidance, "task_plan is empty");
    assert_eq!(shown.len(), 1, "{guidance:?}");
    assert!(shown[0] >= 2, "{shown:?}");
}

#[tokio::test]
async fn a_plan_that_falls_behind_the_work_is_pointed_out_once_per_state() {
    let (dir, s) = saved_fixture();
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    let mut steps = vec![
        call(
            "plan",
            "task_plan",
            json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Write the usage section","Verify the document"]}]}),
        ),
        call(
            "usage",
            "document_edit",
            json!({"action":"append","expected_hash":hash,"text":"\n# Usage\nRun it with a history. main.js:1-1\n"}),
        ),
    ];
    steps.extend(new_file_reads(dir.path(), 0, 2));
    steps.push(call(
        "close",
        "task_plan",
        json!({"action":"apply","expected_revision":1,"operations":[
            {"op":"complete","id":"T1","result":"Saved the usage section"},
            {"op":"complete","id":"T2","result":"Checked the saved document"}]}),
    ));
    steps.extend(new_file_reads(dir.path(), 2, 2));
    let (_, guidance) = run_scripted(s, steps).await;
    // The saved section left T1 open: pointed out once, not on every request.
    let section = plan_checks(
        &guidance,
        "A section was saved while current_todo T1 stayed open",
    );
    assert_eq!(section, [2], "{guidance:?}");
    // Every item done: the model judges whether something is missing.
    let done = plan_checks(&guidance, "Every item in task_plan is done");
    assert_eq!(done, [5], "{guidance:?}");
}

#[tokio::test]
async fn a_to_do_that_stays_current_while_work_moves_on_is_pointed_out_once() {
    let (dir, mut s) = saved_fixture();
    s.config.stall_round_limit = 3;
    let mut steps = vec![call(
        "plan",
        "task_plan",
        json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Read the extra modules","Write their section"]}]}),
    )];
    steps.extend(new_file_reads(dir.path(), 0, 6));
    let (_, guidance) = run_scripted(s, steps).await;
    let stale = plan_checks(&guidance, "current_todo T1 has been current for 3 requests");
    assert_eq!(stale.len(), 1, "{guidance:?}");
    assert_eq!(
        plan_checks(&guidance, "current_todo T1").len(),
        1,
        "{guidance:?}"
    );
}

/// Fills the history with complete groups a checkpoint may evict.
fn pad_history(s: &mut Session, groups: usize) {
    for i in 0..groups {
        s.history.push(
            vec![json!({"role":"user","content":format!("{i} {}", "context ".repeat(1000))})],
            true,
        );
    }
}

#[test]
fn closing_starts_a_checkpoint_only_when_the_request_no_longer_fits() {
    use mnemoarc::context::ContextManager;
    let (_dir, mut s) = saved_fixture();
    pad_history(&mut s, 12);
    let budget = ContextManager::input_budget(&s.config);
    let above_high_water = (budget as f64 * (1.0 + s.config.high_water) / 2.0) as usize;
    let mut steady = s.clone();
    assert!(ContextManager::prepare(&mut steady, above_high_water).unwrap());
    // Closing has a few bounded requests left; crossing high-water alone
    // would spend them on cleanup the run may never use.
    s.progress_recovery.closing = Some(Closing::default());
    let mut closing = s.clone();
    assert!(!ContextManager::prepare(&mut closing, above_high_water).unwrap());
    assert!(closing.checkpoint.is_none());
    assert!(ContextManager::prepare(&mut s, budget + 1).unwrap());
}

/// Spends the run budget on its first reply so closing starts, keeps working
/// in closing, grows the context past the input budget on the request before
/// the last, and answers once a request arrives after the checkpoint.
struct ClosingCheckpoint {
    pad_tokens: usize,
    closing_rounds: Mutex<Vec<u64>>,
    checkpoints: Mutex<usize>,
}

#[async_trait]
impl LlmClient for ClosingCheckpoint {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let state: Value = request["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .and_then(|message| message["content"].as_str())
            .and_then(|content| content.split_once('\n'))
            .and_then(|(_, json)| serde_json::from_str(json).ok())
            .unwrap_or(Value::Null);
        if let Some(id) = state["checkpoint"]["id"].as_str() {
            *self.checkpoints.lock().unwrap() += 1;
            return Ok(call(
                &format!("ack-{id}"),
                "checkpoint_complete",
                json!({"id":id,"progress":"Finish the document","no_save_reason":"Nothing new to save"}),
            ));
        }
        let closing = &state["run_guidance"]["closing"];
        let Some(rounds) = closing["rounds"].as_u64() else {
            let mut first = call("read-0", "file_read", json!({"path":"main.js"}));
            first.usage = Some(Usage {
                input: 9_200_000,
                output: 0,
                cached: None,
            });
            return Ok(first);
        };
        let mut seen = self.closing_rounds.lock().unwrap();
        seen.push(rounds);
        if *self.checkpoints.lock().unwrap() > 0 {
            return Ok(Completion {
                text: "Saved out.md".into(),
                ..Default::default()
            });
        }
        let limit = closing["round_limit"].as_u64().unwrap();
        let line = 1 + seen.len() % 6;
        // Document work keeps only the calls in history, so the context
        // grows through a (rejected) argument rather than reply prose.
        let mut args = json!({"path":"main.js","start_line":line,"max_lines":1});
        if rounds + 1 == limit {
            args["note"] = json!("notes ".repeat(self.pad_tokens));
        }
        Ok(call(&format!("read-{}", seen.len()), "file_read", args))
    }
}

#[tokio::test]
async fn a_checkpoint_on_the_last_closing_request_keeps_that_request() {
    use mnemoarc::context::ContextManager;
    let (_dir, mut s) = saved_fixture();
    s.config.run_tokens = 10_000_000;
    s.config.context_tokens = 128_000;
    pad_history(&mut s, 4);
    let budget = ContextManager::input_budget(&s.config);
    let client = Arc::new(ClosingCheckpoint {
        pad_tokens: budget,
        closing_rounds: Mutex::new(vec![]),
        checkpoints: Mutex::new(0),
    });
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, client.clone(), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    let rounds = client.closing_rounds.lock().unwrap().clone();
    assert!(*client.checkpoints.lock().unwrap() > 0, "{rounds:?}");
    // The checkpoint started on the last closing request. That request is
    // sent after the checkpoint instead of being lost to it.
    assert_eq!(
        rounds.last().copied(),
        Some(mnemoarc::agent::CLOSING_ROUND_LIMIT as u64),
        "{rounds:?} {:?} {:?}",
        result.completion_gaps,
        result.last_error
    );
    assert_eq!(result.status, "complete", "{:?}", result.completion_gaps);
}

/// Scripted replies with a delay each, recording every request's
/// run_guidance.
struct Paced {
    steps: Mutex<Vec<(std::time::Duration, Completion)>>,
    guidance: Mutex<Vec<Value>>,
}

#[async_trait]
impl LlmClient for Paced {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let state: Value = request["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .and_then(|message| message["content"].as_str())
            .and_then(|content| content.split_once('\n'))
            .and_then(|(_, json)| serde_json::from_str(json).ok())
            .unwrap_or(Value::Null);
        self.guidance
            .lock()
            .unwrap()
            .push(state["run_guidance"].clone());
        let step = {
            let mut steps = self.steps.lock().unwrap();
            if steps.is_empty() {
                anyhow::bail!("script_exhausted");
            }
            steps.remove(0)
        };
        tokio::time::sleep(step.0).await;
        Ok(step.1)
    }
}

async fn run_paced(
    s: Session,
    steps: Vec<(std::time::Duration, Completion)>,
) -> (Session, Vec<Value>) {
    let client = Arc::new(Paced {
        steps: Mutex::new(steps),
        guidance: Mutex::new(vec![]),
    });
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, client.clone(), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    let guidance = client.guidance.lock().unwrap().clone();
    (result, guidance)
}

fn text(reply: &str) -> Completion {
    Completion {
        text: reply.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_deadline_inside_a_request_finishes_with_the_gap_report() {
    // Live run 2026-10-07: the deadline fell while a slow model's request
    // ran, and the run ended blocked with no gap report for its saved doc.
    let (_dir, mut s, _) = fixture();
    s.config.run_timeout_secs = 2;
    tools::execute(
        &mut s,
        "task_plan",
        json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Verify the loop section"]}]}),
    )
    .unwrap();
    let steps = vec![(
        std::time::Duration::from_secs(4),
        call("read", "file_read", json!({"path":"main.js"})),
    )];
    let (result, guidance) = run_paced(s, steps).await;
    assert_eq!(guidance.len(), 1);
    assert_eq!(
        result.status, "complete_with_gaps",
        "{:?}",
        result.last_error
    );
    assert_eq!(result.run_history.back().unwrap().reason, "run_timeout");
    assert!(
        result
            .completion_gaps
            .iter()
            .any(|gap| gap.starts_with("미완료 할 일")),
        "{:?}",
        result.completion_gaps
    );
}

#[tokio::test]
async fn a_slow_model_closes_early_and_skips_a_request_that_cannot_end() {
    // Three requests at this run's pace (7.5 s) exceed the 10% reserve (0.6 s),
    // so closing can start after the first request, but never before the
    // verification share of the run: 25% here keeps it (1.5 s), 90% lets it
    // start. With half a request's time left, either run finishes instead of
    // sending a request the deadline would cut.
    let slow_run = |verification_reserve_ratio: f64| {
        let (dir, mut s, _) = fixture();
        s.config.run_timeout_secs = 6;
        s.config.verification_reserve_ratio = verification_reserve_ratio;
        s.config.writing_reserve_ratio = s.config.writing_reserve_ratio.max(0.95);
        std::fs::write(dir.path().join("a.js"), "export const a = 1;\n").unwrap();
        std::fs::write(dir.path().join("b.js"), "export const b = 2;\n").unwrap();
        let pace = std::time::Duration::from_millis(2500);
        let steps = vec![
            (pace, call("a", "file_read", json!({"path":"a.js"}))),
            (pace, call("b", "file_read", json!({"path":"b.js"}))),
            (pace, call("c", "file_read", json!({"path":"main.js"}))),
        ];
        async move {
            let result = run_paced(s, steps).await;
            drop(dir);
            result
        }
    };
    let ((capped, capped_guidance), (open, open_guidance)) =
        tokio::join!(slow_run(0.25), slow_run(0.9));
    assert!(
        capped_guidance[1]["closing"].is_null(),
        "{}",
        capped_guidance[1]
    );
    assert_eq!(
        open_guidance[1]["closing"]["active"], true,
        "{}",
        open_guidance[1]
    );
    assert_eq!(open_guidance[1]["closing"]["reason"], "budget");
    for (result, guidance) in [(capped, capped_guidance), (open, open_guidance)] {
        assert_eq!(guidance.len(), 2, "{guidance:?}");
        assert_ne!(result.status, "blocked", "{:?}", result.last_error);
        assert_eq!(result.run_history.back().unwrap().reason, "run_timeout");
    }
}

#[tokio::test]
async fn reads_after_a_declared_draft_phase_still_count_as_progress() {
    // Live run 2026-10-07: the model declared phase draft before its first
    // save, and its reads for later sections stopped counting (2 to 10).
    let (dir, s, _) = fixture();
    for name in ["a.js", "b.js", "c.js"] {
        std::fs::write(
            dir.path().join(name),
            format!("export const x = '{name}';\n"),
        )
        .unwrap();
    }
    let steps = vec![
        call(
            "draft",
            "task_state",
            json!({"action":"update","patch":{"phase":"draft"}}),
        ),
        call("a", "file_read", json!({"path":"a.js"})),
        call("b", "file_read", json!({"path":"b.js"})),
        call("c", "file_read", json!({"path":"c.js"})),
    ];
    let (_, guidance) = run_scripted(s, steps).await;
    assert_eq!(guidance[2]["phase"], "draft", "{}", guidance[2]);
    for g in &guidance[2..] {
        assert_eq!(g["progress_recovery"]["rounds_since_progress"], 0, "{g}");
    }
}

/// Switches the run to another model, as a settings change during a run
/// does, while the request with this index runs.
struct SwitchModel {
    inner: Arc<Paced>,
    at: usize,
    requests: Mutex<usize>,
    switch: Mutex<Option<(mpsc::Sender<mnemoarc::agent::RunCommand>, Config)>>,
}

#[async_trait]
impl LlmClient for SwitchModel {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        cancel: CancellationToken,
        tx: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let index = {
            let mut requests = self.requests.lock().unwrap();
            *requests += 1;
            *requests
        };
        if index == self.at
            && let Some((commands, next)) = self.switch.lock().unwrap().take()
        {
            commands
                .try_send(mnemoarc::agent::RunCommand::Configure(Box::new(next)))
                .unwrap();
        }
        self.inner.complete(request, config, cancel, tx).await
    }
}

async fn run_switching(
    s: Session,
    steps: Vec<(std::time::Duration, Completion)>,
    at: usize,
) -> (Session, Vec<Value>) {
    let mut next = s.config.clone();
    next.model = "gpt-4o-mini".into();
    let (commands, receiver) = mpsc::channel(4);
    let paced = Arc::new(Paced {
        steps: Mutex::new(steps),
        guidance: Mutex::new(vec![]),
    });
    let client = Arc::new(SwitchModel {
        inner: paced.clone(),
        at,
        requests: Mutex::new(0),
        switch: Mutex::new(Some((commands, next))),
    });
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result =
        mnemoarc::agent::run_session_controlled(s, client, CancellationToken::new(), tx, receiver)
            .await;
    drain.await.unwrap();
    let guidance = paced.guidance.lock().unwrap().clone();
    (result, guidance)
}

#[tokio::test]
async fn a_model_switched_mid_run_learns_its_own_pace() {
    // The first model's 2.5 s requests would start closing at once (as in
    // a_slow_model_closes_early...); the model that replaced it has shown
    // no pace yet, so the ratio reserve alone decides.
    let (dir, mut s, _) = fixture();
    s.config.run_timeout_secs = 6;
    s.config.verification_reserve_ratio = 0.9;
    s.config.writing_reserve_ratio = s.config.writing_reserve_ratio.max(0.95);
    std::fs::write(dir.path().join("a.js"), "export const a = 1;\n").unwrap();
    let steps = vec![
        (
            std::time::Duration::from_millis(2500),
            call("a", "file_read", json!({"path":"a.js"})),
        ),
        (std::time::Duration::ZERO, text("Saved out.md.")),
    ];
    let (result, guidance) = run_switching(s, steps, 1).await;
    assert_eq!(result.config.model, "gpt-4o-mini");
    assert!(guidance[1]["closing"].is_null(), "{}", guidance[1]);
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
}

/// Gives the final answer until the run refuses an uncited document; then
/// cites the read loop lines when `cite` is set.
struct CiteOnRefusal {
    calls: Mutex<usize>,
    cite: bool,
}

#[async_trait]
impl LlmClient for CiteOnRefusal {
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
        )?;
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        assert!(*calls < 20, "the run did not finish");
        let refused = state["run_guidance"]["finalization_error"]
            .as_str()
            .is_some_and(|error| error.starts_with("document_citations_missing:"));
        if refused && self.cite {
            return Ok(Completion {
                calls: vec![ToolCall {
                    id: format!("cite-{calls}"),
                    name: "document_edit".into(),
                    arguments: json!({"action":"replace_text","old_text":"A loop runs work.",
                        "text":"A loop runs work. main.js:3-5"})
                    .to_string(),
                }],
                ..Default::default()
            });
        }
        Ok(Completion {
            text: "Saved out.md.".into(),
            ..Default::default()
        })
    }
}

/// The fixture with its document rewritten to cite nothing.
fn uncited_fixture() -> (tempfile::TempDir, Session) {
    let (dir, mut s, _) = fixture();
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"write","expected_hash":hash,"text":"# Flow\nA loop runs work.\n"}),
    )
    .unwrap();
    (dir, s)
}

async fn run_citing(s: Session, client: Arc<CiteOnRefusal>) -> Session {
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, client, CancellationToken::new(), tx).await;
    drain.await.unwrap();
    result
}

#[tokio::test]
async fn a_source_document_finishes_only_once_it_cites_a_source() {
    // Live run 2026-10-07: a document that cited nothing finished "complete"
    // with no source check and no reported gap.
    let (_dir, s) = uncited_fixture();
    let client = Arc::new(CiteOnRefusal {
        calls: Mutex::new(0),
        cite: true,
    });
    let result = run_citing(s, client.clone()).await;
    assert_eq!(
        result.status, "complete",
        "{:?} {:?}",
        result.last_error, result.completion_gaps
    );
    assert!(result.completion_gaps.is_empty());
    assert!(
        std::fs::read_to_string(&result.project.output)
            .unwrap()
            .contains("main.js:3-5")
    );
    // Refused final, the citing edit, the accepted final.
    assert_eq!(*client.calls.lock().unwrap(), 3);
}

#[tokio::test]
async fn closing_accepts_an_uncited_document_and_reports_it() {
    // A model that never cites: refused finals stall the run into closing,
    // whose second final is accepted with the omission reported.
    let (_dir, mut s) = uncited_fixture();
    s.config.stall_round_limit = 2;
    let client = Arc::new(CiteOnRefusal {
        calls: Mutex::new(0),
        cite: false,
    });
    let result = run_citing(s, client.clone()).await;
    assert_eq!(
        result.status, "complete_with_gaps",
        "{:?}",
        result.last_error
    );
    assert!(result.progress_recovery.closing.is_some());
    assert!(
        result
            .completion_gaps
            .iter()
            .any(|gap| gap.starts_with("소스 인용")),
        "{:?}",
        result.completion_gaps
    );
}

#[test]
fn an_uncited_save_says_the_final_answer_needs_a_citation() {
    let (_dir, mut s, _) = fixture();
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    let saved = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"write","expected_hash":hash,"text":"# Flow\nA loop runs work.\n"}),
    )
    .unwrap();
    let note = saved["citation_check"]["citations_required"]
        .as_str()
        .unwrap();
    assert!(note.contains("final answer is refused"), "{note}");
    let audit = tools::execute(&mut s, "document_audit", json!({})).unwrap();
    assert_eq!(audit["structural_ok"], true, "{audit}");
    assert!(
        audit["issues"][0]["guidance"]
            .as_str()
            .unwrap()
            .contains("final answer is refused"),
        "{audit}"
    );
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    let cited = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"write","expected_hash":hash,"text":"# Flow\nA loop runs work. main.js:3-5\n"}),
    )
    .unwrap();
    assert!(
        cited["citation_check"].get("citations_required").is_none(),
        "{cited}"
    );
}
