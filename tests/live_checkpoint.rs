//! Opt-in provider check; never runs with normal `cargo test` and writes no session log.
use mnemoarc::{
    agent::{self, AgentEvent},
    config::{Config, Project, Secret},
    context::ContextManager,
    llm::OpenAiClient,
    memory::{MemoryInput, MemoryKind},
    session::Session,
};
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
#[ignore = "uses the configured paid model; set MNEMOARC_LIVE_TEST=1 and run explicitly"]
async fn configured_model_completes_checkpoint_with_inline_progress() {
    assert_eq!(std::env::var("MNEMOARC_LIVE_TEST").as_deref(), Ok("1"));
    let path = PathBuf::from(std::env::var("MNEMOARC_LIVE_CONFIG").unwrap_or("config.toml".into()));
    let mut config = Config::load(&path, &BTreeMap::new()).unwrap();
    if path.with_extension("credentials.json").exists() {
        let keys: BTreeMap<String, String> = serde_json::from_slice(
            &std::fs::read(path.with_extension("credentials.json")).unwrap(),
        )
        .unwrap();
        config.api_key = keys.get(&config.api_key_env).cloned().map(Secret);
    }
    config.run_tokens = 60000;
    config.run_timeout_secs = 600;
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            ..Default::default()
        },
        config,
    );
    s.task.constraints = vec!["원본 파일을 변경하지 않는다".into()];
    s.task.current = "기억을 사용해 중단 없이 이어가는 프로토콜을 테스트한다".into();
    s.add_user("테스트 사실: Alpha는 RAM 기억을 사용한다. Beta는 기억을 파일에 저장한다. 이 사실과 파일 수정 금지를 유지한다.".into());
    s.memory.save(MemoryInput {
        key:Some("comparison".into()), title:"설계 차이".into(), summary:"Alpha RAM, Beta file".into(),
        body:"Alpha는 RAM 기억을 사용한다. Beta는 기억을 파일에 저장한다. 원본 파일은 변경하지 않는다.".into(),
        tags:vec![], kind:MemoryKind::Fact, inferred:false, source_ids:vec![], metadata:json!(null), expected_revision:None,
    }, s.sources.values().cloned().collect(), &s.config).unwrap();
    s.add_user("기억 정리 프로토콜 테스트다. 기존 기억에 필요한 사실이 이미 저장되어 있으므로 중복 저장하지 마. 대기 중인 체크포인트를 완료하고 진행 요약과 다음 작업을 보존해. 완료 후 두 설계 차이를 한 문장으로 답하고 종료해. 파일·도구 선택은 필요 없다.".into());
    ContextManager::prepare(&mut s, 60000).unwrap();
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let AgentEvent::Tool { name, status, .. } = event {
                eprintln!("{name}: {status}");
            }
        }
    });
    let result = agent::run_session(s, Arc::new(OpenAiClient), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    eprintln!(
        "status={}, checkpoints={}, input={}, output={}",
        result.status, result.checkpoints_completed, result.input_tokens, result.output_tokens
    );
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(result.checkpoints_completed, 1);
    assert!(result.task.revision > 0);
    assert_eq!(result.task.constraints, ["원본 파일을 변경하지 않는다"]);
    assert!(
        result
            .memory
            .get("comparison")
            .unwrap()
            .body
            .contains("Alpha")
    );
    assert!(
        result
            .history
            .bundles
            .iter()
            .any(|b| !b.active && b.reviewed)
    );
}

/// Replay an unpruned, read-only failed investigation into the current engine.
/// The source server stays untouched; no transcript or credential is written to disk.
#[tokio::test]
#[ignore = "paid model; requires MNEMOARC_LIVE_TEST=1 and MNEMOARC_LIVE_SESSION URL"]
async fn configured_model_resumes_readonly_investigation() {
    use mnemoarc::memory::Source;
    use mnemoarc::session::Bundle;
    use serde_json::Value;
    assert_eq!(std::env::var("MNEMOARC_LIVE_TEST").as_deref(), Ok("1"));
    let url = std::env::var("MNEMOARC_LIVE_SESSION").expect("explicit local session URL required");
    let parsed = reqwest::Url::parse(&url).unwrap();
    assert_eq!(parsed.host_str(), Some("127.0.0.1"));
    let client = reqwest::Client::new();
    let snapshot: Value = client
        .get(format!("{url}?limit=100"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        snapshot["pruned_through"].is_null(),
        "pruned histories cannot be replayed by this probe"
    );
    assert_eq!(snapshot["status"], "blocked");
    assert!(
        snapshot["memories"].as_array().unwrap().is_empty(),
        "probe supports investigations before successful memory writes only"
    );
    let config_path =
        PathBuf::from(std::env::var("MNEMOARC_LIVE_CONFIG").unwrap_or("config.toml".into()));
    let disk = Config::load(&config_path, &BTreeMap::new()).unwrap();
    let mut config: Config = serde_json::from_value(snapshot["config"].clone()).unwrap();
    assert_eq!(config.base_url, disk.base_url);
    if config_path.with_extension("credentials.json").exists() {
        let keys: BTreeMap<String, String> = serde_json::from_slice(
            &std::fs::read(config_path.with_extension("credentials.json")).unwrap(),
        )
        .unwrap();
        config.api_key = keys.get(&config.api_key_env).cloned().map(Secret);
    }
    config.run_tokens = 250000;
    config.run_timeout_secs = 900;
    let mut bundles: Vec<Bundle> = serde_json::from_value(snapshot["bundles"].clone()).unwrap();
    let mut before = snapshot["previous"].as_u64();
    while let Some(id) = before {
        let page: Value = client
            .get(format!("{url}?limit=100&before={id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(page["revision"], snapshot["revision"]);
        let mut earlier: Vec<Bundle> = serde_json::from_value(page["bundles"].clone()).unwrap();
        earlier.append(&mut bundles);
        bundles = earlier;
        before = page["previous"].as_u64();
    }
    let mut session = Session::new(
        serde_json::from_value(snapshot["project"].clone()).unwrap(),
        config,
    );
    session.task = serde_json::from_value(snapshot["task"].clone()).unwrap();
    session.investigations = serde_json::from_value(snapshot["investigations"].clone()).unwrap();
    session.active_tools = serde_json::from_value(snapshot["active_tools"].clone()).unwrap();
    let latest = bundles
        .iter()
        .flat_map(|b| &b.messages)
        .rfind(|m| m["role"] == "user")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    session.add_user(latest.into()); // Reissue the user observation; no memory referred to its old ID.
    session.history.next_id = bundles.iter().map(|b| b.id).max().unwrap_or(0);
    session.history.bundles = bundles.into();
    session.checkpoint = serde_json::from_value(snapshot["checkpoint"].clone()).unwrap();
    if let Some(cp) = &mut session.checkpoint {
        cp.attempts = 0;
        cp.failed_attempts = 0;
        cp.last_failure = None;
        cp.failed = false;
        cp.acknowledged = false;
    }
    fn source(value: &Value, sources: &mut BTreeMap<String, Source>) {
        if !value.is_object() {
            return;
        }
        let mut value = value.clone();
        // Some bounded tool results omit only the duplicate source excerpt.
        if value.get("excerpt").is_none() {
            value["excerpt"] = json!("");
        }
        if let Ok(candidate) = serde_json::from_value::<Source>(value)
            && sources
                .get(&candidate.id)
                .is_none_or(|old| old.excerpt.len() < candidate.excerpt.len())
        {
            sources.insert(candidate.id.clone(), candidate);
        }
    }
    let mut calls = BTreeMap::new();
    for bundle in &session.history.bundles {
        assert!(bundle.complete);
        for message in &bundle.messages {
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                assert_ne!(
                    call["function"]["name"], "document_edit",
                    "do not replay sessions with possible file writes"
                );
                calls.insert(
                    call["id"].as_str().unwrap().to_string(),
                    format!(
                        "{}:{}",
                        call["function"]["name"].as_str().unwrap(),
                        call["function"]["arguments"].as_str().unwrap()
                    ),
                );
            }
            let result = if message["role"] == "tool" {
                serde_json::from_str::<Value>(message["content"].as_str().unwrap()).unwrap()
            } else {
                message["result"].clone()
            };
            // Only program-issued source fields, never arbitrary JSON inside source text.
            source(&result["data"]["source"], &mut session.sources);
            for row in result["data"]["matches"].as_array().into_iter().flatten() {
                source(&row["source"], &mut session.sources);
            }
            if result["status"] == "ok"
                && let Some(id) = message["tool_call_id"].as_str()
            {
                session
                    .ledger
                    .insert(id.into(), (calls[id].clone(), result));
            }
        }
    }
    for item in &session.investigations {
        for s in &item.sources {
            session.sources.insert(s.id.clone(), s.clone());
        }
    }
    assert!(!session.sources.is_empty());
    let stable: Value = client.get(&url).send().await.unwrap().json().await.unwrap();
    assert_eq!(
        stable["revision"], snapshot["revision"],
        "session changed during capture"
    );
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Tool { name, status, .. } => eprintln!("{name}: {status}"),
                AgentEvent::Snapshot(s) => eprintln!(
                    "status={}, checkpoints={}, memories={}, input={}, output={}",
                    s.status,
                    s.checkpoints_completed,
                    s.memory.entries.len(),
                    s.input_tokens,
                    s.output_tokens
                ),
                _ => {}
            }
        }
    });
    let result = agent::run_session(
        session,
        Arc::new(OpenAiClient),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    eprintln!(
        "RESULT status={}, checkpoints={}, memories={}, verified={}/{}, input={}, output={}, error={:?}",
        result.status,
        result.checkpoints_completed,
        result.memory.entries.len(),
        result
            .investigations
            .iter()
            .filter(|i| i.status == "verified")
            .count(),
        result.investigations.len(),
        result.input_tokens,
        result.output_tokens,
        result.last_error
    );
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert!(result.checkpoints_completed > 0);
    assert!(!result.investigations.is_empty());
    assert!(result.investigations.iter().all(|i| i.status == "verified"));
    assert!(
        mnemoarc::tools::output_path(&result.project)
            .unwrap()
            .exists()
    );
}
