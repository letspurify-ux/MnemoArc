//! Standalone browser application packaging and platform integration.
use anyhow::{Context, Result};
use axum::{
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use std::{path::PathBuf, process::Command};

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

pub fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = Command::new("rundll32.exe");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut command = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    anyhow::ensure!(
        command
            .status()
            .context("Cannot launch the default browser")?
            .success(),
        "Default browser command failed"
    );
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
