#[cfg(unix)]
#[test]
fn non_utf8_environment_does_not_panic_during_config_load() {
    use std::os::unix::ffi::OsStringExt;

    let dir = tempfile::tempdir().unwrap();
    for (key, expected) in [
        ("REVIEW_UNRELATED", "Configure model and model_context"),
        ("MNEMOARC_MODEL", "MNEMOARC_MODEL must contain valid UTF-8"),
    ] {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_mnemoarc"))
            .current_dir(dir.path())
            .env_clear()
            .env(key, std::ffi::OsString::from_vec(vec![0xff]))
            .args(["--config", "missing.toml", "check"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(!stderr.contains("panicked"), "{key}: {stderr}");
        assert!(stderr.contains(expected), "{key}: {stderr}");
    }
}
