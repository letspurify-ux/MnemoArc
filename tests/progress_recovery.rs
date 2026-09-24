mod support;

use anyhow::{Result, bail};
use async_trait::async_trait;
use mnemoarc::{
    agent::run_session,
    config::{Config, Project},
    llm::{Completion, LlmClient, ToolCall},
    session::Session,
    tools,
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn session(root: &std::path::Path) -> Session {
    let mut s = Session::new(
        Project {
            root: root.into(),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            context_tokens: 128_000,
            run_tokens: 500_000,
            stall_round_limit: 3,
            source_answer_review: false,
            ..Default::default()
        },
    );
    s.add_user("Write result.txt with the requested result".into());
    s
}

fn pending_todo(s: &mut Session) {
    let result = tools::execute(
        s,
        "task_plan",
        json!({"action":"apply","expected_revision":s.task.plan_revision,"operations":[{"op":"insert","texts":["Write result.txt"]}]}),
    )
    .unwrap();
    assert_eq!(result["applied"], true);
}

async fn run(s: Session, client: Arc<dyn LlmClient>) -> Session {
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, client, CancellationToken::new(), tx).await;
    drain.await.unwrap();
    result
}

fn payload(request: &Value) -> Value {
    serde_json::from_str(request["messages"][1]["content"].as_str().unwrap_or(""))
        .unwrap_or(Value::Null)
}

#[derive(Clone, Copy)]
enum Pattern {
    Final,
    InvalidPlan,
    AlternatingWrite,
    RepeatedOutline,
}

struct Stubborn {
    pattern: Pattern,
    calls: Mutex<usize>,
    reviews: Mutex<usize>,
    file: PathBuf,
}

#[async_trait]
impl LlmClient for Stubborn {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let review = payload(&request);
        if review["completion_review"] == true {
            *self.reviews.lock().unwrap() += 1;
            return Ok(Completion {
                text: json!({"checks":review["criteria"].as_array().unwrap().iter().map(|criterion|json!({
                    "id":criterion["id"],"status":"unmet","reason":"Requested result is absent",
                    "evidence":[],"next_action":"Write the requested result to result.txt"
                })).collect::<Vec<_>>()}).to_string(),
                ..Default::default()
            });
        }
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls >= 30 {
            bail!("test_limit: repeated requests escaped recovery");
        }
        let response = match self.pattern {
            Pattern::Final => Completion {
                text: "Done".into(),
                ..Default::default()
            },
            Pattern::InvalidPlan => Completion {
                calls: vec![ToolCall {
                    id: format!("invalid-{calls}"),
                    name: "task_plan".into(),
                    arguments: json!({"action":"apply","expected_revision":0,"operations":false})
                        .to_string(),
                }],
                ..Default::default()
            },
            Pattern::RepeatedOutline => Completion {
                calls: vec![ToolCall {
                    id: format!("outline-{calls}"),
                    name: "code_outline".into(),
                    arguments: json!({"path":"source.rs","view":"compact"}).to_string(),
                }],
                ..Default::default()
            },
            Pattern::AlternatingWrite if *calls == 1 => Completion {
                text: "Done".into(),
                ..Default::default()
            },
            Pattern::AlternatingWrite => {
                let previous = std::fs::read(&self.file).ok();
                let mut args = json!({"path":"result.txt","content":if (*calls).is_multiple_of(2) {"A"} else {"B"}});
                if let Some(previous) = previous {
                    args["expected_hash"] = json!(tools::hash(&previous));
                }
                Completion {
                    calls: vec![ToolCall {
                        id: format!("write-{calls}"),
                        name: "file_write".into(),
                        arguments: args.to_string(),
                    }],
                    ..Default::default()
                }
            }
        };
        Ok(response)
    }
}

#[tokio::test]
async fn pending_final_and_invalid_plan_calls_are_bounded_and_resume_retains_the_count() {
    for pattern in [Pattern::Final, Pattern::InvalidPlan] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        if matches!(pattern, Pattern::Final) {
            pending_todo(&mut s);
        }
        let client = Arc::new(Stubborn {
            pattern,
            calls: Mutex::new(0),
            reviews: Mutex::new(0),
            file: dir.path().join("result.txt"),
        });
        let result = run(s, client.clone()).await;
        assert_eq!(result.status, "partial", "{:?}", result.last_error);
        assert!(
            result
                .last_error
                .as_deref()
                .unwrap_or("")
                .starts_with("progress_recovery_exhausted")
        );
        assert!(*client.calls.lock().unwrap() < 30);
        assert!(result.progress_recovery.repeated_outcome_rounds >= 12);
        let before = *client.calls.lock().unwrap();
        let resumed = run(result, client.clone()).await;
        assert_eq!(resumed.status, "partial");
        assert!(*client.calls.lock().unwrap() <= before + 1);
    }
}

#[tokio::test]
async fn revisiting_earlier_file_versions_does_not_reset_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.completion_review.required = true;
    let client = Arc::new(Stubborn {
        pattern: Pattern::AlternatingWrite,
        calls: Mutex::new(0),
        reviews: Mutex::new(0),
        file: dir.path().join("result.txt"),
    });
    let result = run(s, client.clone()).await;
    assert_eq!(result.status, "partial", "{:?}", result.last_error);
    assert!(
        result
            .last_error
            .as_deref()
            .unwrap_or("")
            .starts_with("progress_recovery_exhausted")
    );
    assert_eq!(*client.reviews.lock().unwrap(), 1);
    assert!(result.progress_recovery.seen_artifact_versions.len() <= 2);
    assert!(*client.calls.lock().unwrap() < 30);
}

#[tokio::test]
async fn unchanged_navigation_result_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("source.rs"), "fn one() {}\n").unwrap();
    let s = session(dir.path());
    let client = Arc::new(Stubborn {
        pattern: Pattern::RepeatedOutline,
        calls: Mutex::new(0),
        reviews: Mutex::new(0),
        file: dir.path().join("result.txt"),
    });
    let result = run(s, client.clone()).await;
    assert_eq!(result.status, "partial", "{:?}", result.last_error);
    assert!(
        result
            .last_error
            .as_deref()
            .unwrap_or("")
            .starts_with("progress_recovery_exhausted")
    );
    assert_eq!(result.progress_recovery.seen_navigation_results.len(), 1);
    assert!(*client.calls.lock().unwrap() < 30);
}

struct FocusedRepair {
    calls: Mutex<usize>,
    file: PathBuf,
}

#[async_trait]
impl LlmClient for FocusedRepair {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        if let Some(review) = support::acceptance(&request) {
            return Ok(review);
        }
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
        if *calls > 20 {
            bail!("test_limit: focused repair did not complete");
        }
        let current = &state["run_guidance"]["current_todo"];
        if self.file.exists() {
            if !current.is_null() {
                return Ok(Completion { calls:vec![ToolCall {
                    id:format!("complete-{calls}"), name:"task_plan".into(),
                    arguments:json!({"action":"apply","expected_revision":state["task"]["plan_revision"],
                        "operations":[{"op":"complete","id":current["id"],"result":"Saved and checked result.txt"}]}).to_string(),
                }], ..Default::default() });
            }
            return Ok(Completion {
                text: "Saved result.txt".into(),
                ..Default::default()
            });
        }
        if state["run_guidance"]["progress_recovery"]["focused"] != true {
            return Ok(Completion {
                text: "Done".into(),
                ..Default::default()
            });
        }
        Ok(Completion {
            calls: vec![ToolCall {
                id: "write-actual-result".into(),
                name: "file_write".into(),
                arguments: json!({"path":"result.txt","content":"Requested result\n"}).to_string(),
            }],
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn focused_recovery_continues_to_accepted_completion() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    pending_todo(&mut s);
    let client = Arc::new(FocusedRepair {
        calls: Mutex::new(0),
        file: dir.path().join("result.txt"),
    });
    let result = run(s, client.clone()).await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert!(result.completion_review.approved);
    assert!(result.task.current_todo().is_none());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("result.txt")).unwrap(),
        "Requested result\n"
    );
    assert_eq!(result.progress_recovery.artifact_edits_without_milestone, 0);
    assert!(*client.calls.lock().unwrap() < 20);
}

struct UniqueWrites {
    calls: Mutex<usize>,
    file: PathBuf,
}

#[async_trait]
impl LlmClient for UniqueWrites {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls >= 50 {
            bail!("test_limit: unique rewrites escaped recovery");
        }
        let mut args = json!({"path":"result.txt","content":format!("Draft {calls}\n")});
        if let Ok(previous) = std::fs::read(&self.file) {
            args["expected_hash"] = json!(tools::hash(&previous));
        }
        Ok(Completion {
            calls: vec![ToolCall {
                id: format!("unique-write-{calls}"),
                name: "file_write".into(),
                arguments: args.to_string(),
            }],
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn distinct_rewrites_without_a_completed_milestone_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let s = session(dir.path());
    let client = Arc::new(UniqueWrites {
        calls: Mutex::new(0),
        file: dir.path().join("result.txt"),
    });
    let result = run(s, client.clone()).await;
    assert_eq!(result.status, "partial", "{:?}", result.last_error);
    assert!(
        result
            .last_error
            .as_deref()
            .unwrap_or("")
            .starts_with("artifact_progress_exhausted")
    );
    assert!(result.progress_recovery.artifact_edits_without_milestone >= 32);
    assert!(*client.calls.lock().unwrap() < 50);
}

struct UniqueNavigation(Mutex<usize>);

#[async_trait]
impl LlmClient for UniqueNavigation {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut calls = self.0.lock().unwrap();
        *calls += 1;
        if *calls <= 15 {
            return Ok(Completion {
                calls: vec![ToolCall {
                    id: format!("outline-{calls}"),
                    name: "code_outline".into(),
                    arguments: json!({"path":format!("file-{calls}.rs"),"view":"compact"})
                        .to_string(),
                }],
                ..Default::default()
            });
        }
        Ok(Completion {
            text: "Reviewed the requested files.".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn distinct_navigation_results_do_not_trigger_a_false_loop_limit() {
    let dir = tempfile::tempdir().unwrap();
    for n in 1..=15 {
        std::fs::write(
            dir.path().join(format!("file-{n}.rs")),
            format!("fn item_{n}() {{}}\n"),
        )
        .unwrap();
    }
    let s = session(dir.path());
    let result = run(s, Arc::new(UniqueNavigation(Mutex::new(0)))).await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(result.progress_recovery.repeated_outcome_rounds, 0);
}

#[tokio::test]
async fn novel_navigation_without_source_evidence_eventually_stops() {
    let dir = tempfile::tempdir().unwrap();
    for n in 1..=50 {
        std::fs::write(
            dir.path().join(format!("file-{n}.rs")),
            format!("fn item_{n}() {{}}\n"),
        )
        .unwrap();
    }
    let s = session(dir.path());
    // Keep finding different outlines without reading source evidence.
    struct EndlessNavigation(Mutex<usize>);
    #[async_trait]
    impl LlmClient for EndlessNavigation {
        async fn complete(
            &self,
            _: Value,
            _: &Config,
            _: CancellationToken,
            _: mpsc::Sender<String>,
        ) -> Result<Completion> {
            let mut calls = self.0.lock().unwrap();
            *calls += 1;
            if *calls > 45 {
                bail!("test_limit: navigation loop escaped recovery");
            }
            Ok(Completion {
                calls: vec![ToolCall {
                    id: format!("outline-{calls}"),
                    name: "code_outline".into(),
                    arguments: json!({"path":format!("file-{calls}.rs"),"view":"compact"})
                        .to_string(),
                }],
                ..Default::default()
            })
        }
    }
    let client = Arc::new(EndlessNavigation(Mutex::new(0)));
    let result = run(s, client.clone()).await;
    assert_eq!(result.status, "partial", "{:?}", result.last_error);
    assert!(
        result
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("navigation and bookkeeping")
    );
    assert_eq!(result.progress_recovery.repeated_outcome_rounds, 0);
    assert_eq!(
        result.progress_recovery.rounds_without_substantive_progress,
        24
    );
    assert_eq!(*client.0.lock().unwrap(), 24);
}

struct ManyArtifacts {
    calls: Mutex<usize>,
    document: bool,
    headings: bool,
    path: PathBuf,
}

#[async_trait]
impl LlmClient for ManyArtifacts {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        if let Some(review) = support::acceptance(&request) {
            return Ok(review);
        }
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls > 50 {
            bail!("test_limit: additive writing did not finish");
        }
        if *calls > 40 {
            return Ok(Completion {
                text: "All forty parts are saved.".into(),
                ..Default::default()
            });
        }
        let (name, args) = if self.document {
            let text = if self.headings {
                format!("\n## Part {calls}\nContent for part {calls}.\n")
            } else {
                format!("\nParagraph {calls} explains part {calls}.\n")
            };
            if let Ok(previous) = std::fs::read(&self.path) {
                (
                    "document_edit",
                    json!({"action":"append","expected_hash":tools::hash(&previous),"text":text}),
                )
            } else {
                (
                    "document_edit",
                    json!({"action":"create","text":format!("# Result\n{text}")}),
                )
            }
        } else {
            (
                "file_write",
                json!({"path":format!("part-{calls}.txt"),"content":format!("Part {calls}\n")}),
            )
        };
        Ok(Completion {
            calls: vec![ToolCall {
                id: format!("part-{calls}"),
                name: name.into(),
                arguments: args.to_string(),
            }],
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn many_distinct_files_can_finish_without_an_artifact_churn_false_positive() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.run_tokens = 5_000_000;
    let result = run(
        s,
        Arc::new(ManyArtifacts {
            calls: Mutex::new(0),
            document: false,
            headings: false,
            path: dir.path().join("unused"),
        }),
    )
    .await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(result.progress_recovery.artifact_edits_without_milestone, 0);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 40);
}

#[tokio::test]
async fn many_new_document_sections_can_reach_final_completion() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.run_tokens = 5_000_000;
    s.task.workflow = "document_edit".into();
    s.active_tools.insert("document_edit".into());
    s.config.source_document_review = false;
    let path = dir.path().join("docs/source-summary.md");
    let result = run(
        s,
        Arc::new(ManyArtifacts {
            calls: Mutex::new(0),
            document: true,
            headings: true,
            path: path.clone(),
        }),
    )
    .await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(result.progress_recovery.artifact_edits_without_milestone, 0);
    assert_eq!(result.progress_recovery.best_document_section_count, 41);
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .contains("## Part 40")
    );
}

#[tokio::test]
async fn one_long_document_section_can_expand_to_completion() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.run_tokens = 5_000_000;
    s.config.source_document_review = false;
    s.task.workflow = "document_edit".into();
    s.active_tools.insert("document_edit".into());
    let path = dir.path().join("docs/source-summary.md");
    let result = run(
        s,
        Arc::new(ManyArtifacts {
            calls: Mutex::new(0),
            document: true,
            headings: false,
            path: path.clone(),
        }),
    )
    .await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(result.progress_recovery.best_document_section_count, 1);
    assert_eq!(result.progress_recovery.best_document_content_lines, 41);
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .contains("Paragraph 40")
    );
}

struct BlankDocumentChurn {
    calls: Mutex<usize>,
    path: PathBuf,
}

#[async_trait]
impl LlmClient for BlankDocumentChurn {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls > 45 {
            bail!("test_limit: blank document edits escaped recovery");
        }
        let args = if let Ok(previous) = std::fs::read(&self.path) {
            json!({"action":"append","expected_hash":tools::hash(&previous),"text":"\n"})
        } else {
            json!({"action":"create","text":"# Result\n"})
        };
        Ok(Completion {
            calls: vec![ToolCall {
                id: format!("blank-{calls}"),
                name: "document_edit".into(),
                arguments: args.to_string(),
            }],
            usage: Some(mnemoarc::llm::Usage {
                input: 5_000,
                output: 10,
                cached: None,
            }),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn empty_line_growth_does_not_keep_a_document_loop_alive() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.run_tokens = 220_000;
    s.config.source_document_review = false;
    s.task.workflow = "document_edit".into();
    s.active_tools.insert("document_edit".into());
    let result = run(
        s,
        Arc::new(BlankDocumentChurn {
            calls: Mutex::new(0),
            path: dir.path().join("docs/source-summary.md"),
        }),
    )
    .await;
    // Blank lines are not progress. A heading-only file has no body to
    // finish, so closing mode stops the loop before the run budget.
    assert_eq!(result.status, "blocked", "{:?}", result.last_error);
    assert!(
        result
            .last_error
            .as_deref()
            .unwrap_or("")
            .starts_with("closing_round_limit")
    );
    assert_eq!(result.progress_recovery.best_document_content_lines, 1);
}
