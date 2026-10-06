use crate::support;
use axum::{Json, Router, http::StatusCode, routing::post};
use mnemoarc::{
    config::{Config, Secret},
    evaluation,
};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[tokio::test]
async fn both_evaluation_variants_offer_documentation_tools_on_the_first_request() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("main.rs"), "fn main() {}\n").unwrap();
    let suite = dir.path().join("suite.toml");
    std::fs::write(
        &suite,
        r#"repetitions = 3
[[cases]]
name = "fixture"
root = "source"
prompt = "Document main.rs with source evidence."
"#,
    )
    .unwrap();

    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    let app = Router::new().route(
        "/chat/completions",
        post(move |Json(request): Json<Value>| {
            let captured = captured.clone();
            async move {
                captured.lock().unwrap().push(request);
                // Stop each run after its first request without generating a
                // document or relying on the model's choice of tools.
                (StatusCode::UNAUTHORIZED, "evaluation request captured")
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config {
        base_url: format!("http://{}", listener.local_addr().unwrap()),
        model: "evaluation-test".into(),
        model_context: Some(230_000),
        context_tokens: 160_000,
        output_tokens: 16_000,
        low_water: 0.4,
        api_key: Some(Secret("local-test".into())),
        disable_proxy: true,
        retries: 0,
        request_timeout_secs: 3,
        ..support::compact_config()
    };
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        evaluation::run(config, &suite, &dir.path().join("results")),
    )
    .await;
    server.abort();
    let _ = server.await;
    result.unwrap().unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 6, "three runs per memory-reuse variant");
    for (index, request) in requests.iter().enumerate() {
        let state: Value = serde_json::from_str(
            request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap()
                .1,
        )
        .unwrap();
        assert_eq!(state["task"]["workflow"], "source_document");
        assert_eq!(state["task"]["require_investigation"], true);
        assert_eq!(state["memory_reuse_enabled"], index < 3);
        let tools = request["tools"].as_array().unwrap();
        for name in ["investigation", "document_audit", "document_edit"] {
            assert!(
                tools.iter().any(|tool| tool["function"]["name"] == name),
                "missing {name} in evaluation request {index}"
            );
        }
        for name in ["memory_find", "memory_read"] {
            assert_eq!(
                tools.iter().any(|tool| tool["function"]["name"] == name),
                index < 3,
                "memory reuse boundary changed for {name}"
            );
        }
    }
}
