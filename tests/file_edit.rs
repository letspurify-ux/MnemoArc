use mnemoarc::{
    config::{Config, Project},
    session::Session,
    tools,
};
use serde_json::{Value, json};

fn setup() -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("summary.md"),
            ..Default::default()
        },
        Config::default(),
    );
    (dir, session)
}

fn run(session: &mut Session, name: &str, args: Value) -> Value {
    tools::execute(session, name, args).unwrap()
}

#[test]
fn exact_edit_requires_one_match_and_current_hash() {
    let (dir, mut session) = setup();
    let path = dir.path().join("notes.md");
    std::fs::write(&path, "one\none\n").unwrap();
    let digest = tools::hash(b"one\none\n");
    assert!(
        tools::execute(
            &mut session,
            "file_edit",
            json!({"path":"notes.md","expected_hash":digest,"old_text":"one","new_text":"two"})
        )
        .is_err()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\none\n");
    let edited = run(
        &mut session,
        "file_edit",
        json!({"path":"notes.md","expected_hash":digest,"old_text":"one","new_text":"two","replace_all":true}),
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "two\ntwo\n");
    assert_eq!(edited["files"][0]["hash"], tools::hash(b"two\ntwo\n"));
    assert!(tools::execute(&mut session, "file_edit", json!({"path":"notes.md","expected_hash":digest,"old_text":"two","new_text":"three","replace_all":true})).is_err());
    std::fs::write(&path, "aaa").unwrap();
    assert!(tools::execute(&mut session, "file_edit", json!({"path":"notes.md","expected_hash":tools::hash(b"aaa"),"old_text":"aa","new_text":"b"})).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "aaa");
}

#[test]
fn file_write_creates_empty_file_and_replaces_existing_content() {
    let (dir, mut session) = setup();
    run(
        &mut session,
        "file_write",
        json!({"path":"nested/empty.txt","content":""}),
    );
    let path = dir.path().join("nested/empty.txt");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    run(
        &mut session,
        "file_write",
        json!({"path":"nested/empty.txt","content":"hello","expected_hash":tools::hash(b"")}),
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    assert!(
        tools::execute(
            &mut session,
            "file_write",
            json!({"path":"nested/empty.txt","content":"bad"})
        )
        .is_err()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
}

#[test]
fn patch_supports_ordered_updates_move_and_delete() {
    let (dir, mut session) = setup();
    std::fs::write(dir.path().join("old.txt"), "first\n").unwrap();
    std::fs::write(dir.path().join("remove.txt"), "bye").unwrap();
    let result = run(
        &mut session,
        "file_patch",
        json!({"operations":[
            {"action":"update","path":"old.txt","expected_hash":tools::hash(b"first\n"),"old_text":"first","new_text":"second"},
            {"action":"move","path":"old.txt","to_path":"new.txt"},
            {"action":"add","path":"added.txt","content":"added"},
            {"action":"delete","path":"remove.txt","expected_hash":tools::hash(b"bye")}
        ]}),
    );
    assert_eq!(result["operation_count"], 4);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
        "second\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("added.txt")).unwrap(),
        "added"
    );
    assert!(!dir.path().join("old.txt").exists());
    assert!(!dir.path().join("remove.txt").exists());
}

#[test]
fn invalid_later_patch_operation_does_not_save_earlier_ones() {
    let (dir, mut session) = setup();
    std::fs::write(dir.path().join("keep.txt"), "same").unwrap();
    let result = tools::execute(
        &mut session,
        "file_patch",
        json!({"operations":[
            {"action":"add","path":"new.txt","content":"new"},
            {"action":"delete","path":"keep.txt","expected_hash":"stale"}
        ]}),
    );
    assert!(result.is_err());
    assert!(!dir.path().join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("keep.txt")).unwrap(),
        "same"
    );
}

#[test]
fn patch_reuses_first_hash_for_later_edits_but_requires_one_initially() {
    let (dir, mut session) = setup();
    std::fs::write(dir.path().join("part.md"), "one two").unwrap();
    let missing_hash = tools::execute(
        &mut session,
        "file_patch",
        json!({"operations":[
            {"action":"update","path":"part.md","old_text":"one","new_text":"three"}
        ]}),
    )
    .unwrap_err();
    let recovery = tools::envelope(Err(missing_hash));
    assert_eq!(recovery["recovery"]["code"], "file_hash_required");
    assert_eq!(recovery["recovery"]["action"], "copy_file_hash");
    run(
        &mut session,
        "file_patch",
        json!({"operations":[
            {"action":"update","path":"part.md","expected_hash":tools::hash(b"one two"),"old_text":"one","new_text":"three"},
            {"action":"update","path":"part.md","old_text":"two","new_text":"four"}
        ]}),
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("part.md")).unwrap(),
        "three four"
    );
}

#[test]
fn generic_edits_reject_output_escape_and_symlinks() {
    let (dir, mut session) = setup();
    assert_eq!(session.project.output, dir.path().join("summary.md"));
    assert_eq!(
        tools::output_path(&session.project).unwrap(),
        dir.path().join("summary.md")
    );
    for path in [
        "../outside.txt",
        "/tmp/outside.txt",
        ".git/config",
        "summary.md",
    ] {
        assert!(
            tools::execute(
                &mut session,
                "file_write",
                json!({"path":path,"content":"bad"})
            )
            .is_err(),
            "{path}"
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.path().join("summary.md"), dir.path().join("alias.md"))
            .unwrap();
        assert!(
            tools::execute(
                &mut session,
                "file_write",
                json!({"path":"alias.md","content":"bad"})
            )
            .is_err()
        );
    }
}

#[test]
fn configured_output_cannot_be_created_through_a_case_alias() {
    let (dir, mut session) = setup();
    let mixed_case = dir.path().join("SUMMARY.md");
    let configured = dir.path().join("summary.md");
    if !{
        std::fs::write(&mixed_case, "probe").unwrap();
        let aliases = configured.exists();
        std::fs::remove_file(&mixed_case).unwrap();
        aliases
    } {
        return;
    }
    let result = tools::execute(
        &mut session,
        "file_write",
        json!({"path":"SUMMARY.md","content":"bypass"}),
    );
    assert!(result.is_err());
    assert!(!configured.exists());
}

#[test]
fn case_alias_cannot_bypass_excluded_existing_file() {
    let (dir, mut session) = setup();
    let path = dir.path().join("secret.txt");
    std::fs::write(&path, "private").unwrap();
    if !dir.path().join("SECRET.txt").exists() {
        return;
    }
    session.project.exclude = vec!["secret.txt".into()];
    let result = tools::execute(
        &mut session,
        "file_edit",
        json!({
            "path":"SECRET.txt","expected_hash":tools::hash(b"private"),"old_text":"private","new_text":"exposed"
        }),
    );
    assert!(result.is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "private");
}

#[test]
fn case_alias_cannot_create_an_excluded_file() {
    let (dir, mut session) = setup();
    session.project.exclude = vec!["secret.txt".into()];
    let result = tools::execute(
        &mut session,
        "file_write",
        json!({"path":"SECRET.txt","content":"private"}),
    );
    assert!(result.is_err());
    assert!(!dir.path().join("secret.txt").exists());
    let result = tools::execute(
        &mut session,
        "file_write",
        json!({"path":".GIT/config","content":"bad"}),
    );
    assert!(result.is_err());
}

#[test]
fn unicode_normalization_aliases_cannot_bypass_output_or_exclusions() {
    use std::path::PathBuf;
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("café.md"),
            exclude: vec!["résumé.txt".into()],
            ..Default::default()
        },
        Config::default(),
    );
    let output_alias = "cafe\u{301}.md";
    let excluded_alias = "re\u{301}sume\u{301}.txt";
    let probe = PathBuf::from(dir.path()).join(output_alias);
    std::fs::write(&probe, "probe").unwrap();
    let same_file = dir.path().join("café.md").exists();
    std::fs::remove_file(&probe).unwrap();
    if !same_file {
        return;
    }
    assert!(
        tools::execute(
            &mut session,
            "file_write",
            json!({"path":output_alias,"content":"bad"})
        )
        .is_err()
    );
    assert!(
        tools::execute(
            &mut session,
            "file_write",
            json!({"path":excluded_alias,"content":"bad"})
        )
        .is_err()
    );
}

#[test]
fn unicode_casefold_aliases_cannot_bypass_output_or_exclusions() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("straße.md"),
            exclude: vec!["ﬁle.txt".into()],
            ..Default::default()
        },
        Config::default(),
    );
    let output_alias = dir.path().join("STRASSE.md");
    std::fs::write(&output_alias, "probe").unwrap();
    let output_aliases = dir.path().join("straße.md").exists();
    std::fs::remove_file(&output_alias).unwrap();
    if output_aliases {
        assert!(
            tools::execute(
                &mut session,
                "file_write",
                json!({"path":"STRASSE.md","content":"bad"})
            )
            .is_err()
        );
        assert!(!dir.path().join("straße.md").exists());
    }
    assert!(
        tools::execute(
            &mut session,
            "file_write",
            json!({"path":"file.txt","content":"bad"})
        )
        .is_err()
    );
}

#[test]
fn patch_applies_existing_file_case_aliases_in_order() {
    let (dir, mut session) = setup();
    let path = dir.path().join("note.txt");
    std::fs::write(&path, "one two").unwrap();
    if !dir.path().join("NOTE.txt").exists() {
        return;
    }
    run(
        &mut session,
        "file_patch",
        json!({"operations":[
            {"action":"update","path":"note.txt","expected_hash":tools::hash(b"one two"),"old_text":"one","new_text":"three"},
            {"action":"update","path":"NOTE.txt","old_text":"two","new_text":"four"}
        ]}),
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "three four");
}

#[cfg(unix)]
#[test]
fn moving_an_executable_preserves_its_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, mut session) = setup();
    let from = dir.path().join("run.sh");
    std::fs::write(&from, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&from, std::fs::Permissions::from_mode(0o755)).unwrap();
    run(
        &mut session,
        "file_patch",
        json!({"operations":[
            {"action":"move","path":"run.sh","to_path":"scripts/run.sh","expected_hash":tools::hash(b"#!/bin/sh\nexit 0\n")}
        ]}),
    );
    let to = dir.path().join("scripts/run.sh");
    assert_eq!(
        std::fs::metadata(to).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[cfg(unix)]
#[test]
fn replacing_same_content_destination_with_move_keeps_source_mode() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, mut session) = setup();
    let source = dir.path().join("source.sh");
    let destination = dir.path().join("target.sh");
    std::fs::write(&source, "same").unwrap();
    std::fs::write(&destination, "same").unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o644)).unwrap();
    let result = run(
        &mut session,
        "file_patch",
        json!({"operations":[
            {"action":"delete","path":"target.sh","expected_hash":tools::hash(b"same")},
            {"action":"move","path":"source.sh","to_path":"target.sh","expected_hash":tools::hash(b"same")}
        ]}),
    );
    assert_eq!(result["changed_files"], 2);
    assert!(!source.exists());
    assert_eq!(
        std::fs::metadata(destination).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn edit_hash_conflict_keeps_its_recovery_code() {
    let (dir, mut session) = setup();
    std::fs::write(dir.path().join("text.txt"), "current").unwrap();
    let error = tools::execute(
        &mut session,
        "file_edit",
        json!({
            "path":"text.txt","expected_hash":"stale","old_text":"current","new_text":"new"
        }),
    )
    .unwrap_err();
    let response = tools::envelope(Err(error));
    assert_eq!(response["recovery"]["code"], "file_revision_conflict");
    assert_eq!(response["recovery"]["class"], "stale_state");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("text.txt")).unwrap(),
        "current"
    );
}
