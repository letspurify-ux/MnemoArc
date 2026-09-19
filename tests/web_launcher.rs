use std::{
    process::{Command, Stdio},
    time::Duration,
};

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
        .args(["web", "--port", &port.to_string(), "--shutdown-on-stdin"])
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
