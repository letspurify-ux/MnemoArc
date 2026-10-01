//! Standalone browser application packaging and platform integration.
use anyhow::{Context, Result};
use axum::{
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use std::{path::PathBuf, process::Command, time::Duration};
use tokio_util::sync::CancellationToken;

include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));

pub fn config_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("MNEMOARC_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("config.toml"));
    }
    // Keep existing installations and repository development configuration working.
    let local = PathBuf::from("config.toml");
    if local.is_file() {
        return Ok(local);
    }
    #[cfg(target_os = "windows")]
    let dir =
        PathBuf::from(std::env::var_os("APPDATA").context("APPDATA is unavailable; use --config")?)
            .join("MnemoArc");
    #[cfg(target_os = "macos")]
    let dir = PathBuf::from(std::env::var_os("HOME").context("HOME is unavailable; use --config")?)
        .join("Library/Application Support/MnemoArc");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let dir =
        match std::env::var_os("XDG_CONFIG_HOME").filter(|p| PathBuf::from(p).is_absolute()) {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(
                std::env::var_os("HOME").context("HOME is unavailable; use --config")?,
            )
            .join(".config"),
        }
        .join("mnemoarc");
    Ok(dir.join("config.toml"))
}

fn browser_command(url: &str) -> Command {
    #[cfg(target_os = "macos")]
    let command = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let command = {
        let mut c = Command::new("rundll32.exe");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let command = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    command
}

pub fn open_browser(url: &str) -> Result<()> {
    anyhow::ensure!(
        browser_command(url)
            .status()
            .context("Cannot launch the default browser")?
            .success(),
        "Default browser command failed"
    );
    Ok(())
}

pub(crate) async fn open_browser_managed(url: &str, cancel: &CancellationToken) -> Result<()> {
    let mut command = tokio::process::Command::from(browser_command(url));
    run_browser_command(&mut command, Duration::from_secs(15), cancel).await
}

async fn run_browser_command(
    command: &mut tokio::process::Command,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    // The opener can hang independently of the browser it launches. Reap it
    // on cancellation/timeout; kill_on_drop also covers an aborted future.
    if cancel.is_cancelled() {
        anyhow::bail!("cancelled");
    }
    let mut child = command
        .kill_on_drop(true)
        .spawn()
        .context("Cannot launch the default browser")?;
    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        result = tokio::time::timeout(timeout, child.wait()) => Some(result),
    };
    let status = match outcome {
        Some(Ok(result)) => result.context("Cannot wait for the default browser command")?,
        stopped => {
            child
                .kill()
                .await
                .context("Cannot stop the default browser command")?;
            if stopped.is_none() {
                anyhow::bail!("cancelled");
            }
            anyhow::bail!("Default browser command timed out");
        }
    };
    anyhow::ensure!(status.success(), "Default browser command failed");
    Ok(())
}

pub async fn embedded_ui(uri: Uri) -> Response {
    let name = match uri.path() {
        "/" => "index.html",
        path => path.trim_start_matches('/'),
    };
    if let Some((_, bytes)) = ASSETS.iter().find(|(path, _)| *path == name) {
        return (
            [
                (
                    header::CONTENT_TYPE,
                    mime_guess::from_path(name)
                        .first_or_octet_stream()
                        .to_string(),
                ),
                (header::CACHE_CONTROL, "no-cache".into()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
            ],
            *bytes,
        )
            .into_response();
    }
    if name == "index.html" && ASSETS.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "UI not built. Run npm run build, or use npm run dev for development.",
        )
            .into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::process::Command;

    struct Launcher(Option<libc::pid_t>);
    impl Drop for Launcher {
        fn drop(&mut self) {
            if let Some(pid) = self.0 {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }

    async fn started(path: &std::path::Path) -> Launcher {
        Launcher(Some(
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(text) = std::fs::read_to_string(path)
                        && let Ok(pid) = text.parse()
                    {
                        break pid;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("browser launcher fixture did not start"),
        ))
    }

    #[tokio::test]
    async fn timeout_cancel_and_dropped_launches_kill_and_reap_the_browser_helper() {
        for stop in ["timeout", "cancel", "abort"] {
            let dir = tempfile::tempdir().unwrap();
            let capture = dir.path().join("pid");
            let timeout = Duration::from_secs(if stop == "timeout" { 1 } else { 60 });
            let cancel = CancellationToken::new();
            let _cancel_on_drop = cancel.clone().drop_guard();
            let job_cancel = cancel.clone();
            let path = capture.clone();
            let task = tokio::spawn(async move {
                let mut command = Command::new("/bin/sh");
                command
                    .args([
                        "-c",
                        "printf '%s' \"$$\" > \"$1\"; exec /bin/sleep 60",
                        "mnemoarc-browser-test",
                    ])
                    .arg(path);
                run_browser_command(&mut command, timeout, &job_cancel).await
            });
            let mut launcher = started(&capture).await;
            if stop == "abort" {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                if stop == "cancel" {
                    cancel.cancel();
                }
                let error = task.await.unwrap().unwrap_err();
                assert_eq!(
                    error.to_string(),
                    if stop == "cancel" {
                        "cancelled"
                    } else {
                        "Default browser command timed out"
                    }
                );
            }
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if unsafe { libc::kill(launcher.0.unwrap(), 0) } != 0 {
                        assert_eq!(
                            std::io::Error::last_os_error().raw_os_error(),
                            Some(libc::ESRCH)
                        );
                        launcher.0 = None;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("a stopped launch retained its helper process or a zombie");
        }
    }

    #[tokio::test]
    async fn browser_command_preserves_success_and_failure_status() {
        for code in [0, 7] {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", &format!("exit {code}")]);
            let result = run_browser_command(
                &mut command,
                Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .await;
            if code == 0 {
                result.unwrap();
            } else {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    "Default browser command failed"
                );
            }
        }
    }
}
