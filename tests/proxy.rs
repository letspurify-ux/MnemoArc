use axum::{Router, http::header, routing::post};
use mnemoarc::{
    config::{Config, Secret},
    llm::{LlmClient, OpenAiClient},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

// Set proxy variables only in a child process, never mutate the environment of
// the parallel test runner (reqwest may also cache proxy discovery).
#[test]
fn forced_direct_connection_ignores_configured_and_environment_proxies() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "proxy_child", "--ignored", "--nocapture"])
        .env("MNEMOARC_PROXY_TEST_CHILD", "1")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("http_proxy", "http://127.0.0.1:9")
        .env("https_proxy", "http://127.0.0.1:9")
        .env("all_proxy", "http://127.0.0.1:9")
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .env_remove("REQUEST_METHOD")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "invoked by isolated proxy test parent"]
async fn proxy_child() {
    assert_eq!(
        std::env::var("MNEMOARC_PROXY_TEST_CHILD").as_deref(),
        Ok("1")
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route("/chat/completions",post(||async {
        ([(header::CONTENT_TYPE,"text/event-stream")],
         "data: {\"choices\":[{\"delta\":{\"content\":\"direct\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut config = Config {
        base_url,
        api_key: Some(Secret("local-test".into())),
        retries: 0,
        request_timeout_secs: 3,
        ..Default::default()
    };
    assert!(!config.disable_proxy);
    // Control: environment proxy is actually active when the option is off.
    assert!(request(&config).await.is_err());
    config.disable_proxy = true;
    assert_eq!(request(&config).await.unwrap(), "direct");
    config.proxy = Some("http://127.0.0.1:9".into());
    assert_eq!(request(&config).await.unwrap(), "direct");
    config.proxy = Some("http://[invalid".into());
    assert_eq!(request(&config).await.unwrap(), "direct");
    config.disable_proxy = false;
    assert!(request(&config).await.is_err());
    server.abort();
    let _ = server.await;
}

async fn request(config: &Config) -> anyhow::Result<String> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = OpenAiClient
        .complete(
            json!({"model":"local","messages":[]}),
            config,
            CancellationToken::new(),
            tx,
        )
        .await;
    drain.await.unwrap();
    result.map(|result| result.text)
}

#[test]
fn proxy_disable_setting_defaults_and_round_trips() {
    let old: Config = serde_json::from_value(json!({"proxy":"http://localhost:8080"})).unwrap();
    assert!(!old.disable_proxy);
    let enabled: Config =
        serde_json::from_value(json!({"proxy":"http://localhost:8080","disable_proxy":true}))
            .unwrap();
    let restored: Config = serde_json::from_value(serde_json::to_value(enabled).unwrap()).unwrap();
    assert!(restored.disable_proxy);
    assert_eq!(restored.proxy.as_deref(), Some("http://localhost:8080"));
}
