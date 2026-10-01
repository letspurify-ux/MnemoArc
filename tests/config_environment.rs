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
        ..Default::default()
    };
    std::fs::write(&path, "original config").unwrap();
    assert!(config.validate().unwrap_err().to_string().contains("중복"));
    assert!(config.save(&path).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "original config");
}
