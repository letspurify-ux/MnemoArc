use std::{
    process::{Command, Stdio},
    time::Duration,
};

static DEFAULT_PORT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn launcher_stdin_eof_shuts_down_with_an_open_event_stream() {
    let dir = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let child = Command::new(env!("CARGO_BIN_EXE_mnemoarc"))
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
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = Server(child);
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if client.get(format!("{url}/api/state")).send().await.is_ok() {
                break;
            }
            assert!(
                server.0.try_wait().unwrap().is_none(),
                "server exited during startup"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    let _stream = client
        .get(format!("{url}/api/events"))
        .send()
        .await
        .unwrap();
    drop(server.0.stdin.take());
    let status = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = server.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    assert!(status.success());
}

// Exercise the distributable from a directory containing no frontend or source files.
#[tokio::test]
async fn standalone_assets_port_fallback_and_shutdown() {
    let _guard = DEFAULT_PORT.lock().await;
    use tokio::io::{AsyncBufReadExt, BufReader};
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join(if cfg!(windows) {
        "mnemoarc.exe"
    } else {
        "mnemoarc"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_mnemoarc"), &binary).unwrap();
    // If another local process already occupies 3030, that also exercises fallback.
    let _occupied = std::net::TcpListener::bind("127.0.0.1:3030").ok();
    let mut child = tokio::process::Command::new(&binary)
        .args(["web", "--no-open"])
        .env("MNEMOARC_CONFIG_DIR", dir.path().join("settings"))
        .current_dir(dir.path())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let url = line.strip_prefix("MnemoArc: ").unwrap();
    assert!(
        !url.ends_with(":3030"),
        "occupied default port must be bypassed"
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let response = client.get(url).send().await.unwrap();
    if mnemoarc::desktop::ASSETS.is_empty() {
        // Clean debug builds can run the Vite UI without a production bundle.
        assert_eq!(response.status(), 503);
    } else {
        assert_eq!(response.status(), 200);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        assert!(response.text().await.unwrap().contains("<div id=\"root\""));
        // Includes JS chunks loaded on demand by Mermaid, styles and bundled fonts.
        for (name, bytes) in mnemoarc::desktop::ASSETS {
            let response = client.get(format!("{url}/{name}")).send().await.unwrap();
            assert_eq!(response.status(), 200, "{name}");
            assert_eq!(response.bytes().await.unwrap().as_ref(), *bytes, "{name}");
        }
    }
    assert_eq!(
        client
            .get(format!("{url}/assets/missing.js"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        client
            .get(format!("{url}/api/unknown"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let state: serde_json::Value = client
        .get(format!("{url}/api/state"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let saved = client
        .put(format!("{url}/api/settings"))
        .header("X-MnemoArc-Client", "web")
        .json(&serde_json::json!({"config": state["config"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status(), 200);
    assert!(dir.path().join("settings/config.toml").is_file());
    let _stream = client
        .get(format!("{url}/api/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        client
            .post(format!("{url}/api/shutdown"))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .post(format!("{url}/api/shutdown"))
            .header("X-MnemoArc-Client", "web")
            .header("Origin", "https://example.com")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .post(format!("{url}/api/shutdown"))
            .header("X-MnemoArc-Client", "web")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!dir.path().join("frontend").exists());
}

#[tokio::test]
async fn explicit_port_collision_fails_instead_of_changing_port() {
    let dir = tempfile::tempdir().unwrap();
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied.local_addr().unwrap().port();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_mnemoarc"))
        .args([
            "--config",
            "config.toml",
            "web",
            "--no-open",
            "--port",
            &port.to_string(),
        ])
        .current_dir(dir.path())
        .kill_on_drop(true)
        .output();
    assert!(
        !tokio::time::timeout(Duration::from_secs(10), output)
            .await
            .unwrap()
            .unwrap()
            .status
            .success()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn default_launch_opens_actual_bound_url() {
    let _guard = DEFAULT_PORT.lock().await;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, BufReader};
    let dir = tempfile::tempdir().unwrap();
    let command = dir.path().join(if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    });
    std::fs::write(
        &command,
        "#!/bin/sh\nprintf '%s' \"$1\" > \"$BROWSER_CAPTURE\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();
    let capture = dir.path().join("opened-url");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mnemoarc"))
        .current_dir(dir.path())
        .env("PATH", dir.path())
        .env("MNEMOARC_CONFIG_DIR", dir.path().join("settings"))
        .env("BROWSER_CAPTURE", &capture)
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let url = line.strip_prefix("MnemoArc: ").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if std::fs::read_to_string(&capture).is_ok_and(|s| s == url) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    assert_eq!(
        client
            .post(format!("{url}/api/shutdown"))
            .header("X-MnemoArc-Client", "web")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
