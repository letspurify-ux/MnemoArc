mod support;
use axum::{Router, http::header, response::IntoResponse, routing::post};
use mnemoarc::{
    config::Config,
    llm::{
        AttemptAction, CompletionError, LlmClient, MAX_TOOL_CALL_ID_BYTES, MAX_TOOL_CALLS,
        MAX_TOOL_NAME_BYTES, OpenAiClient, SseDecoder,
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

#[test]
fn sse_accepts_mixed_line_endings_and_a_split_utf8_bom() {
    for raw in [
        "\u{feff}data: 한글🦀\r\rdata: [DONE]\r\r",
        "data: 한글🦀\r\n\ndata: [DONE]\n\r\n",
        "data: first\r\ndata:  second\rdata:\tthird\n\n",
    ] {
        let expected = if raw.contains("first") {
            vec!["first\n second\n\tthird"]
        } else {
            vec!["한글🦀", "[DONE]"]
        };
        for split in 0..=raw.len() {
            let mut parser = SseDecoder::default();
            let mut events = parser.feed(&raw.as_bytes()[..split]).unwrap();
            events.extend(parser.feed(&raw.as_bytes()[split..]).unwrap());
            assert_eq!(events, expected, "split {split}: {raw:?}");
        }
        let mut parser = SseDecoder::default();
        let mut events = vec![];
        for byte in raw.bytes() {
            events.extend(parser.feed(&[byte]).unwrap());
        }
        assert_eq!(events, expected);
    }
}

#[tokio::test]
async fn completion_terminator_ignores_later_events_in_the_same_chunk() {
    let body = format!(
        "{}data: [DONE]\n\n{}",
        event(json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]})),
        event(json!({"error":{"code":500,"message":"after completion"}}))
    );
    let (url, server) = server(body).await;
    let config = Config {
        base_url: url,
        retries: 0,
        ..support::compact_config()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let result = OpenAiClient
        .complete(
            json!({"messages":[]}),
            &config,
            CancellationToken::new(),
            tx,
        )
        .await;
    server.abort();
    let completion = result.unwrap();
    assert_eq!(completion.text, "OK");
}

#[tokio::test]
async fn system_dns_preserves_the_explicit_port_of_a_local_completion_server() {
    let body = format!(
        "{}data: [DONE]\n\n",
        event(json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]}))
    );
    let (url, server) = server(body).await;
    let config = Config {
        base_url: url.replace("127.0.0.1", "localhost"),
        disable_proxy: true,
        retries: 0,
        ..support::compact_config()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let completion = OpenAiClient
        .complete(
            json!({"messages":[]}),
            &config,
            CancellationToken::new(),
            tx,
        )
        .await;
    server.abort();
    let _ = server.await;
    assert_eq!(completion.unwrap().text, "OK");
}

#[tokio::test]
async fn connection_probe_requires_valid_plain_and_final_answers() {
    let tool = format!(
        "{}data: [DONE]\n\n",
        event(
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"echo","function":{"name":"connection_echo","arguments":"{\"text\":\"OK\"}"}}]},"finish_reason":"tool_calls"}]})
        )
    );
    let tool_with_stop = format!(
        "{}data: [DONE]\n\n",
        event(
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"echo","function":{"name":"connection_echo","arguments":"{\"text\":\"OK\"}"}}]},"finish_reason":"stop"}]})
        )
    );
    let text = format!(
        "{}data: [DONE]\n\n",
        event(json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]}))
    );
    let empty = format!(
        "{}data: [DONE]\n\n",
        event(json!({"choices":[{"delta":{},"finish_reason":"stop"}]}))
    );
    let plain =
        json!({"choices":[{"message":{"content":"OK"},"finish_reason":"stop"}]}).to_string();
    for (first, tool_response, last, expected_error) in [
        (
            "<html>upstream unavailable</html>".into(),
            tool.clone(),
            text.clone(),
            Some("plain response probe failed"),
        ),
        (
            json!({"error":{"message":"upstream unavailable"}}).to_string(),
            tool.clone(),
            text.clone(),
            Some("plain response probe failed"),
        ),
        (
            json!({"choices":[{"message":{"content":"OK"}}]}).to_string(),
            tool.clone(),
            text.clone(),
            Some("plain response probe failed"),
        ),
        (
            plain.clone(),
            tool.clone(),
            tool.clone(),
            Some("tool round-trip probe failed"),
        ),
        (
            plain.clone(),
            tool.clone(),
            empty,
            Some("tool round-trip probe failed"),
        ),
        (plain.clone(), tool, text.clone(), None),
        (plain, tool_with_stop, text, None),
    ] {
        let (url, server, _) = sequenced_server(vec![first, tool_response, last]).await;
        let config = Config {
            base_url: url,
            model: "probe-model".into(),
            model_context: Some(128_000),
            retries: 0,
            ..support::compact_config()
        };
        let result = OpenAiClient.probe(&config).await;
        server.abort();
        if let Some(expected) = expected_error {
            assert!(result.is_err(), "probe accepted an invalid response");
            assert!(result.unwrap_err().to_string().contains(expected));
        } else {
            assert!(result.is_ok(), "{result:?}");
        }
    }
}

#[tokio::test]
async fn connection_probe_rejects_a_tool_call_with_the_wrong_arguments() {
    let plain =
        json!({"choices":[{"message":{"content":"OK"},"finish_reason":"stop"}]}).to_string();
    let final_text = format!(
        "{}data: [DONE]\n\n",
        event(json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]}))
    );
    for arguments in ["{}", "{\"text\":\"NOT OK\"}", "{\"text\":7}"] {
        let tool = format!(
            "{}data: [DONE]\n\n",
            event(
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"echo","function":{"name":"connection_echo","arguments":arguments}}]},"finish_reason":"tool_calls"}]})
            )
        );
        let (url, server, _) =
            sequenced_server(vec![plain.clone(), tool, final_text.clone()]).await;
        let config = Config {
            base_url: url,
            model: "probe-arguments-test-model".into(),
            model_context: Some(128_000),
            retries: 0,
            ..support::compact_config()
        };
        let result = OpenAiClient.probe(&config).await;
        server.abort();
        assert!(result.unwrap_err().to_string().contains("tool probe"));
    }
}
#[tokio::test]
async fn completion_terminator_ignores_invalid_utf8_after_the_final_event() {
    let mut body = format!(
        "{}data: [DONE]\n\n",
        event(json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]}))
    )
    .into_bytes();
    body.extend_from_slice(b"data: \xff\n\n");
    let (url, server) = server(body).await;
    let config = Config {
        base_url: url,
        retries: 0,
        disable_proxy: true,
        ..support::compact_config()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let result = OpenAiClient
        .complete(
            json!({"messages":[]}),
            &config,
            CancellationToken::new(),
            tx,
        )
        .await;
    server.abort();
    assert_eq!(result.unwrap().text, "OK");
}

async fn server(body: impl Into<axum::body::Bytes>) -> (String, tokio::task::JoinHandle<()>) {
    let body = body.into();
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
async fn connection_probe_times_out_despite_stream_keepalives() {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    let count = Arc::new(AtomicUsize::new(0));
    let received = count.clone();
    let app = Router::new().route(
        "/chat/completions",
        post(move || {
            let received = received.clone();
            async move {
                if received.fetch_add(1, Ordering::SeqCst) == 0 {
                    return axum::Json(
                        json!({"choices":[{"message":{"content":"OK"},"finish_reason":"stop"}]}),
                    )
                    .into_response();
                }
                let stream = futures_util::stream::unfold((), |_| async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Some((Ok::<_, std::convert::Infallible>(": keepalive\n\n"), ()))
                });
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    axum::body::Body::from_stream(stream),
                )
                    .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config {
        base_url: format!("http://{}", listener.local_addr().unwrap()),
        model: "mock".into(),
        model_context: Some(128000),
        request_timeout_secs: 1,
        run_timeout_secs: 1,
        retries: 0,
        disable_proxy: true,
        ..support::compact_config()
    };
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let result = tokio::time::timeout(Duration::from_secs(3), OpenAiClient.probe(&config)).await;
    server.abort();
    let error = result
        .expect("connection probe ignored its overall deadline")
        .unwrap_err();
    assert!(
        error.to_string().contains("connection_probe_timeout"),
        "{error}"
    );
    assert!(count.load(Ordering::SeqCst) >= 2);
}
#[tokio::test]
async fn assemble_interleaved_calls_and_usage() {
    let body=[event(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"memory_read","arguments":"{\"id\":"}},{"index":1,"id":"c2","function":{"name":"memory_read","arguments":"{\"id\":"}}]},"finish_reason":null}]})),event(json!({"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"\"b\"}"}},{"index":0,"function":{"arguments":"\"a\"}"}}]},"finish_reason":"tool_calls"}]})),event(json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":3}}})),"data: [DONE]\n\n".into()].concat();
    let (url, server) = server(body).await;
    let c = Config {
        base_url: url,
        model: "mock".into(),
        model_context: Some(64000),
        ..support::compact_config()
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
            ..support::compact_config()
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
        ..support::compact_config()
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
async fn stop_finish_never_exposes_an_incomplete_tool_call() {
    let body = format!(
        "{}data: [DONE]\n\n",
        event(
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"echo","function":{"name":"connection_echo","arguments":"{\"text\":"}}]},"finish_reason":"stop"}]})
        )
    );
    let (url, server) = server(body).await;
    let config = Config {
        base_url: url,
        retries: 0,
        ..support::compact_config()
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
    server.abort();
    assert!(
        error.to_string().contains("invalid_tool_arguments"),
        "{error}"
    );
}

#[tokio::test]
async fn repeated_identical_finish_reason_accepts_provider_tool_call_stream() {
    let body = [
        event(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"ping-1","function":{"name":"ping","arguments":"{}"}}]},"finish_reason":null}]})),
        event(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})),
        event(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})),
        event(json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":4}})),
        "data: [DONE]\n\n".into(),
    ]
    .concat();
    let (url, server) = server(body).await;
    let config = Config {
        base_url: url,
        retries: 0,
        ..support::compact_config()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let completion = OpenAiClient
        .complete(
            json!({"messages":[]}),
            &config,
            CancellationToken::new(),
            tx,
        )
        .await
        .unwrap();
    server.abort();
    assert_eq!(completion.calls.len(), 1);
    assert_eq!(completion.calls[0].name, "ping");
    assert_eq!(completion.usage.unwrap().output, 4);
}

#[tokio::test]
async fn contradictory_stream_finish_reasons_never_complete() {
    for (body, expected) in [
        (
            format!(
                "{}data: [DONE]\n\n",
                event(json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"tool_calls"}]}))
            ),
            "tool_calls finish without a call",
        ),
        (
            format!(
                "{}{}data: [DONE]\n\n",
                event(
                    json!({"choices":[{"delta":{"content":"partial"},"finish_reason":"length"}]})
                ),
                event(json!({"choices":[{"delta":{"content":"final"},"finish_reason":"stop"}]}))
            ),
            "delta after finish reason",
        ),
        (
            format!(
                "{}{}data: [DONE]\n\n",
                event(json!({"choices":[{"delta":{},"finish_reason":"length"}]})),
                event(json!({"choices":[{"delta":{},"finish_reason":"stop"}]}))
            ),
            "multiple finish reasons",
        ),
    ] {
        let (url, server) = server(body).await;
        let config = Config {
            base_url: url,
            retries: 0,
            ..support::compact_config()
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let result = OpenAiClient
            .complete(
                json!({"messages":[]}),
                &config,
                CancellationToken::new(),
                tx,
            )
            .await;
        server.abort();
        assert!(result.unwrap_err().to_string().contains(expected));
    }
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
        ..support::compact_config()
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
        ..support::compact_config()
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
        ..support::compact_config()
    };
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let result = OpenAiClient
        .complete(json!({"messages":[]}), &c, CancellationToken::new(), tx)
        .await
        .unwrap();
    assert_eq!(result.text, "OK");
    assert_eq!(result.attempts, 2);
    assert_eq!(
        json!(result.attempt_diagnostics),
        json!([{"attempt":1,"code":"http_429","reason":"http_429: retry","action":"retry"}])
    );
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
        ..support::compact_config()
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
        ..support::compact_config()
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
        ..support::compact_config()
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
    let provider = error.downcast_ref::<CompletionError>().unwrap();
    assert_eq!(provider.attempts(), 3);
    assert_eq!(
        json!(provider.attempt_diagnostics()),
        json!([
            {"attempt":1,"code":"provider_stream_error","reason":"provider_stream_error: finish_reason=error","action":"retry"},
            {"attempt":2,"code":"provider_stream_error","reason":"provider_stream_error: finish_reason=error","action":"retry"},
            {"attempt":3,"code":"provider_stream_error","reason":"provider_stream_error: finish_reason=error","action":"stop"}
        ])
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    server.abort();
}

#[tokio::test]
async fn interrupted_stream_is_retried_until_the_retry_limit() {
    use std::sync::atomic::Ordering;
    let interrupted = event(
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"discard","function":{"name":"memory_write","arguments":"{\"body\":\"x\"}"}}]},"finish_reason":"tool_calls"}]}),
    );
    let valid = format!(
        "{}data: [DONE]\n\n",
        event(json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]}))
    );
    let (url, server, calls) = sequenced_server(vec![interrupted.clone(), valid]).await;
    let c = Config {
        base_url: url,
        retries: 1,
        ..support::compact_config()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let out = OpenAiClient
        .complete(json!({"messages":[]}), &c, CancellationToken::new(), tx)
        .await
        .unwrap();
    assert_eq!(out.attempts, 2);
    assert_eq!(out.text, "OK");
    assert!(
        out.calls.is_empty(),
        "the interrupted batch must be discarded"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();

    let (url, server, calls) = sequenced_server(vec![interrupted]).await;
    let c = Config {
        base_url: url,
        retries: 1,
        ..support::compact_config()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let error = OpenAiClient
        .complete(json!({"messages":[]}), &c, CancellationToken::new(), tx)
        .await
        .unwrap_err();
    assert!(error.to_string().starts_with("stream_interrupted:"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn interrupted_stream_after_visible_text_is_not_retried() {
    use std::sync::atomic::Ordering;
    let partial = event(json!({"choices":[{"delta":{"content":"visible"}}]}));
    let (url, server, calls) = sequenced_server(vec![partial]).await;
    let c = Config {
        base_url: url,
        retries: 2,
        ..support::compact_config()
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let error = OpenAiClient
        .complete(json!({"messages":[]}), &c, CancellationToken::new(), tx)
        .await
        .unwrap_err();
    assert!(error.to_string().starts_with("stream_interrupted:"));
    assert_eq!(rx.recv().await.as_deref(), Some("visible"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
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
        ..support::compact_config()
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
        ..support::compact_config()
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
        retries: 0,
        ..support::compact_config()
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
            &support::compact_config(),
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
        ..support::compact_config()
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
        ..support::compact_config()
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
        ..support::compact_config()
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
    assert_eq!(
        json!(first.attempt_diagnostics),
        json!([{"attempt":1,"code":"http_400","reason":"http_400: response_format json_schema is not supported","action":"json_mode_fallback"}])
    );
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let second = OpenAiClient
        .complete(request, &c, CancellationToken::new(), tx)
        .await
        .unwrap();
    assert_eq!(second.attempts, 1);
    assert!(second.attempt_diagnostics.is_empty());
    assert_eq!(
        *formats.lock().unwrap(),
        ["json_schema", "json_object", "json_object"]
    );
    server.abort();
}

#[tokio::test]
async fn rejected_json_mode_records_fallback_without_response_format() {
    let app = Router::new().route(
        "/chat/completions",
        post(|axum::Json(body): axum::Json<serde_json::Value>| async move {
            if body.get("response_format").is_some() {
                (axum::http::StatusCode::BAD_REQUEST, "json mode is not supported").into_response()
            } else {
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    format!("{}data: [DONE]\n\n", event(json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]}))),
                ).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config {
        base_url: format!("http://{}", listener.local_addr().unwrap()),
        retries: 0,
        ..support::compact_config()
    };
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let result = OpenAiClient
        .complete(
            json!({"messages":[],"response_format":{"type":"json_object"}}),
            &config,
            CancellationToken::new(),
            tx,
        )
        .await
        .unwrap();
    assert_eq!(result.text, "OK");
    assert_eq!(result.attempts, 2);
    assert_eq!(
        json!(result.attempt_diagnostics),
        json!([
            {"attempt":1,"code":"http_400","reason":"http_400: json mode is not supported","action":"unstructured_fallback"}
        ])
    );
    server.abort();
}

#[tokio::test]
async fn sse_schema_failure_is_cached_after_two_recoveries_and_only_for_that_schema() {
    use std::sync::{Arc, Mutex};
    let formats = Arc::new(Mutex::new(Vec::new()));
    let seen = formats.clone();
    let app=Router::new().route("/chat/completions",post(move |axum::Json(body):axum::Json<serde_json::Value>| {
        let seen=seen.clone();
        async move {
            let format=body["response_format"]["type"].as_str().unwrap_or("none").to_owned();
            let failing=format=="json_schema" && body["response_format"]["json_schema"]["name"]=="document_review";
            seen.lock().unwrap().push(format);
            let content=if failing { event(json!({"error":{"code":502,"message":"JSON error injected into SSE stream"}})) }
                else { format!("{}data: [DONE]\n\n",event(json!({"choices":[{"delta":{"content":"{\"issues\":[]}"},"finish_reason":"stop"}]}))) };
            ([(header::CONTENT_TYPE,"text/event-stream")],content)
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config {
        base_url: format!("http://{}", listener.local_addr().unwrap()),
        model: "schema-stream-recovery".into(),
        retries: 0,
        ..support::compact_config()
    };
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut request = json!({"messages":[],"response_format":mnemoarc::tools::document_review::response_format()});
    // One recovery may be an outage that cleared in time; the second request
    // with the same schema confirms the grammar failure and caches it.
    for attempts in [2, 2, 1] {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let result = OpenAiClient
            .complete(request.clone(), &config, CancellationToken::new(), tx)
            .await
            .unwrap();
        assert_eq!(result.attempts, attempts);
        if attempts == 2 {
            assert_eq!(result.attempt_diagnostics.len(), 1);
            assert_eq!(result.attempt_diagnostics[0].attempt, 1);
            assert_eq!(result.attempt_diagnostics[0].code, "provider_stream_error");
            assert_eq!(
                result.attempt_diagnostics[0].action,
                AttemptAction::JsonModeFallback
            );
        } else {
            assert!(result.attempt_diagnostics.is_empty());
        }
    }
    request["response_format"]["json_schema"]["name"] = json!("different_schema");
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    assert_eq!(
        OpenAiClient
            .complete(request, &config, CancellationToken::new(), tx)
            .await
            .unwrap()
            .attempts,
        1
    );
    assert_eq!(
        *formats.lock().unwrap(),
        [
            "json_schema",
            "json_object",
            "json_schema",
            "json_object",
            "json_object",
            "json_schema"
        ]
    );
    server.abort();
}

#[tokio::test]
async fn an_outage_cleared_by_the_json_mode_attempt_keeps_strict_schema_output() {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    // The first request's strict attempts hit an in-stream 502 outage that
    // clears exactly when the JSON-mode comparison attempt is sent.
    let failures_left = Arc::new(AtomicUsize::new(1));
    let formats = Arc::new(Mutex::new(Vec::new()));
    let (left, seen) = (failures_left.clone(), formats.clone());
    let app=Router::new().route("/chat/completions",post(move |axum::Json(body):axum::Json<serde_json::Value>| {
        let (left,seen)=(left.clone(),seen.clone());
        async move {
            let format=body["response_format"]["type"].as_str().unwrap_or("none").to_owned();
            let failing=format=="json_schema" && left.load(Ordering::SeqCst)>0;
            if failing { left.fetch_sub(1,Ordering::SeqCst); }
            seen.lock().unwrap().push(format);
            let content=if failing { event(json!({"error":{"code":502,"message":"JSON error injected into SSE stream","metadata":{"error_type":"provider_unavailable"}}})) }
                else { format!("{}data: [DONE]\n\n",event(json!({"choices":[{"delta":{"content":"{\"issues\":[]}"},"finish_reason":"stop"}]}))) };
            ([(header::CONTENT_TYPE,"text/event-stream")],content)
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config {
        base_url: format!("http://{}", listener.local_addr().unwrap()),
        model: "schema-outage-cleared".into(),
        retries: 0,
        ..support::compact_config()
    };
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let request = json!({"messages":[],"response_format":mnemoarc::tools::document_review::response_format()});
    for (attempts, fail_next) in [(2, 0), (1, 1), (2, 0), (1, 0)] {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let result = OpenAiClient
            .complete(request.clone(), &config, CancellationToken::new(), tx)
            .await
            .unwrap();
        assert_eq!(result.attempts, attempts);
        failures_left.store(fail_next, Ordering::SeqCst);
    }
    // The strict success on the second request cleared the first recovery,
    // so the third recovery is again only a suspicion, not a cached failure.
    assert_eq!(
        *formats.lock().unwrap(),
        [
            "json_schema",
            "json_object",
            "json_schema",
            "json_schema",
            "json_object",
            "json_schema"
        ]
    );
    server.abort();
}

#[tokio::test]
async fn an_outage_in_both_formats_is_bounded_and_does_not_poison_schema_cache() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let available = Arc::new(AtomicBool::new(false));
    let state = available.clone();
    let app=Router::new().route("/chat/completions",post(move |axum::Json(body):axum::Json<serde_json::Value>| {
        let state=state.clone();
        async move {
            let content=if !state.load(Ordering::SeqCst) || body["response_format"]["type"]=="json_schema" {
                event(json!({"error":{"code":502,"message":"provider unavailable"}}))
            } else {format!("{}data: [DONE]\n\n",event(json!({"choices":[{"delta":{"content":"{\"issues\":[]}"},"finish_reason":"stop"}]})))};
            ([(header::CONTENT_TYPE,"text/event-stream")],content)
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config {
        base_url: format!("http://{}", listener.local_addr().unwrap()),
        model: "schema-outage-recovery".into(),
        retries: 0,
        ..support::compact_config()
    };
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let request = json!({"messages":[],"response_format":mnemoarc::tools::document_review::response_format()});
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        OpenAiClient.complete(request.clone(), &config, CancellationToken::new(), tx),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().starts_with("provider_stream_error:"));
    available.store(true, Ordering::SeqCst);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    assert_eq!(
        OpenAiClient
            .complete(request, &config, CancellationToken::new(), tx)
            .await
            .unwrap()
            .attempts,
        2,
        "a failed comparison must not cache the schema as unsupported"
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
                        event(
                            json!({"choices":[{"delta":{"content":"."},"finish_reason":"stop"}]})
                        )
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
        ..support::compact_config()
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
                        event(
                            json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]})
                        )
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
        ..support::compact_config()
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

#[tokio::test]
async fn transient_error_events_in_a_stream_are_retried_and_others_are_not() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    for (error, retried) in [
        // The live shape: OpenRouter relays an upstream overload as an event.
        (
            json!({"code":503,"message":"Upstream error from DigitalOcean","metadata":{"error_type":"provider_overloaded"}}),
            true,
        ),
        (json!({"code":"429","message":"rate limited"}), true),
        (json!({"code":400,"message":"bad request"}), false),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = Router::new().route(
            "/chat/completions",
            post(move || {
                let count = counter.fetch_add(1, Ordering::SeqCst);
                let body = if count == 0 {
                    event(json!({"error":error}))
                } else {
                    format!(
                        "{}data: [DONE]\n\n",
                        event(
                            json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]})
                        )
                    )
                };
                async move { ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response() }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let c = Config {
            base_url: format!("http://{}", listener.local_addr().unwrap()),
            retries: 1,
            ..support::compact_config()
        };
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let result = OpenAiClient
            .complete(json!({"messages":[]}), &c, CancellationToken::new(), tx)
            .await;
        if retried {
            assert_eq!(result.unwrap().text, "OK");
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        } else {
            assert!(result.unwrap_err().to_string().contains("provider_error"));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
        server.abort();
    }
}
