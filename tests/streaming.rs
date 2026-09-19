use axum::{Router, http::header, response::IntoResponse, routing::post};
use mnemoarc::{
    config::Config,
    llm::{LlmClient, OpenAiClient, SseDecoder},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;
#[test]
fn sse_all_byte_boundaries() {
    let raw = "data: {\"text\":\"한글🦀\"}\r\n\r\ndata: [DONE]\n\n".as_bytes();
    for split in 0..=raw.len() {
        let mut parser = SseDecoder::default();
        let mut events = parser.feed(&raw[..split]).unwrap();
        events.extend(parser.feed(&raw[split..]).unwrap());
        assert_eq!(events.len(), 2);
        assert_eq!(events[1], "[DONE]");
    }
    let mut parser = SseDecoder::default();
    let mut events = vec![];
    for b in raw {
        events.extend(parser.feed(&[*b]).unwrap());
    }
    assert_eq!(events.len(), 2);
}
async fn server(body: String) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/chat/completions",
        post(move || {
            let body = body.clone();
            async move { ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response() }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, handle)
}
fn event(v: serde_json::Value) -> String {
    format!("data: {v}\n\n")
}
#[tokio::test]
async fn assemble_interleaved_calls_and_usage() {
    let body=[event(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"memory_read","arguments":"{\"id\":"}},{"index":1,"id":"c2","function":{"name":"memory_read","arguments":"{\"id\":"}}]},"finish_reason":null}]})),event(json!({"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"\"b\"}"}},{"index":0,"function":{"arguments":"\"a\"}"}}]},"finish_reason":"tool_calls"}]})),event(json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":3}}})),"data: [DONE]\n\n".into()].concat();
    let (url, server) = server(body).await;
    let c = Config {
        base_url: url,
        model: "mock".into(),
        model_context: Some(64000),
        ..Default::default()
    };
    let (tx, _) = tokio::sync::mpsc::channel(8);
    let out = OpenAiClient
        .complete(
            json!({"model":"mock","messages":[]}),
            &c,
            CancellationToken::new(),
            tx,
        )
        .await
        .unwrap();
    assert_eq!(out.calls.len(), 2);
    assert_eq!(out.calls[0].arguments, "{\"id\":\"a\"}");
    assert_eq!(out.usage.unwrap().cached, Some(3));
    server.abort();
}
#[tokio::test]
async fn incomplete_stream_never_returns_calls() {
    let body = event(
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"document_edit","arguments":"{\"action\":\"create\"}"}}]},"finish_reason":null}]}),
    );
    let (url, server) = server(body).await;
    let c = Config {
        base_url: url,
        ..Default::default()
    };
    let (tx, _) = tokio::sync::mpsc::channel(8);
    assert!(
        OpenAiClient
            .complete(json!({"messages":[]}), &c, CancellationToken::new(), tx)
            .await
            .is_err()
    );
    server.abort();
}
#[tokio::test]
async fn missing_usage_is_not_zero() {
    let body = format!(
        "{}data: [DONE]\n\n",
        event(json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]}))
    );
    let (url, server) = server(body).await;
    let c = Config {
        base_url: url,
        ..Default::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let result = OpenAiClient
        .complete(json!({"messages":[]}), &c, CancellationToken::new(), tx)
        .await
        .unwrap();
    assert!(result.usage.is_none());
    server.abort();
}

#[tokio::test]
async fn retries_transient_http_error_before_accepting_a_complete_stream() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let app = Router::new().route("/chat/completions", post(move || {
        let count = counter.fetch_add(1, Ordering::SeqCst);
        async move {
            if count == 0 { (axum::http::StatusCode::TOO_MANY_REQUESTS, "retry").into_response() }
            else { ([(header::CONTENT_TYPE, "text/event-stream")], format!("{}data: [DONE]\n\n", event(json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7}})))).into_response() }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let c = Config {
        base_url: format!("http://{}", listener.local_addr().unwrap()),
        ..Default::default()
    };
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let result = OpenAiClient
        .complete(json!({"messages":[]}), &c, CancellationToken::new(), tx)
        .await
        .unwrap();
    assert_eq!(result.text, "OK");
    assert_eq!(result.attempts, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        result.usage.is_none(),
        "partial usage must not invent zero output"
    );
    server.abort();
}
