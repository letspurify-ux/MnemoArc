use mnemoarc::{
    config::{Config, Project},
    session::Session,
    tools::document_review as review,
};
use serde_json::{Value, json};

fn fixture() -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ui.js"), "const message = '검색 결과가 없습니다.';\nfunction save() { if (clearKey) deleteKey(); }\n").unwrap();
    let output = dir.path().join("manual.md");
    std::fs::write(&output, "# 설정\n키 지우기를 누르면 저장 시 등록한 키가 삭제됩니다. ui.js:2\n검색 결과가 없습니다. ui.js:1\n").unwrap();
    let mut session = Session::new(
        Project {
            root: dir.path().into(),
            output,
            audience: "일반 사용자".into(),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            ..Default::default()
        },
    );
    session.add_user("UI 사용자 매뉴얼 만들어줘".into());
    (dir, session)
}

fn proposal(problem: &str) -> Value {
    json!({"previous_id":null,"kind":"factual",
        "document":{"start_line":2,"end_line":2,"quote":"키 지우기를 누르면 저장 시 등록한 키가 삭제됩니다."},
        "requirement_id":null,"sources":[{"path":"ui.js","start_line":2,"end_line":2,"quote":"function save() { if (clearKey) deleteKey(); }"}],
        "problem":problem,"correction":"저장할 때 삭제된다고 설명하세요.","ui_labels":[]})
}

fn payload(request: Value) -> Value {
    serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap()
}
fn decision(id: &str, status: &str) -> Value {
    json!({"id":id,"status":status,"reason":"The current document already says 저장 시, and deletion is inside save().","duplicate_of":null})
}
fn submit(s: &mut Session, issues: Vec<Value>) {
    review::finish(s, &json!({"issues":issues}).to_string()).unwrap();
}
fn validate(s: &mut Session, decisions: Vec<Value>) {
    review::request(s).unwrap();
    review::finish(s, &json!({"decisions":decisions}).to_string()).unwrap();
}

#[test]
fn a_misreading_is_not_a_repair_instruction_until_validation() {
    let (_dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    submit(
        &mut s,
        vec![proposal("매뉴얼이 즉시 삭제한다고 잘못 설명합니다.")],
    );
    assert!(s.document_review.validating);
    assert_eq!(s.document_review.attempts, 0);
    assert!(s.document_review.issues.is_empty());
    assert!(!review::approved(&s));
    let request = review::request(&mut s).unwrap();
    assert!(mnemoarc::context::count(&request, &s.config.model) <= 24000);
    let p = payload(request);
    assert_eq!(p["review_stage"], "validate_findings");
    assert!(
        p["candidates"][0]["observed_context"]["document_context"]
            .as_str()
            .unwrap()
            .contains("저장 시")
    );
    review::finish(
        &mut s,
        &json!({"decisions":[decision("F1","dismissed")]}).to_string(),
    )
    .unwrap();
    assert!(review::approved(&s));
    assert_eq!(s.document_review.dismissed_findings, 1);
    assert_eq!(s.document_review.validation_rounds, 1);
}

#[test]
fn invalid_quotes_and_invented_ui_labels_never_become_empty_approvals() {
    for case in 0..4 {
        let (_dir, mut s) = fixture();
        review::request(&mut s).unwrap();
        let mut item = proposal("Incorrect deletion timing");
        match case {
            0 => item["document"]["quote"] = json!("즉시 삭제됩니다"),
            1 => item["sources"][0]["quote"] = json!("function clear() { deleteKey(); }"),
            2 => item["ui_labels"] = json!(["검색 결과가 없습니다_MEMORY"]),
            _ => item["sources"][0]["start_line"] = json!(999),
        }
        assert!(
            review::finish(&mut s, &json!({"issues":[item]}).to_string())
                .unwrap_err()
                .to_string()
                .starts_with("document_review_invalid:")
        );
        assert!(!review::approved(&s));
        assert_eq!(s.document_review.attempts, 0);
        assert!(s.document_review.findings.is_empty());
    }
}

#[test]
fn missing_or_unknown_validation_cannot_approve_or_partially_commit() {
    for decisions in [
        vec![],
        vec![decision("F9", "dismissed")],
        vec![json!({"id":"F1","status":"unverified","reason":" ","duplicate_of":null})],
    ] {
        let (_dir, mut s) = fixture();
        review::request(&mut s).unwrap();
        submit(&mut s, vec![proposal("Timing issue")]);
        review::request(&mut s).unwrap();
        assert!(review::finish(&mut s, &json!({"decisions":decisions}).to_string()).is_err());
        assert!(!review::approved(&s));
        assert!(s.document_review.validating);
        assert!(s.document_review.validation_log.is_empty());
        assert_eq!(s.document_review.attempts, 0);
    }
}

// Live run 2026-09-30: the validator confirmed two findings and returned
// unverified for a third three times; rejecting the whole response abandoned
// the review and discarded the confirmed findings.
#[test]
fn unverified_decision_drops_only_that_finding_and_keeps_confirmed_ones() {
    let (_dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    let mut other = proposal("검색 결과 문구가 다릅니다.");
    other["document"] = json!({"start_line":3,"end_line":3,"quote":"검색 결과가 없습니다."});
    other["sources"] = json!([{"path":"ui.js","start_line":1,"end_line":1,"quote":"const message = '검색 결과가 없습니다.';"}]);
    submit(&mut s, vec![proposal("Timing issue"), other]);
    validate(
        &mut s,
        vec![decision("F1", "confirmed"), decision("F2", "unverified")],
    );
    assert!(!s.document_review.validating);
    assert_eq!(s.document_review.findings.len(), 1);
    assert_eq!(s.document_review.findings[0].id, "F1");
    assert_eq!(s.document_review.dismissed_findings, 1);
    assert_eq!(
        s.document_review.validation_log[1]["decision"]["status"],
        "unverified"
    );
    assert!(!review::approved(&s));

    // An unverified-only result is not a confirmed defect, so it cannot block approval.
    let (_dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    submit(&mut s, vec![proposal("Timing issue")]);
    validate(&mut s, vec![decision("F1", "unverified")]);
    assert!(review::approved(&s));
}

#[test]
fn same_id_reworded_findings_merge_without_resetting_stall_count() {
    let (_dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    submit(&mut s, vec![proposal("Timing issue")]);
    validate(&mut s, vec![decision("F1", "confirmed")]);
    review::request(&mut s).unwrap();
    let mut first = proposal("Timing issue is still present");
    first["previous_id"] = json!("F1");
    let mut duplicate = first.clone();
    duplicate["problem"] = json!("The same deletion timing remains wrong");
    submit(&mut s, vec![first, duplicate]);
    validate(&mut s, vec![decision("F1", "confirmed")]);
    assert_eq!(s.document_review.findings.len(), 1);
    assert_eq!(s.document_review.findings[0].id, "F1");
    assert_eq!(s.document_review.merged_findings, 1);
    assert_eq!(s.document_review.stalled_attempts, 1);
}

#[test]
fn different_defects_on_the_same_passage_keep_distinct_ids() {
    let (_dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    submit(
        &mut s,
        vec![
            proposal("Timing issue"),
            proposal("Wrong scope of removed credentials"),
        ],
    );
    validate(
        &mut s,
        vec![decision("F1", "confirmed"), decision("F2", "confirmed")],
    );
    assert_eq!(s.document_review.findings.len(), 2);
    review::request(&mut s).unwrap();
    let mut remaining = proposal("Wrong scope of removed credentials");
    remaining["previous_id"] = json!("F2");
    submit(&mut s, vec![remaining]);
    assert!(
        !s.document_review.validating,
        "unchanged accepted finding reuses its validation"
    );
    assert_eq!(s.document_review.findings[0].id, "F2");
    assert_eq!(s.document_review.validation_rounds, 1);
}

#[test]
fn semantic_duplicates_merge_only_into_a_confirmed_target() {
    let (_dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    submit(
        &mut s,
        vec![
            proposal("Timing issue"),
            proposal("Delete timing is incorrect"),
        ],
    );
    let mut duplicate = decision("F2", "duplicate");
    duplicate["duplicate_of"] = json!("F1");
    validate(&mut s, vec![duplicate, decision("F1", "confirmed")]);
    assert_eq!(s.document_review.findings.len(), 1);
    assert_eq!(s.document_review.merged_findings, 1);
}

#[test]
fn merging_repeated_findings_preserves_their_additional_evidence() {
    let (_dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    submit(&mut s, vec![proposal("Timing issue")]);
    validate(&mut s, vec![decision("F1", "confirmed")]);
    review::request(&mut s).unwrap();
    let mut first = proposal("Timing issue");
    first["previous_id"] = json!("F1");
    let mut repeated = first.clone();
    repeated["problem"] = json!("Repeated timing issue with more context");
    repeated["sources"] = json!([{"path":"ui.js","start_line":1,"end_line":1,"quote":"const message = '검색 결과가 없습니다.';"}]);
    submit(&mut s, vec![first, repeated]);
    let p = payload(review::request(&mut s).unwrap());
    assert_eq!(p["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(p["candidates"][0]["sources"].as_array().unwrap().len(), 2);
    assert_eq!(
        p["candidates"][0]["observed_context"]["sources"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    validate(&mut s, vec![decision("F1", "dismissed")]);
    assert!(review::approved(&s));
}

#[test]
fn edits_during_validation_invalidate_the_pending_verdict() {
    let (dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    submit(&mut s, vec![proposal("Timing issue")]);
    review::request(&mut s).unwrap();
    std::fs::write(dir.path().join("ui.js"), "changed source\n").unwrap();
    assert!(
        review::finish(
            &mut s,
            &json!({"decisions":[decision("F1","dismissed")]}).to_string()
        )
        .unwrap_err()
        .to_string()
        .starts_with("document_review_stale:")
    );
    assert!(!review::approved(&s));
    let p = payload(review::request(&mut s).unwrap());
    assert_ne!(p["review_stage"], "validate_findings");
    assert_eq!(p["current_findings"], json!([]));
}

#[test]
fn a_legacy_string_response_is_a_protocol_error_not_approval() {
    let (_dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    assert!(review::finish(&mut s, r#"{"issues":["A claim without evidence"]}"#).is_err());
    assert!(!review::approved(&s));
}

#[test]
fn indentation_and_off_by_one_hints_are_grounded_to_actual_supplied_lines() {
    let (dir, mut s) = fixture();
    std::fs::write(
        dir.path().join("ui.js"),
        "function save() {\n    if (clearKey) {\n        deleteKey();\n    }\n}\n",
    )
    .unwrap();
    review::request(&mut s).unwrap();
    let mut issue = proposal("Check deletion timing");
    issue["sources"] = json!([{"path":"ui.js","start_line":2,"end_line":3,
        "quote":"if (clearKey) {\n  deleteKey();\n}"}]);
    submit(&mut s, vec![issue]);
    let p = payload(review::request(&mut s).unwrap());
    let actual = &p["candidates"][0]["sources"][0];
    assert_eq!(actual["start_line"], 2);
    assert_eq!(actual["end_line"], 4);
    assert_eq!(
        actual["quote"],
        "    if (clearKey) {\n        deleteKey();\n    }"
    );
    assert_eq!(s.document_review.anchor_corrections, 1);
    validate(&mut s, vec![decision("F1", "dismissed")]);
    assert!(review::approved(&s));
}

#[test]
fn ambiguous_quotes_and_changed_literal_words_are_never_repaired_by_guessing() {
    for quote in ["deleteKey();", "const label = 'Search  results';"] {
        let (dir, mut s) = fixture();
        std::fs::write(
            dir.path().join("ui.js"),
            "deleteKey();\nconst label = 'Search results';\ndeleteKey();\n",
        )
        .unwrap();
        review::request(&mut s).unwrap();
        let mut issue = proposal("Unsupported quote");
        issue["sources"] = json!([{"path":"ui.js","start_line":2,"end_line":2,"quote":quote}]);
        let error = review::finish(&mut s, &json!({"issues":[issue]}).to_string())
            .unwrap_err()
            .to_string();
        assert!(error.contains("issues[0].sources[0]"), "{error}");
        assert!(!review::approved(&s));
        assert_eq!(s.document_review.anchor_corrections, 0);
    }
}

#[test]
fn an_unobserved_source_is_rejected_without_reading_its_contents_into_feedback() {
    let (dir, mut s) = fixture();
    std::fs::write(
        dir.path().join("uncited.js"),
        "UNOBSERVED_SOURCE_SENTINEL\n",
    )
    .unwrap();
    review::request(&mut s).unwrap();
    let mut issue = proposal("Unsupported source");
    issue["sources"] =
        json!([{"path":"uncited.js","start_line":1,"end_line":1,"quote":"guessed content"}]);
    let error = review::finish(&mut s, &json!({"issues":[issue]}).to_string())
        .unwrap_err()
        .to_string();
    assert!(error.contains("source was not supplied"), "{error}");
    assert!(!error.contains("UNOBSERVED_SOURCE_SENTINEL"), "{error}");
    assert!(!review::approved(&s));
    assert!(s.document_review.findings.is_empty());
}

#[test]
fn invalid_line_hints_cannot_expose_source_outside_the_supplied_page() {
    let (dir, mut s) = fixture();
    let mut text = std::fs::read_to_string(dir.path().join("ui.js")).unwrap();
    text.push_str(&"// unrelated line\n".repeat(97));
    text.push_str("OUTSIDE_PAGE_SENTINEL\n");
    std::fs::write(dir.path().join("ui.js"), text).unwrap();
    let request = review::request(&mut s).unwrap();
    assert!(!request.to_string().contains("OUTSIDE_PAGE_SENTINEL"));
    let mut issue = proposal("Unsupported source range");
    issue["sources"] =
        json!([{"path":"ui.js","start_line":100,"end_line":100,"quote":"guessed content"}]);
    let error = review::finish(&mut s, &json!({"issues":[issue]}).to_string())
        .unwrap_err()
        .to_string();
    assert!(error.contains("issues[0].sources[0]"), "{error}");
    assert!(!error.contains("OUTSIDE_PAGE_SENTINEL"), "{error}");
    assert!(!review::approved(&s));
    assert_eq!(s.document_review.anchor_corrections, 0);
}

#[test]
fn missing_requirements_need_a_known_original_requirement() {
    let (_dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    let mut issue = proposal("Missing required settings section");
    issue["kind"] = json!("requirement");
    issue["document"] = Value::Null;
    issue["sources"] = json!([]);
    issue["requirement_id"] = json!("model-invented-check");
    assert!(review::finish(&mut s, &json!({"issues":[issue.clone()]}).to_string()).is_err());
    issue["requirement_id"] = json!("R0");
    submit(&mut s, vec![issue]);
    let p = payload(review::request(&mut s).unwrap());
    assert!(
        p["candidates"][0]["observed_context"]["document_context"]
            .as_str()
            .unwrap()
            .contains("저장 시"),
        "absence checks need document content, not only its outline"
    );
    validate(&mut s, vec![decision("F1", "dismissed")]);
    assert!(review::approved(&s));
}

#[test]
fn rewriting_the_same_unresolved_claim_does_not_fake_progress() {
    for keep_id in [false, true] {
        let (_dir, mut s) = fixture();
        review::request(&mut s).unwrap();
        submit(&mut s, vec![proposal("Incorrect deletion timing")]);
        validate(&mut s, vec![decision("F1", "confirmed")]);
        let document = std::fs::read_to_string(&s.project.output).unwrap().replace(
            "키 지우기를 누르면 저장 시 등록한 키가 삭제됩니다.",
            "키는 저장할 때 삭제된다고 설명한 수정 문장입니다.",
        );
        std::fs::write(&s.project.output, document).unwrap();
        review::request(&mut s).unwrap();
        let mut changed = proposal("The same timing defect remains after rewriting");
        changed["document"]["quote"] = json!("키는 저장할 때 삭제된다고 설명한 수정 문장입니다.");
        if keep_id {
            changed["previous_id"] = json!("F1");
        }
        submit(&mut s, vec![changed]);
        validate(
            &mut s,
            vec![decision(if keep_id { "F1" } else { "F2" }, "confirmed")],
        );
        assert_eq!(s.document_review.stalled_attempts, 1);
        assert_eq!(s.document_review.resolved_findings, 0);
    }
}
