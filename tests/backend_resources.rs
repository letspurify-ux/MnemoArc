use axum::{Router, body::Body, http::header, routing::post};
use mnemoarc::{
    config::{Config, Project},
    web,
};
use serde_json::{Value, json};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn shutdown_and_abort_close_connections_with_unfinished_request_bodies() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    for abort in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reserved.local_addr().unwrap().port();
        drop(reserved);
        let config = Config {
            projects: vec![Project {
                root: dir.path().into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut backend = tokio::spawn(web::serve_app(
            config,
            dir.path().join("config.toml"),
            Some(port),
            None,
            false,
            false,
        ));
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{port}");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if client.get(format!("{url}/api/state")).send().await.is_ok() {
                    break;
                }
                assert!(!backend.is_finished());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let mut stalled = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stalled
            .write_all(format!("POST /api/sessions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-mnemoarc-client: web\r\nContent-Type: application/json\r\nContent-Length: 100\r\nExpect: 100-continue\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut interim = [0; 25];
        tokio::time::timeout(Duration::from_secs(3), stalled.read_exact(&mut interim))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        // The server is now extracting JSON, while this socket stays open
        // without sending the rest of its declared body.
        if abort {
            backend.abort();
            assert!((&mut backend).await.unwrap_err().is_cancelled());
        } else {
            client
                .post(format!("{url}/api/shutdown"))
                .header("x-mnemoarc-client", "web")
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
        }
        let closed = tokio::time::timeout(Duration::from_secs(2), async {
            let mut bytes = [0; 1024];
            loop {
                match stalled.read(&mut bytes).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        })
        .await;
        // Always unblock a regressed server before asserting, so a failure
        // cannot itself leave a detached connection task in the test runtime.
        drop(stalled);
        if !abort {
            tokio::time::timeout(Duration::from_secs(3), backend)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        closed.expect("server shutdown retained an unfinished request and its connection");
    }
}

#[tokio::test]
async fn shutdown_finishes_when_a_download_client_stops_reading() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = tempfile::tempdir().unwrap();
    // A sparse file exceeds the socket buffers without allocating its full
    // contents in memory or writing a large fixture to disk.
    std::fs::File::create(dir.path().join("output.md"))
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    let config = Config {
        projects: vec![Project {
            root: dir.path().into(),
            output: "output.md".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut backend = tokio::spawn(web::serve_app(
        config,
        dir.path().join("config.toml"),
        Some(port),
        None,
        false,
        false,
    ));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let state = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(response) = client.get(format!("{url}/api/state")).send().await {
                break response.json::<Value>().await.unwrap();
            }
            assert!(!backend.is_finished());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let mut stalled = socket.connect(([127, 0, 0, 1], port).into()).await.unwrap();
    let id = state["sessions"][0]["id"].as_str().unwrap();
    stalled
        .write_all(
            format!(
                "GET /api/sessions/{id}/output/download HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut headers = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(stalled.read_u8().await.unwrap());
            assert!(headers.len() < 4096);
        }
    })
    .await
    .unwrap();
    assert!(headers.starts_with(b"HTTP/1.1 200"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    client
        .post(format!("{url}/api/shutdown"))
        .header("x-mnemoarc-client", "web")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(2), &mut backend).await;
    drop(stalled);
    if stopped.is_err() {
        backend.abort();
        let _ = backend.await;
    }
    stopped
        .expect("server shutdown waited for the download client to resume reading")
        .unwrap()
        .unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn restarting_stdin_managed_servers_does_not_accumulate_reader_threads() {
    const PROBE: &str = "MNEMOARC_STDIN_RESOURCE_TEST_CHILD";
    if std::env::var_os(PROBE).is_none() {
        // Keep stdin open in a fresh process while it repeatedly starts and
        // stops servers. Its thread count is unaffected by concurrent tests.
        use std::process::{Command, Stdio};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "restarting_stdin_managed_servers_does_not_accumulate_reader_threads",
                "--nocapture",
            ])
            .env(PROBE, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let timed_out = child.try_wait().unwrap().is_none();
        if timed_out {
            child.kill().unwrap();
        }
        let output = child.wait_with_output().unwrap();
        drop(input);
        assert!(!timed_out, "server restart probe did not finish");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    #[cfg(target_os = "linux")]
    fn threads() -> usize {
        std::fs::read_dir("/proc/self/task").unwrap().count()
    }
    #[cfg(target_os = "macos")]
    fn threads() -> usize {
        let output = std::process::Command::new("/bin/ps")
            .args(["-M", "-p", &std::process::id().to_string()])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .skip(1)
            .count()
    }

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap();
            let mut initial = None;
            for _ in 0..6 {
                let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let port = reserved.local_addr().unwrap().port();
                drop(reserved);
                let config = Config {
                    projects: vec![Project {
                        root: dir.path().into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                };
                let server = tokio::spawn(web::serve_app(
                    config,
                    dir.path().join("config.toml"),
                    Some(port),
                    None,
                    true,
                    false,
                ));
                let url = format!("http://127.0.0.1:{port}");
                tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        if client.get(format!("{url}/api/state")).send().await.is_ok() {
                            break;
                        }
                        assert!(!server.is_finished());
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap();
                client
                    .post(format!("{url}/api/shutdown"))
                    .header("x-mnemoarc-client", "web")
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(3), server)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                let count = threads();
                let baseline = *initial.get_or_insert(count);
                assert!(
                    count <= baseline,
                    "stdin reader threads accumulated across restarts: {baseline} -> {count}"
                );
            }
        });
}

#[tokio::test]
async fn aborting_the_server_releases_active_model_connections_and_event_streams() {
    let entered = Arc::new(Notify::new());
    let dropped = CancellationToken::new();
    let upstream = Router::new().route(
        "/chat/completions",
        post({
            let entered = entered.clone();
            let dropped = dropped.clone();
            move || {
                let entered = entered.clone();
                let guard = dropped.clone().drop_guard();
                async move {
                    let stream =
                        futures_util::stream::unfold((guard, true), move |(guard, first)| {
                            let entered = entered.clone();
                            async move {
                                if first {
                                    entered.notify_one();
                                } else {
                                    tokio::time::sleep(Duration::from_millis(20)).await;
                                }
                                Some((Ok::<_, Infallible>(": keepalive\n\n"), (guard, false)))
                            }
                        });
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        Body::from_stream(stream),
                    )
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let upstream_server =
        tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    let config = Config {
        base_url,
        model: "gpt-4o".into(),
        model_context: Some(128000),
        disable_proxy: true,
        retries: 0,
        request_timeout_secs: 60,
        run_timeout_secs: 60,
        source_answer_review: false,
        completion_review_enabled: false,
        projects: vec![Project {
            root: dir.path().into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let path = dir.path().join("config.toml");
    let backend = tokio::spawn(web::serve_app(config, path, Some(port), None, false, false));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let state = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(response) = client.get(format!("{url}/api/state")).send().await {
                break response
                    .error_for_status()
                    .unwrap()
                    .json::<Value>()
                    .await
                    .unwrap();
            }
            assert!(!backend.is_finished(), "backend stopped during startup");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let events = client
        .get(format!("{url}/api/events"))
        .send()
        .await
        .unwrap();
    let id = state["sessions"][0]["id"].as_str().unwrap();
    client
        .post(format!("{url}/api/sessions/{id}/run"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({"text":"Wait for shutdown", "action":"chat"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    backend.abort();
    assert!(backend.await.unwrap_err().is_cancelled());
    let released = tokio::time::timeout(Duration::from_secs(2), dropped.cancelled()).await;
    let events_closed = tokio::time::timeout(Duration::from_secs(2), events.text()).await;
    upstream_server.abort();
    released.expect("aborting the server retained the provider connection until its run deadline");
    assert!(
        events_closed
            .expect("aborting the server retained its event stream")
            .is_ok()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_headless_work_exits_when_its_output_pipe_stops_reading() {
    for close_pipe in [false, true] {
        use std::process::Stdio;
        use tokio::io::AsyncReadExt;

        let dropped = CancellationToken::new();
        let upstream = Router::new().route(
            "/chat/completions",
            post({
                let dropped = dropped.clone();
                move || {
                    let guard = dropped.clone().drop_guard();
                    async move {
                        let stream = futures_util::stream::unfold(
                            (guard, true),
                            |(guard, first)| async move {
                                let text = if first {
                                    format!(
                                        "data: {}\n\n",
                                        json!({"choices":[{"delta":{"content":"x".repeat(2 * 1024 * 1024)}}]})
                                    )
                                } else {
                                    tokio::time::sleep(Duration::from_millis(20)).await;
                                    ": keepalive\n\n".into()
                                };
                                Some((Ok::<_, Infallible>(text), (guard, false)))
                            },
                        );
                        (
                            [(header::CONTENT_TYPE, "text/event-stream")],
                            Body::from_stream(stream),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let upstream_server =
            tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        Config {
            base_url,
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            api_key_env: "MNEMOARC_RESOURCE_TEST_KEY".into(),
            disable_proxy: true,
            retries: 0,
            request_timeout_secs: 60,
            run_timeout_secs: 60,
            source_answer_review: false,
            completion_review_enabled: false,
            ..Default::default()
        }
        .save(&dir.path().join("config.toml"))
        .unwrap();
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mnemoarc"))
            .args([
                "--config",
                "config.toml",
                "run",
                "--project",
                ".",
                "--prompt",
                "Wait for cancellation",
            ])
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut output = Some(child.stdout.take().unwrap());
        tokio::time::timeout(Duration::from_secs(10), output.as_mut().unwrap().read_u8())
            .await
            .unwrap()
            .unwrap();
        // Keep the pipe open without draining the multi-megabyte delta. The
        // renderer is blocked in the OS write, rather than waiting for model data.
        tokio::time::sleep(Duration::from_millis(100)).await;
        if close_pipe {
            drop(output.take());
        } else {
            assert_eq!(
                unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGINT) },
                0
            );
        }
        let stopped = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        if stopped.is_err() {
            child.kill().await.unwrap();
        }
        drop(output);
        let released = tokio::time::timeout(Duration::from_secs(2), dropped.cancelled()).await;
        upstream_server.abort();
        let _ = upstream_server.await;
        assert_eq!(
            stopped
                .expect(
                    "headless cancellation retained a blocked renderer and prevented process shutdown"
                )
                .unwrap()
                .code(),
            Some(0)
        );
        released.expect("headless cancellation retained its model connection");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn headless_failure_exits_when_stderr_is_full() {
    use std::{io::Write, os::unix::net::UnixStream, process::Stdio};

    let (consumer, mut producer) = UnixStream::pair().unwrap();
    producer.set_nonblocking(true).unwrap();
    loop {
        match producer.write(&[b'x'; 16 * 1024]) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("cannot fill stderr fixture: {error}"),
        }
    }
    producer.set_nonblocking(false).unwrap();
    let entered = Arc::new(Notify::new());
    let upstream = Router::new().route(
        "/chat/completions",
        post({
            let entered = entered.clone();
            move || {
                let entered = entered.clone();
                async move {
                    entered.notify_one();
                    (axum::http::StatusCode::BAD_REQUEST, "invalid request")
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let upstream_server =
        tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    Config {
        base_url,
        model: "gpt-4o".into(),
        model_context: Some(128_000),
        api_key_env: "MNEMOARC_RESOURCE_TEST_KEY".into(),
        disable_proxy: true,
        retries: 0,
        request_timeout_secs: 60,
        run_timeout_secs: 60,
        source_answer_review: false,
        completion_review_enabled: false,
        ..Default::default()
    }
    .save(&dir.path().join("config.toml"))
    .unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mnemoarc"))
        .args([
            "--config",
            "config.toml",
            "run",
            "--project",
            ".",
            "--prompt",
            "Return the provider failure",
        ])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::os::fd::OwnedFd::from(producer)))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
    if stopped.is_err() {
        child.kill().await.unwrap();
    }
    drop(consumer);
    upstream_server.abort();
    let _ = upstream_server.await;
    assert_eq!(
        stopped
            .expect("an unread diagnostic log retained the failed model request and Tokio runtime")
            .unwrap()
            .code(),
        Some(2)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn web_startup_and_shutdown_finish_with_an_unread_output_stream() {
    use std::{io::Write, os::unix::net::UnixStream, process::Stdio};

    let (consumer, mut producer) = UnixStream::pair().unwrap();
    producer.set_nonblocking(true).unwrap();
    loop {
        match producer.write(&[b'x'; 16 * 1024]) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("cannot fill stdout fixture: {error}"),
        }
    }
    producer.set_nonblocking(false).unwrap();
    let dir = tempfile::tempdir().unwrap();
    Config::default()
        .save(&dir.path().join("config.toml"))
        .unwrap();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mnemoarc"))
        .args([
            "--config",
            "config.toml",
            "web",
            "--no-open",
            "--port",
            &port.to_string(),
            "--shutdown-on-stdin",
        ])
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(producer)))
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let ready = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if client
                .get(format!("http://127.0.0.1:{port}/api/state"))
                .send()
                .await
                .is_ok()
            {
                break;
            }
            assert!(child.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    drop(child.stdin.take());
    let stopped = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
    if stopped.is_err() {
        child.kill().await.unwrap();
    }
    drop(consumer);
    ready.expect("a startup log blocked the bound HTTP listener from serving requests");
    assert!(
        stopped
            .expect("server shutdown waited for the output stream to resume reading")
            .unwrap()
            .success()
    );
}
