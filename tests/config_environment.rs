mod support;
#[cfg(unix)]
#[test]
fn non_utf8_environment_does_not_panic_during_config_load() {
    use std::os::unix::ffi::OsStringExt;

    let dir = tempfile::tempdir().unwrap();
    for (key, expected) in [
        ("REVIEW_UNRELATED", "Configure model and model_context"),
        ("MNEMOARC_MODEL", "MNEMOARC_MODEL must contain valid UTF-8"),
    ] {
        // The built-in model would send check to the provider; an empty
        // model keeps the run offline at the configuration check.
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_mnemoarc"))
            .current_dir(dir.path())
            .env_clear()
            .env("MNEMOARC_MODEL", "")
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
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "old content").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    support::compact_config().save(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[cfg(unix)]
#[test]
fn configuration_and_credentials_reject_fifos_without_waiting_for_eof() {
    use mnemoarc::config::Config;
    use std::{collections::BTreeMap, os::unix::fs::OpenOptionsExt, time::Duration};

    for credentials in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fifo = if credentials {
            path.with_extension("credentials.json")
        } else {
            path.clone()
        };
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let writer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo)
            .unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            sender
                .send(
                    Config::load(&worker_path, &BTreeMap::new())
                        .err()
                        .map(|e| e.to_string()),
                )
                .unwrap();
        });
        let timely = receiver.recv_timeout(Duration::from_millis(500));
        drop(writer);
        let returned = timely.is_ok();
        let error =
            timely.unwrap_or_else(|_| receiver.recv_timeout(Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
        assert!(returned, "configuration load waited for FIFO EOF");
        assert!(error.unwrap().starts_with("unsupported_file_type:"));
        if !credentials {
            assert!(
                support::compact_config()
                    .save(&path)
                    .unwrap_err()
                    .to_string()
                    .starts_with("unsupported_file_type:")
            );
            assert!(!std::fs::metadata(&path).unwrap().is_file());
        }
    }
}

#[cfg(unix)]
#[test]
fn optional_dotenv_fifo_does_not_block_startup() {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    let dir = tempfile::tempdir().unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(dir.path().join(".env"))
            .status()
            .unwrap()
            .success()
    );
    // An empty model keeps check offline (see the test above).
    let mut child = Command::new(env!("CARGO_BIN_EXE_mnemoarc"))
        .current_dir(dir.path())
        .env_clear()
        .env("MNEMOARC_MODEL", "")
        .args(["--config", "missing.toml", "check"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let timed_out = child.try_wait().unwrap().is_none();
    if timed_out {
        child.kill().unwrap();
    }
    let result = child.wait_with_output().unwrap();
    assert!(
        !timed_out,
        "startup waited for an optional .env pipe writer"
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(
        stderr.contains("Configure model and model_context"),
        "{stderr}"
    );
}

#[test]
fn legacy_projects_with_identical_definitions_receive_distinct_persisted_ids() {
    use mnemoarc::config::Config;
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[[projects]]\nname = 'Shared project'\nroot = '.'\n\n[[projects]]\nname = 'Shared project'\nroot = '.'\n",
    )
    .unwrap();
    let mut config = Config::load(&path, &BTreeMap::new()).unwrap();
    let ids: Vec<_> = config.projects.iter().map(|p| p.id.clone()).collect();
    assert!(ids.iter().all(|id| uuid::Uuid::parse_str(id).is_ok()));
    assert_ne!(ids[0], ids[1]);

    config.projects[0].name = "Renamed project".into();
    config.projects[0].output = "renamed.md".into();
    config.projects.swap(0, 1);
    config.save(&path).unwrap();
    let reloaded = Config::load(&path, &BTreeMap::new()).unwrap();
    assert_eq!(reloaded.projects[0].id, ids[1]);
    assert_eq!(reloaded.projects[1].id, ids[0]);
    assert_eq!(reloaded.projects[1].name, "Renamed project");
}

#[test]
fn duplicate_project_ids_are_rejected_without_overwriting_config() {
    use mnemoarc::config::{Config, Project};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let project = Project::default();
    let config = Config {
        projects: vec![
            project.clone(),
            Project {
                name: "Another project".into(),
                ..project
            },
        ],
        ..support::compact_config()
    };
    std::fs::write(&path, "original config").unwrap();
    assert!(config.validate().unwrap_err().to_string().contains("중복"));
    assert!(config.save(&path).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "original config");
}

#[test]
fn a_missing_config_file_loads_the_live_tested_settings() {
    use mnemoarc::config::Config;

    let dir = tempfile::tempdir().unwrap();
    let config = Config::load(&dir.path().join("missing.toml"), &Default::default()).unwrap();
    assert_eq!(config.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(config.model, "stealth/space-bunny-alpha");
    assert_eq!(config.model_context, Some(230000));
    assert_eq!(
        (config.context_tokens, config.output_tokens),
        (160000, 16000)
    );
    assert_eq!(config.reasoning_effort.as_deref(), Some("low"));
    assert_eq!(
        (config.memory_body_bytes, config.memory_bytes),
        (32768, 33554432)
    );
    assert_eq!((config.high_water, config.low_water), (0.9, 0.4));
    assert_eq!(
        (config.request_timeout_secs, config.run_timeout_secs),
        (600, 3600)
    );
    assert_eq!((config.run_tokens, config.review_limit), (10_000_000, 20));
    assert!(config.source_document_review && config.completion_review_enabled);
    config.runnable().unwrap();
}

#[test]
fn cleared_optional_settings_survive_save_and_load() {
    use mnemoarc::config::Config;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    // Both have a built-in value, so an omitted key would bring it back.
    let config = Config {
        model_context: None,
        reasoning_effort: None,
        ..Config::default()
    };
    config.save(&path).unwrap();
    let loaded = Config::load(&path, &Default::default()).unwrap();
    assert_eq!(loaded.model_context, None);
    assert_eq!(loaded.reasoning_effort, None);
    // Set values are saved as they are.
    Config::default().save(&path).unwrap();
    let loaded = Config::load(&path, &Default::default()).unwrap();
    assert_eq!(loaded.model_context, Some(230000));
    assert_eq!(loaded.reasoning_effort.as_deref(), Some("low"));
}
