mod support;
use async_trait::async_trait;
use mnemoarc::{
    agent,
    config::{Config, Project},
    context::ContextManager,
    llm::{Completion, LlmClient, MAX_TOOL_CALL_ID_BYTES, ToolCall, Usage},
    session::Session,
    tools,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

fn session(root: &std::path::Path) -> Session {
    let mut s = Session::new(
        Project {
            root: root.into(),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            ..support::compact_config()
        },
    );
    s.active_tools = tools::ToolRegistry::optional_names();
    s.add_user("Keep this request after errors".into());
    s
}

#[test]
fn extreme_pagination_and_budget_inputs_do_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.select_workflow("source_document").unwrap();
    for (name, args) in [
        ("task_state", json!({"action":"details","offset":u64::MAX})),
        ("task_plan", json!({"action":"list","offset":u64::MAX})),
    ] {
        assert!(tools::execute(&mut s, name, args).is_ok());
    }
    assert!(
        s.memory
            .page(
                "",
                &[],
                Some(&format!("{}:{}", s.memory.generation, usize::MAX)),
                100
            )
            .is_err()
    );
    let c = Config {
        context_tokens: usize::MAX,
        output_tokens: usize::MAX / 3,
        ..support::compact_config()
    };
    // Saturating arithmetic: an extreme output limit never panics.
    assert!(ContextManager::input_budget(&c) < c.context_tokens);
    let c = Config {
        context_tokens: 10_000,
        output_tokens: usize::MAX / 3,
        ..support::compact_config()
    };
    assert_eq!(ContextManager::input_budget(&c), 0);
    assert!(c.validate().is_err());
    for field in [
        "request_timeout_secs",
        "tool_timeout_secs",
        "run_timeout_secs",
    ] {
        let mut value = serde_json::to_value(support::compact_config()).unwrap();
        value[field] = json!(u64::MAX);
        assert!(
            serde_json::from_value::<Config>(value)
                .unwrap()
                .validate()
                .is_err()
        );
    }
}

#[test]
fn deeply_nested_rust_imports_do_not_overflow_the_tool_worker_stack() {
    // A stack overflow aborts the process instead of unwinding. Isolate the
    // regression so a failed navigation worker cannot kill the test runner.
    const CHILD: &str = "MNEMOARC_NESTED_IMPORT_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "deeply_nested_rust_imports_do_not_overflow_the_tool_worker_stack",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "navigation worker aborted:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    std::thread::Builder::new()
        .name("nested-import-worker".into())
        .stack_size(512 * 1024)
        .spawn(|| {
            let dir = tempfile::tempdir().unwrap();
            let depth = 2048;
            for prefix in ["module::{", "{"] {
                let source = format!(
                    "use {}target{};\nfn main() {{ target(); }}\n",
                    prefix.repeat(depth),
                    "}".repeat(depth)
                );
                std::fs::write(dir.path().join("main.rs"), source).unwrap();
                let mut s = session(dir.path());
                let outline =
                    tools::execute(&mut s, "code_outline", json!({"path":"main.rs"})).unwrap();
                assert_eq!(outline["has_parse_errors"], false);
                let result = tools::execute(
                    &mut s,
                    "symbol_relations",
                    json!({"path":"main.rs","symbol_id":outline["symbols"][0]["symbol_id"]}),
                )
                .unwrap();
                assert_eq!(result["relations"].as_array().unwrap().len(), 1);
                assert_eq!(result["relations"][0]["name"], "target");
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn direct_syntax_tools_reject_extreme_timeouts_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    let mut s = session(dir.path());
    let outline = tools::execute(&mut s, "code_outline", json!({"path":"main.rs"})).unwrap();
    s.config.tool_timeout_secs = u64::MAX;
    for (name, args) in [
        ("code_outline", json!({"path":"main.rs"})),
        (
            "symbol_read",
            json!({"path":"main.rs","symbol_id":outline["symbols"][0]["symbol_id"]}),
        ),
        ("symbol_search", json!({"path":"main.rs","query":"main"})),
        (
            "symbol_relations",
            json!({"path":"main.rs","symbol_id":outline["symbols"][0]["symbol_id"]}),
        ),
    ] {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tools::execute(&mut s, name, args)
        }));
        assert!(
            result.is_ok(),
            "{name} panicked while creating its deadline"
        );
        assert!(result.unwrap().unwrap_err().to_string().contains("timeout"));
    }
}

#[test]
fn direct_database_calls_reject_extreme_timeouts_without_panicking() {
    use mnemoarc::database::{self, DatabaseConfig, SavedQuery};

    let config = DatabaseConfig {
        enabled: true,
        raw_query_enabled: true,
        raw_statement_enabled: true,
        procedure_enabled: true,
        function_enabled: true,
        queries: vec![SavedQuery {
            id: "example".into(),
            enabled: true,
            sql: "SELECT 1 FROM dual".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let cancel = CancellationToken::new();
    let result = std::panic::catch_unwind(|| {
        database::execute(
            &config,
            &json!({"action":"run","id":"example"}),
            &cancel,
            u64::MAX,
        )
    });
    assert!(result.is_ok(), "saved query deadline caused a panic");
    assert!(result.unwrap().unwrap_err().to_string().contains("timeout"));
    for mode in ["query", "statement", "procedure", "function"] {
        let result = std::panic::catch_unwind(|| {
            database::execute_free(&config, &json!({"mode":mode}), &cancel, u64::MAX)
        });
        assert!(result.is_ok(), "{mode} deadline caused a panic");
        assert!(result.unwrap().unwrap_err().to_string().contains("timeout"));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn reading_a_utf8_alias_of_a_non_utf8_file_returns_an_error_without_panicking() {
    use std::{
        ffi::OsString,
        os::unix::{ffi::OsStringExt, fs::symlink},
    };

    let dir = tempfile::tempdir().unwrap();
    let source = dir
        .path()
        .join(OsString::from_vec(b"source-\xff.rs".to_vec()));
    std::fs::write(&source, "fn example() {}\n").unwrap();
    symlink(&source, dir.path().join("alias.rs")).unwrap();
    for (name, args) in [
        ("file_read", json!({"path":"alias.rs"})),
        ("code_outline", json!({"path":"alias.rs"})),
        (
            "symbol_search",
            json!({"path":"alias.rs","query":"example"}),
        ),
    ] {
        let mut current = session(dir.path());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tools::execute(&mut current, name, args)
        }));
        assert!(result.is_ok(), "{name} panicked while serializing a path");
        let error = result.unwrap().unwrap_err().to_string();
        assert!(
            error.starts_with("unsupported_non_utf8_path:"),
            "{name}: {error}"
        );
    }
}

struct Panicking;
#[async_trait]
impl LlmClient for Panicking {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        panic!("injected model failure")
    }
}
/// Answers once with a length-limited reply, then waits for cancellation.
/// The second request proves length recovery inspected the FIFO output
/// without blocking; holding it keeps the run alive until the test cancels,
/// however fast the recovery rounds run.
struct LengthLimited {
    calls: std::sync::atomic::AtomicUsize,
    recovered: Notify,
}
#[async_trait]
impl LlmClient for LengthLimited {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        cancel: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return Ok(Completion {
                text: "partial answer".into(),
                length_limited: true,
                ..Default::default()
            });
        }
        self.recovered.notify_one();
        cancel.cancelled().await;
        anyhow::bail!("cancelled")
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn length_recovery_rejects_fifo_output_and_can_cancel() {
    use std::os::unix::fs::OpenOptionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.md");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let mut s = session(dir.path());
    s.project.output = "out.md".into();
    s.config.run_timeout_secs = 1;
    let cancel = CancellationToken::new();
    let client = Arc::new(LengthLimited {
        calls: Default::default(),
        recovered: Notify::new(),
    });
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let job = tokio::spawn(agent::run_session(s, client.clone(), cancel.clone(), tx));
    let recovered = tokio::time::timeout(Duration::from_secs(5), client.recovered.notified())
        .await
        .is_ok();
    if !recovered {
        // Release the FIFO reader so the test fails without leaving a blocked
        // runtime thread behind.
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path);
    }
    cancel.cancel();
    let returned = tokio::time::timeout(Duration::from_secs(3), job)
        .await
        .unwrap()
        .unwrap();
    drain.await.unwrap();
    assert!(recovered, "length recovery blocked on a FIFO output");
    assert_eq!(returned.status, "cancelled");
}
struct RetainedSender(Mutex<Option<mpsc::Sender<String>>>);
#[async_trait]
impl LlmClient for RetainedSender {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        delta: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        delta.send("delivered delta".into()).await.unwrap();
        *self.0.lock().unwrap() = Some(delta);
        Ok(Completion {
            text: "done".into(),
            ..Default::default()
        })
    }
}
struct HugeUsage;
#[async_trait]
impl LlmClient for HugeUsage {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        Ok(Completion {
            text: "done".into(),
            usage: Some(Usage {
                input: usize::MAX,
                output: usize::MAX,
                cached: Some(usize::MAX),
            }),
            attempts: usize::MAX,
            ..Default::default()
        })
    }
}

struct OversizedToolIdentity;
#[async_trait]
impl LlmClient for OversizedToolIdentity {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        Ok(Completion {
            calls: vec![ToolCall {
                id: "x".repeat(MAX_TOOL_CALL_ID_BYTES + 1),
                name: "tool_select".into(),
                arguments: json!({"action":"add","names":["document_edit"]}).to_string(),
            }],
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn alternate_client_cannot_execute_oversized_tool_identity() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(32);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        agent::run_session(
            session(dir.path()),
            Arc::new(OversizedToolIdentity),
            CancellationToken::new(),
            tx,
        ),
    )
    .await
    .unwrap();
    drain.await.unwrap();
    assert_eq!(result.status, "blocked");
    assert!(
        result
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("malformed_tool_call"))
    );
    assert!(result.pending_tools.is_none());
    assert!(result.ledger.is_empty());
}

#[tokio::test]
async fn model_panics_retained_stream_senders_and_extreme_usage_return_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let retained = Arc::new(RetainedSender(Mutex::new(None)));
    let clients: Vec<Arc<dyn LlmClient>> = vec![Arc::new(Panicking), retained, Arc::new(HugeUsage)];
    for (i, client) in clients.into_iter().enumerate() {
        let (tx, mut rx) = mpsc::channel(8);
        let drain = tokio::spawn(async move {
            let mut deltas = String::new();
            while let Some(event) = rx.recv().await {
                if let agent::AgentEvent::Delta { text, .. } = event {
                    deltas.push_str(&text);
                }
            }
            deltas
        });
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            agent::run_session(session(dir.path()), client, CancellationToken::new(), tx),
        )
        .await
        .expect("run must not hang");
        assert_eq!(result.latest_request, "Keep this request after errors");
        if i == 0 {
            assert_eq!(result.status, "blocked");
            assert_eq!(
                result.run_history.back().unwrap().reason,
                "model_worker_panic"
            );
            assert!(result.last_error.unwrap().contains("model_worker_panic"));
        } else {
            assert_eq!(result.status, "complete");
        }
        let deltas = tokio::time::timeout(Duration::from_secs(2), drain)
            .await
            .unwrap()
            .unwrap();
        if i == 1 {
            assert_eq!(deltas, "delivered delta");
        }
    }
}

#[tokio::test]
async fn cancellation_unblocks_a_full_event_channel() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, _rx) = mpsc::channel(1); // Receiver remains alive but does not drain.
    let cancel = CancellationToken::new();
    let task = tokio::spawn(agent::run_session(
        session(dir.path()),
        Arc::new(Panicking),
        cancel.clone(),
        tx,
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, "cancelled");
}

struct FloodThenWait;
#[async_trait]
impl LlmClient for FloodThenWait {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        delta: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        for _ in 0..200 {
            let _ = delta.send("chunk".into()).await;
        }
        std::future::pending().await
    }
}

#[tokio::test]
async fn run_deadline_unblocks_initial_snapshot_without_user_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.run_timeout_secs = 1;
    let (tx, _rx) = mpsc::channel(1);
    tx.send(agent::AgentEvent::Notice {
        session: s.id.clone(),
        text: "full".into(),
    })
    .await
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        agent::run_session(s, Arc::new(Panicking), CancellationToken::new(), tx),
    )
    .await
    .expect("run deadline must also bound snapshot delivery");
    assert_eq!(result.status, "blocked");
    assert!(result.last_error.unwrap().contains("run_timeout"));
}

#[tokio::test]
async fn run_deadline_unblocks_stream_relay_without_user_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.run_timeout_secs = 1;
    let (tx, _rx) = mpsc::channel(2); // Both snapshots fit; deltas then block.
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        agent::run_session(s, Arc::new(FloodThenWait), CancellationToken::new(), tx),
    )
    .await
    .expect("model timeout must also release relay and final snapshot");
    assert_eq!(result.status, "blocked");
    assert!(result.last_error.unwrap().contains("run_timeout"));
}

struct WriteAfterResponse;
#[async_trait]
impl LlmClient for WriteAfterResponse {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        Ok(Completion {
            calls: vec![mnemoarc::llm::ToolCall {
                id: "late-write".into(),
                name: "document_edit".into(),
                arguments: json!({"action":"create","text":"must not be written after deadline"})
                    .to_string(),
            }],
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn expired_event_delivery_does_not_start_a_new_file_write() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.run_timeout_secs = 1;
    let (tx, _rx) = mpsc::channel(2);
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        agent::run_session(
            s,
            Arc::new(WriteAfterResponse),
            CancellationToken::new(),
            tx,
        ),
    )
    .await
    .unwrap();
    assert!(!dir.path().join("docs/source-summary.md").exists());
    assert_eq!(result.status, "blocked");
}
