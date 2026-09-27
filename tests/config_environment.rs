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

#[cfg(unix)]
#[test]
fn saving_existing_config_preserves_its_file_mode() {
    use mnemoarc::config::Config;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "old content").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    Config::default().save(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
}
