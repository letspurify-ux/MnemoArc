use axum::{Router, http::header, response::IntoResponse, routing::post};
use mnemoarc::{
    config::Config,
    llm::{
        LlmClient, MAX_TOOL_CALL_ID_BYTES, MAX_TOOL_CALLS, MAX_TOOL_NAME_BYTES, OpenAiClient,
        SseDecoder,
    },
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

#[test]
fn oversized_sse_event_reports_a_recoverable_response_size_code() {
    let mut parser = SseDecoder::default();
    let error = parser.feed(&vec![b'x'; 8 * 1024 * 1024 + 1]).unwrap_err();
    assert!(error.to_string().starts_with("response_size_limit:"));
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
async fn oversized_tool_identity_and_batch_are_rejected_during_streaming() {
    let oversized_id = format!(
        "{}data: [DONE]\n\n",
        event(
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"x".repeat(MAX_TOOL_CALL_ID_BYTES + 1),"function":{"name":"tool_catalog","arguments":"{}"}}]},"finish_reason":"tool_calls"}]})
        )
    );
    let oversized_name = [
        event(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"x".repeat(MAX_TOOL_NAME_BYTES / 2 + 1),"arguments":"{}"}}]},"finish_reason":null}]})),
        event(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"x".repeat(MAX_TOOL_NAME_BYTES / 2 + 1)}}]},"finish_reason":"tool_calls"}]})),
        "data: [DONE]\n\n".into(),
    ].concat();
    let entries: Vec<_> = (0..=MAX_TOOL_CALLS).map(|index| json!({"index":index,"id":format!("call-{index}"),"function":{"name":"tool_catalog","arguments":"{}"}})).collect();
    let too_many_calls = format!(
        "{}data: [DONE]\n\n",
        event(json!({"choices":[{"delta":{"tool_calls":entries},"finish_reason":"tool_calls"}]}))
    );
    for (body, expected) in [
        (oversized_id, "malformed_tool_call"),
        (oversized_name, "malformed_tool_call"),
        (too_many_calls, "tool_call_batch_limit"),
    ] {
        let (url, server) = server(body).await;
        let config = Config {
            base_url: url,
            retries: 0,
            ..Default::default()
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let error = OpenAiClient
            .complete(
                json!({"messages":[]}),
                &config,
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        server.abort();
    }
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
async fn disabling_thinking_overrides_saved_qwen_reasoning_effort() {
    use axum::extract::Json as RequestJson;
    use std::sync::{Arc, Mutex};

    let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let captured = requests.clone();
    let app = Router::new().route(
        "/chat/completions",
        post(move |RequestJson(body): RequestJson<serde_json::Value>| {
            let captured = captured.clone();
            async move {
                captured.lock().unwrap().push(body);
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    format!(
                        "{}data: [DONE]\n\n",
                        event(json!({
                            "choices": [{
                                "delta": {"content": "OK"},
                                "finish_reason": "stop"
                            }]
                        }))
                    ),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let c = Config {
        base_url: url,
        model: "qwen/qwen3.8-27b".into(),
        model_context: Some(1_000_000),
        reasoning_effort: Some("xhigh".into()),
        enable_thinking: false,
        ..Default::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    OpenAiClient
        .complete(
            json!({"model":"qwen/qwen3.8-27b","messages":[]}),
            &c,
            CancellationToken::new(),
            tx,
        )
        .await
        .unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["reasoning_effort"], "none");
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

async fn sequenced_server(
    bodies: Vec<String>,
) -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let bodies = Arc::new(bodies);
    let app = Router::new().route(
        "/chat/completions",
        post(move || {
            let index = counter.fetch_add(1, Ordering::SeqCst);
            let body = bodies[index.min(bodies.len() - 1)].clone();
            async move { ([(header::CONTENT_TYPE, "text/event-stream")], body) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, server, calls)
}

#[tokio::test]
async fn retries_provider_finish_error_without_accepting_partial_calls() {
    use std::sync::atomic::Ordering;
    let failed = format!(
        "{}data: [DONE]\n\n",
        event(
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"discard","function":{"name":"document_edit","arguments":"{\"action\":\"create\"}"}}]},"finish_reason":"error"}]})
        )
    );
    let valid = format!(
        "{}data: [DONE]\n\n",
        event(json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]}))
    );
    let (url, server, calls) = sequenced_server(vec![failed, valid]).await;
    let c = Config {
        base_url: url,
        retries: 1,
        ..Default::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let out = OpenAiClient
        .complete(json!({"messages":[]}), &c, CancellationToken::new(), tx)
        .await
        .unwrap();
    assert_eq!(out.attempts, 2);
    assert_eq!(out.text, "OK");
    assert!(out.calls.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn retries_malformed_tool_arguments_once_then_returns_only_complete_call() {
    use std::sync::atomic::Ordering;
    let malformed = format!(
        "{}data: [DONE]\n\n",
        event(
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"broken","function":{"name":"memory_write","arguments":"{\"body\":\"unfinished"}}]},"finish_reason":"tool_calls"}]})
        )
    );
    let valid = format!(
        "{}data: [DONE]\n\n",
        event(
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"good","function":{"name":"memory_write","arguments":"{\"body\":\"saved\"}"}}]},"finish_reason":"tool_calls"}]})
        )
    );
    let (url, server, calls) = sequenced_server(vec![malformed, valid]).await;
    let c = Config {
        base_url: url,
        retries: 2,
        ..Default::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let out = OpenAiClient
        .complete(json!({"messages":[]}), &c, CancellationToken::new(), tx)
        .await
        .unwrap();
    assert_eq!(out.attempts, 2);
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].id, "good");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn repeated_provider_finish_error_stops_at_retry_limit() {
    use std::sync::atomic::Ordering;
    let failed = format!(
        "{}data: [DONE]\n\n",
        event(json!({"choices":[{"delta":{},"finish_reason":"error"}]}))
    );
    let (url, server, calls) = sequenced_server(vec![failed]).await;
    let c = Config {
        base_url: url,
        retries: 2,
        ..Default::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let error = OpenAiClient
        .complete(json!({"messages":[]}), &c, CancellationToken::new(), tx)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "provider_stream_error: finish_reason=error"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    server.abort();
}

#[tokio::test]
async fn malformed_sse_event_is_labeled_and_retried_once() {
    use std::sync::atomic::Ordering;
    let invalid = "data: {\"choices\":\"unterminated\n\ndata: [DONE]\n\n".to_string();
    let valid = format!(
        "{}data: [DONE]\n\n",
        event(json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]}))
    );
    let (url, server, calls) = sequenced_server(vec![invalid, valid]).await;
    let c = Config {
        base_url: url,
        retries: 2,
        ..Default::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let out = OpenAiClient
        .complete(json!({"messages":[]}), &c, CancellationToken::new(), tx)
        .await
        .unwrap();
    assert_eq!(out.attempts, 2);
    assert_eq!(out.text, "OK");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn length_retains_last_delta_and_usage_but_discards_entire_tool_batch() {
    let body = [
        event(json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"valid","function":{"name":"document_edit","arguments":"{\"action\":\"create\",\"text\":\"unsafe\"}"}},
            {"index":1,"id":"partial","function":{"name":"document_edit","arguments":"{\"text\":"}}
        ]},"finish_reason":null}]})),
        event(json!({"choices":[{"delta":{"content":"받은 답변"},"finish_reason":"length"}]})),
        event(json!({"choices":[],"usage":{"prompt_tokens":123,"completion_tokens":16000,"prompt_tokens_details":{"cached_tokens":20}}})),
        "data: [DONE]\n\n".into(),
    ].concat();
    let (url, server) = server(body).await;
    let config = Config {
        base_url: url,
        ..Default::default()
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let result = OpenAiClient
        .complete(
            json!({"messages":[]}),
            &config,
            CancellationToken::new(),
            tx,
        )
        .await
        .unwrap();
    assert!(result.length_limited && result.discarded_tool_calls);
    assert!(result.calls.is_empty());
    assert_eq!(result.text, "받은 답변");
    assert_eq!(rx.recv().await.as_deref(), Some("받은 답변"));
    let usage = result.usage.unwrap();
    assert_eq!(
        (usage.input, usage.output, usage.cached),
        (123, 16000, Some(20))
    );
    server.abort();
}

#[tokio::test]
async fn length_without_done_remains_a_stream_error() {
    let (url, server) = server(event(
        json!({"choices":[{"delta":{"content":"partial"},"finish_reason":"length"}]}),
    ))
    .await;
    let config = Config {
        base_url: url,
        ..Default::default()
    };
    let (tx, _) = tokio::sync::mpsc::channel(8);
    let error = OpenAiClient
        .complete(
            json!({"messages":[]}),
            &config,
            CancellationToken::new(),
            tx,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("stream_interrupted"));
    server.abort();
}

#[tokio::test]
async fn malformed_request_and_extreme_timeout_return_errors_without_panicking() {
    use futures_util::FutureExt;
    for request in [json!(true), json!([]), json!("invalid")] {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let result = std::panic::AssertUnwindSafe(OpenAiClient.complete(
            request,
            &Config::default(),
            CancellationToken::new(),
            tx,
        ))
        .catch_unwind()
        .await;
        assert!(result.is_ok(), "request shape caused a panic");
        assert!(result.unwrap().is_err());
    }
    let c = Config {
        request_timeout_secs: u64::MAX,
        ..Default::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let result = std::panic::AssertUnwindSafe(OpenAiClient.complete(
        json!({"messages":[]}),
        &c,
        CancellationToken::new(),
        tx,
    ))
    .catch_unwind()
    .await;
    assert!(result.is_ok(), "timeout creation caused a panic");
    assert!(result.unwrap().is_err());
}

#[tokio::test]
async fn cancelling_openai_client_unblocks_a_full_delta_channel() {
    use std::time::Duration;
    let body = format!(
        "{}{}data: [DONE]\n\n",
        event(json!({"choices":[{"delta":{"content":"first"}}]})),
        event(json!({"choices":[{"delta":{"content":"second"},"finish_reason":"stop"}]}))
    );
    let (url, server) = server(body).await;
    let c = Config {
        base_url: url,
        request_timeout_secs: 60,
        ..Default::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let observer = tx.clone();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let mut job = tokio::spawn(async move {
        OpenAiClient
            .complete(json!({"messages":[]}), &c, token, tx)
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while observer.capacity() != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), &mut job).await;
    job.abort();
    server.abort();
    assert!(
        result
            .expect("cancel waited for request timeout instead of interrupting send")
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
}

#[tokio::test]
async fn rejected_json_schema_degrades_to_json_mode_and_is_remembered() {
    use std::sync::{Arc, Mutex};
    let formats = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = formats.clone();
    let app = Router::new().route(
        "/chat/completions",
        post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let seen = seen.clone();
            async move {
                let format = body["response_format"]["type"]
                    .as_str()
                    .unwrap_or("none")
                    .to_owned();
                seen.lock().unwrap().push(format.clone());
                if format == "json_schema" {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        "response_format json_schema is not supported",
                    )
                        .into_response();
                }
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    format!(
                        "{}data: [DONE]\n\n",
                        event(json!({"choices":[{"delta":{"content":"{\"issues\":[]}"},"finish_reason":"stop"}]}))
                    ),
                )
                    .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let c = Config {
        base_url: format!("http://{}", listener.local_addr().unwrap()),
        model: "schema-fallback-test-model".into(),
        ..Default::default()
    };
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let request = json!({"messages":[],"response_format":mnemoarc::tools::document_review::response_format()});
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let first = OpenAiClient
        .complete(request.clone(), &c, CancellationToken::new(), tx)
        .await
        .unwrap();
    assert_eq!(first.text, "{\"issues\":[]}");
    assert_eq!(first.attempts, 2);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let second = OpenAiClient
        .complete(request, &c, CancellationToken::new(), tx)
        .await
        .unwrap();
    assert_eq!(second.attempts, 1);
    assert_eq!(
        *formats.lock().unwrap(),
        ["json_schema", "json_object", "json_object"]
    );
    server.abort();
}

#[tokio::test]
async fn request_timeout_bounds_silence_not_a_long_streaming_answer() {
    use futures_util::stream;
    let app = Router::new().route(
        "/chat/completions",
        post(|| async {
            // Six chunks 400ms apart: 2.4s in total, never 1s of silence.
            let chunks = stream::unfold(0, |i| async move {
                if i == 6 {
                    return None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                let body = if i == 5 {
                    format!(
                        "{}data: [DONE]\n\n",
                        event(json!({"choices":[{"delta":{"content":"."},"finish_reason":"stop"}]}))
                    )
                } else {
                    event(json!({"choices":[{"delta":{"content":"."}}]}))
                };
                Some((Ok::<_, std::convert::Infallible>(body), i + 1))
            });
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                axum::body::Body::from_stream(chunks),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let c = Config {
        base_url: format!("http://{}", listener.local_addr().unwrap()),
        request_timeout_secs: 1,
        retries: 0,
        ..Default::default()
    };
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let result = OpenAiClient
        .complete(json!({"messages":[]}), &c, CancellationToken::new(), tx)
        .await
        .unwrap();
    assert_eq!(result.text, "......");
    assert_eq!(result.attempts, 1);
    server.abort();
}

#[tokio::test]
async fn silent_request_times_out_and_is_retried() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let app = Router::new().route(
        "/chat/completions",
        post(move || {
            let count = counter.fetch_add(1, Ordering::SeqCst);
            async move {
                if count == 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    format!(
                        "{}data: [DONE]\n\n",
                        event(json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]}))
                    ),
                )
                    .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let c = Config {
        base_url: format!("http://{}", listener.local_addr().unwrap()),
        request_timeout_secs: 1,
        retries: 1,
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
    server.abort();
}
