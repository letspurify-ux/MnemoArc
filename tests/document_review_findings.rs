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

fn chunked_fallback_fixture() -> (tempfile::TempDir, Session) {
    let (dir, s) = fixture();
    let mut lines: Vec<_> = (1..=130).map(|i| format!("// context line {i}")).collect();
    lines[45] = "const answerText = data.answer || '';".into();
    lines[46] = "const errorText = data.error || '';".into();
    lines[51] = "answer = answerText || errorText || 'fallback';".into();
    lines[99] = "UNRELATED_CHUNK_SENTINEL".into();
    std::fs::write(dir.path().join("ui.js"), lines.join("\n")).unwrap();
    std::fs::write(
        &s.project.output,
        "# 오류 안내\n빈 답변이면 기본 실패 문구가 표시됩니다. ui.js:1-130\n",
    )
    .unwrap();
    (dir, s)
}

fn fallback_proposal(start_line: usize, end_line: usize) -> Value {
    let mut issue = proposal("서버 오류 문구의 표시 우선순위가 누락됐습니다.");
    issue["document"] = json!({"start_line":2,"end_line":2,
        "quote":"빈 답변이면 기본 실패 문구가 표시됩니다."});
    issue["sources"] = json!([{"path":"ui.js","start_line":start_line,"end_line":end_line,
        "quote":"const answerText = data.answer || '';\nconst errorText = data.error || '';"}]);
    issue
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

fn reject(s: &mut Session, issues: Vec<Value>) {
    // The agent schedules a pending review before calling these direct APIs.
    s.document_review.pending = true;
    let error = review::finish(s, &json!({"issues":issues}).to_string())
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("document_review_invalid:"));
    s.last_error = Some(error);
}

// Live run 2026-10-03: calls 20 and 21 mixed a valid finding with a bad
// document quote, in opposite orders. Both valid findings were discarded;
// call 22's empty retry then silently advanced the page.
#[test]
fn a_bad_sibling_cannot_erase_a_grounded_candidate_in_either_order() {
    for valid_first in [false, true] {
        for invalid_kind in 0..4 {
            let (_dir, mut s) = fixture();
            let first = payload(review::request(&mut s).unwrap());
            let good = proposal("Grounded timing candidate");
            let mut bad = proposal("Invalid sibling must never reach validation");
            match invalid_kind {
                0 => bad["document"]["quote"] = json!("Absent document quote"),
                1 => bad["sources"][0]["quote"] = json!("Absent source quote"),
                2 => bad["ui_labels"] = json!(["Invented UI label"]),
                _ => {
                    bad.as_object_mut().unwrap().remove("correction");
                }
            }
            reject(
                &mut s,
                if valid_first {
                    vec![good, bad]
                } else {
                    vec![bad, good]
                },
            );
            assert_eq!(s.document_review.attempts, 0);
            assert!(s.document_review.findings.is_empty());
            assert!(s.document_review.issues.is_empty());
            assert!(!s.document_review.validating);
            assert!(!review::approved(&s));

            let request = review::request(&mut s).unwrap();
            assert!(mnemoarc::context::count(&request, &s.config.model) <= 24_000);
            let retry = payload(request);
            assert_eq!(retry["document"], first["document"]);
            assert_eq!(retry["evidence"], first["evidence"]);
            assert_eq!(retry["evidence_manifest"], first["evidence_manifest"]);
            assert_eq!(retry["current_findings"], json!([]));
            assert_eq!(retry["retry_findings"].as_array().unwrap().len(), 1);
            assert_eq!(retry["retry_findings"][0]["id"], "F1");
            assert_eq!(
                retry["retry_findings"][0]["text"],
                "Grounded timing candidate"
            );

            submit(&mut s, vec![]);
            assert!(s.document_review.validating);
            assert_eq!(s.document_review.attempts, 0);
            assert!(s.document_review.issues.is_empty());
            let verify = payload(review::request(&mut s).unwrap());
            assert_eq!(verify["review_stage"], "validate_findings");
            assert_eq!(verify["candidates"].as_array().unwrap().len(), 1);
            assert_eq!(
                verify["candidates"][0]["problem"],
                "Grounded timing candidate"
            );
            review::finish(
                &mut s,
                &json!({"decisions":[decision("F1", "confirmed")]}).to_string(),
            )
            .unwrap();
            assert_eq!(s.document_review.findings.len(), 1);
            assert_eq!(s.document_review.attempts, 1);
            assert!(!review::approved(&s));
        }
    }
}

#[test]
fn repeated_invalid_retries_keep_stable_ids_and_deduplicate_saved_candidates() {
    let (_dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    let first = proposal("First grounded candidate");
    let mut bad = proposal("Bad sibling");
    bad["document"]["quote"] = json!("Absent document quote");
    reject(&mut s, vec![first.clone(), bad.clone()]);
    review::request(&mut s).unwrap();
    reject(
        &mut s,
        vec![bad.clone(), proposal("Second grounded candidate")],
    );
    review::request(&mut s).unwrap();
    let mut repeated = first;
    repeated["previous_id"] = json!("F1");
    reject(&mut s, vec![repeated, bad]);
    // Even a completely unparseable retry cannot clear earlier candidates.
    assert!(review::finish(&mut s, "not JSON").is_err());
    let retry = payload(review::request(&mut s).unwrap());
    let ids: Vec<_> = retry["retry_findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["F1", "F2"]);
    submit(&mut s, vec![]);
    let verify = payload(review::request(&mut s).unwrap());
    assert_eq!(verify["candidates"].as_array().unwrap().len(), 2);
    review::finish(
        &mut s,
        &json!({"decisions":[decision("F1", "confirmed"), decision("F2", "dismissed")]})
            .to_string(),
    )
    .unwrap();
    assert_eq!(s.document_review.findings.len(), 1);
    assert_eq!(s.document_review.findings[0].id, "F1");
    assert_eq!(s.document_review.dismissed_findings, 1);
    assert!(!review::approved(&s));
}

#[test]
fn a_saved_candidate_keeps_original_source_chunks_for_semantic_validation() {
    let (_dir, mut s) = chunked_fallback_fixture();
    review::request(&mut s).unwrap();
    let good = fallback_proposal(46, 80);
    let mut bad = good.clone();
    bad["document"]["quote"] = json!("Absent document quote");
    reject(&mut s, vec![good, bad]);
    review::request(&mut s).unwrap();
    submit(&mut s, vec![]);
    let verify = payload(review::request(&mut s).unwrap());
    let source = &verify["candidates"][0]["observed_context"]["sources"][0];
    assert!(
        !source["context"]
            .as_str()
            .unwrap()
            .contains("answer = answerText")
    );
    assert!(
        source["review_evidence"]
            .to_string()
            .contains("answer = answerText || errorText")
    );
    review::finish(
        &mut s,
        &json!({"decisions":[decision("F1", "dismissed")]}).to_string(),
    )
    .unwrap();
    assert!(review::approved(&s));
}

#[test]
fn skipping_a_bad_page_still_validates_its_grounded_candidates() {
    for paginated in [false, true] {
        for status in ["confirmed", "dismissed"] {
            let (_dir, mut s) = fixture();
            if paginated {
                let mut doc = std::fs::read_to_string(&s.project.output).unwrap();
                doc.push_str(&"Additional manual context. ui.js:2\n".repeat(220));
                std::fs::write(&s.project.output, doc).unwrap();
            }
            let first = payload(review::request(&mut s).unwrap());
            let mut bad = proposal("Bad sibling");
            bad["document"]["quote"] = json!("Absent document quote");
            reject(
                &mut s,
                vec![proposal("Candidate from the skipped page"), bad],
            );
            assert_eq!(
                review::skip_failing_page(&mut s),
                review::PageSkip::Continued
            );
            for _ in 0..4 {
                if s.document_review.validating {
                    break;
                }
                let next = payload(review::request(&mut s).unwrap());
                assert_eq!(next["retry_findings"], json!([]));
                assert_eq!(next["current_findings"].as_array().unwrap().len(), 1);
                submit(&mut s, vec![]);
            }
            assert!(s.document_review.validating);
            let verify = payload(review::request(&mut s).unwrap());
            assert_eq!(
                verify["candidates"][0]["problem"],
                "Candidate from the skipped page"
            );
            review::finish(
                &mut s,
                &json!({"decisions":[decision("F1", status)]}).to_string(),
            )
            .unwrap();
            assert!(!review::approved(&s));
            if status == "confirmed" {
                assert_eq!(s.document_review.findings.len(), 1);
                assert!(matches!(
                    review::current_verdict(&s),
                    review::CurrentVerdict::Rejected(_)
                ));
            } else {
                assert!(s.document_review.findings.is_empty());
                assert_eq!(
                    review::current_verdict(&s),
                    review::CurrentVerdict::Unavailable
                );
                assert_eq!(
                    s.document_review.unavailable_ranges,
                    vec![(1, first["document_line_end"].as_u64().unwrap() as usize)]
                );
            }
        }
    }
}

#[test]
fn stale_inputs_discard_saved_candidates_instead_of_validating_or_skipping_them() {
    for (changed, skip_stale) in ["document", "source", "requirements", "audience"]
        .into_iter()
        .flat_map(|changed| [false, true].map(|skip_stale| (changed, skip_stale)))
    {
        let (dir, mut s) = fixture();
        review::request(&mut s).unwrap();
        let mut bad = proposal("Bad sibling");
        bad["document"]["quote"] = json!("Absent document quote");
        reject(&mut s, vec![proposal("Candidate from old inputs"), bad]);
        match changed {
            "document" => {
                let doc = std::fs::read_to_string(&s.project.output).unwrap();
                std::fs::write(
                    &s.project.output,
                    format!("{doc}New instruction. ui.js:2\n"),
                )
                .unwrap();
            }
            "source" => {
                std::fs::write(dir.path().join("ui.js"), "function save() { keepKey(); }\n")
                    .unwrap();
            }
            "requirements" => s.answer_review_question = "Write a different manual".into(),
            _ => s.project.audience = "개발자".into(),
        }
        assert!(
            review::finish(&mut s, r#"{"issues":[]}"#)
                .unwrap_err()
                .to_string()
                .starts_with("document_review_stale:")
        );
        if skip_stale {
            assert_eq!(
                review::skip_failing_page(&mut s),
                review::PageSkip::NotApplicable
            );
        }
        let restarted = payload(review::request(&mut s).unwrap());
        assert_eq!(restarted["document_line_start"], 1);
        assert_eq!(restarted["current_findings"], json!([]));
        assert_eq!(restarted["retry_findings"], json!([]));
        submit(&mut s, vec![]);
        assert!(!s.document_review.validating);
        assert!(review::approved(&s));
    }
}

#[test]
fn saved_candidate_feedback_is_bounded_without_changing_page_selection() {
    let (_dir, mut s) = fixture();
    let mut doc = std::fs::read_to_string(&s.project.output).unwrap();
    doc.push_str(&"Additional manual context. ui.js:2\n".repeat(120));
    std::fs::write(&s.project.output, doc).unwrap();
    let first = payload(review::request(&mut s).unwrap());
    let mut bad = proposal("Bad sibling");
    bad["document"]["quote"] = json!("Absent document quote");
    for index in 0..16 {
        let problem = format!(
            "Candidate {index}: {}",
            "Detailed source comparison. ".repeat(40)
        );
        reject(&mut s, vec![proposal(&problem), bad.clone()]);
        let request = review::request(&mut s).unwrap();
        assert!(mnemoarc::context::count(&request, &s.config.model) <= 24_000);
        let retry = payload(request);
        assert_eq!(
            retry["retry_findings"].as_array().unwrap().len(),
            (index + 1).min(12)
        );
        assert_eq!(retry["document"], first["document"]);
        assert_eq!(retry["evidence"], first["evidence"]);
        assert_eq!(retry["evidence_manifest"], first["evidence_manifest"]);
    }
    submit(&mut s, vec![]);
    let next = payload(review::request(&mut s).unwrap());
    assert_eq!(next["retry_findings"], json!([]));
    assert_eq!(next["current_findings"].as_array().unwrap().len(), 12);
    submit(&mut s, vec![]);
    let request = review::request(&mut s).unwrap();
    assert!(mnemoarc::context::count(&request, &s.config.model) <= 24_000);
    let verify = payload(request);
    assert_eq!(verify["candidates"].as_array().unwrap().len(), 12);
}

#[test]
fn recovery_preserves_candidates_already_collected_from_other_pages() {
    let (_dir, mut s) = fixture();
    let mut doc = std::fs::read_to_string(&s.project.output).unwrap();
    for line in 4..=223 {
        doc.push_str(&format!("Later manual claim {line}. ui.js:2\n"));
    }
    std::fs::write(&s.project.output, &doc).unwrap();
    review::request(&mut s).unwrap();
    submit(&mut s, vec![proposal("Finding from a valid earlier page")]);
    let second = payload(review::request(&mut s).unwrap());
    let line = second["document_line_start"].as_u64().unwrap() as usize;
    let mut recovered = proposal("Finding from a rejected later page");
    recovered["document"] = json!({"start_line":line,"end_line":line,
        "quote":doc.lines().nth(line - 1).unwrap()});
    let mut bad = recovered.clone();
    bad["document"]["quote"] = json!("Absent document quote");
    reject(&mut s, vec![bad, recovered]);
    let retry = payload(review::request(&mut s).unwrap());
    assert_eq!(retry["current_findings"].as_array().unwrap().len(), 1);
    assert_eq!(retry["current_findings"][0]["id"], "F1");
    assert_eq!(retry["retry_findings"].as_array().unwrap().len(), 1);
    assert_eq!(retry["retry_findings"][0]["id"], "F2");
    submit(&mut s, vec![]);
    review::request(&mut s).unwrap();
    submit(&mut s, vec![]);
    let verify = payload(review::request(&mut s).unwrap());
    assert_eq!(verify["candidates"].as_array().unwrap().len(), 2);
    review::finish(
        &mut s,
        &json!({"decisions":[decision("F1", "confirmed"), decision("F2", "dismissed")]})
            .to_string(),
    )
    .unwrap();
    assert_eq!(s.document_review.findings.len(), 1);
    assert_eq!(s.document_review.findings[0].id, "F1");
    assert!(!review::approved(&s));
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
fn a_label_missing_from_the_quotes_names_the_issue_and_label() {
    // A live run skipped a review page twice: the bare "label is not present"
    // error never said which label, so the reviewer kept repeating it.
    let (_dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    let mut item = proposal("Incorrect deletion timing");
    item["ui_labels"] = json!(["서버와 통신하지 못했습니다."]);
    let error = review::finish(
        &mut s,
        &json!({"issues":[proposal("Other defect"), item]}).to_string(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("issues[1].ui_labels[0]"), "{error}");
    assert!(error.contains("\"서버와 통신하지 못했습니다.\""), "{error}");
    assert!(error.contains("remove it from ui_labels"), "{error}");
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

// Live run 2026-09-30: the document invented a "입력 중" label; the reviewer's
// correction quoted it for removal with ui_labels [] (it cannot be listed,
// being absent from the source), and the validator dismissed the finding as
// omitting a proposed label. Both instructions now exclude removed strings.
#[test]
fn a_label_quoted_for_removal_is_not_a_proposed_ui_label() {
    let (_dir, mut s) = fixture();
    let review_request = review::request(&mut s).unwrap();
    let instruction = review_request["messages"][0]["content"].as_str().unwrap();
    assert!(instruction.contains("proposes to show or add"));
    assert!(instruction.contains("quotes only to remove or replace"));
    let mut invented = proposal("소스에 없는 \"삭제 완료\" 문구가 표시된다고 설명합니다.");
    invented["correction"] = json!("\"삭제 완료\" 문구가 표시된다는 설명을 제거하세요.");
    submit(&mut s, vec![invented]);
    assert!(s.document_review.validating);
    let verify = review::request(&mut s).unwrap();
    let instruction = verify["messages"][0]["content"].as_str().unwrap();
    assert!(instruction.contains("proposes to show or add"));
    assert!(instruction.contains("its absence never invalidates the finding"));
    assert_eq!(payload(verify)["candidates"][0]["ui_labels"], json!([]));
    review::finish(
        &mut s,
        &json!({"decisions":[decision("F1", "confirmed")]}).to_string(),
    )
    .unwrap();
    assert_eq!(s.document_review.findings.len(), 1);
    assert!(!review::approved(&s));
}

#[test]
fn validator_confirms_removing_exposed_internals_for_end_users() {
    // A live run dismissed 7 of 11 findings asking to remove CSS classes, API
    // routes and storage keys from an end-user manual as "implementation
    // detail" demands, although the reviewer is told to report them.
    let (_dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    submit(&mut s, vec![proposal("문서가 내부 식별자를 노출합니다.")]);
    let verify = review::request(&mut s).unwrap();
    let instruction = verify["messages"][0]["content"].as_str().unwrap();
    assert!(instruction.contains("or audience mismatch"));
    assert!(instruction.contains("internal detail the document itself exposes"));
    assert!(instruction.contains("asks for less implementation detail, not more"));
    assert!(instruction.contains("never an audience mismatch"));
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

fn duplicate_of(id: &str, target: &str) -> Value {
    let mut d = decision(id, "duplicate");
    d["reason"] = json!("Same defect as the target finding.");
    d["duplicate_of"] = json!(target);
    d
}

fn at(problem: &str, line: usize, quote: &str) -> Value {
    let mut issue = proposal(problem);
    issue["document"] = json!({"start_line":line,"end_line":line,"quote":quote});
    issue
}

// Live run 2026-10-03 (MnemoArc): a reused ID split into F4 at another
// passage; the validator then called F4 a duplicate of F2 three times, every
// response was rejected and the whole review was discarded, losing the
// confirmed F2 and F3.
#[test]
fn a_duplicate_at_another_passage_is_confirmed_for_its_own_repair() {
    let cases = [
        // Another document line needs its own repair.
        (
            at("Same defect elsewhere", 3, "검색 결과가 없습니다."),
            true,
        ),
        // Grounding widens a document quote to its whole line, so a shorter
        // quote on the target's line is the same quoted text and merges.
        (
            at(
                "Same defect in the next clause",
                2,
                "등록한 키가 삭제됩니다.",
            ),
            false,
        ),
        (
            at("Same defect, shorter quote", 2, "저장 시 등록한 키가"),
            false,
        ),
    ];
    for (other, separate) in cases {
        let (_dir, mut s) = fixture();
        review::request(&mut s).unwrap();
        let first = at(
            "Timing issue",
            2,
            "키 지우기를 누르면 저장 시 등록한 키가 삭제됩니다.",
        );
        submit(&mut s, vec![first, other]);
        validate(
            &mut s,
            vec![decision("F1", "confirmed"), duplicate_of("F2", "F1")],
        );
        assert!(!s.document_review.validating);
        let ids: Vec<_> = s
            .document_review
            .findings
            .iter()
            .map(|f| f.id.as_str())
            .collect();
        if separate {
            assert_eq!(ids, ["F1", "F2"]);
            assert_eq!(s.document_review.merged_findings, 0);
            assert_eq!(
                s.document_review.validation_log[1]["applied_status"],
                "confirmed"
            );
            assert_eq!(
                s.document_review.validation_log[1]["decision"]["status"],
                "duplicate"
            );
        } else {
            assert_eq!(ids, ["F1"]);
            assert_eq!(s.document_review.merged_findings, 1);
        }
        assert_eq!(s.document_review.dismissed_findings, 0);
        assert!(!review::approved(&s));
    }
}

#[test]
fn a_duplicate_of_another_kind_or_an_unconfirmed_target_is_still_rejected() {
    for target_status in ["confirmed", "dismissed"] {
        for kind in ["citation", "factual"] {
            if target_status == "confirmed" && kind == "factual" {
                continue;
            }
            let (_dir, mut s) = fixture();
            review::request(&mut s).unwrap();
            let mut other = at("Other defect", 3, "검색 결과가 없습니다.");
            other["kind"] = json!(kind);
            submit(&mut s, vec![proposal("Timing issue"), other]);
            review::request(&mut s).unwrap();
            let body =
                json!({"decisions":[decision("F1", target_status), duplicate_of("F2", "F1")]});
            let error = review::finish(&mut s, &body.to_string())
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("confirmed finding of the same kind"),
                "{error}"
            );
            assert!(s.document_review.validating);
            assert_eq!(s.document_review.validation_rounds, 0);
        }
    }
}

// Live run 2026-10-03 (llm_agent): F3 (line 99) and F5 (line 356) had nearly
// the same issue text; the writer, seeing only "problem; correction", fixed
// F5 and left F3 for three review attempts.
#[test]
fn repair_issues_name_the_reviewed_lines_and_quote() {
    let (_dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    let same = "입력 상한 설명이 다릅니다.";
    submit(
        &mut s,
        vec![
            at(
                same,
                2,
                "키 지우기를 누르면 저장 시 등록한 키가 삭제됩니다.",
            ),
            at(same, 3, "검색 결과가 없습니다."),
        ],
    );
    validate(
        &mut s,
        vec![decision("F1", "confirmed"), decision("F2", "confirmed")],
    );
    let issues = review::guidance(&s)["issues"].clone();
    assert_eq!(
        issues[0],
        format!(
            "[reviewed lines 2-2: \"키 지우기를 누르면 저장 시 등록한 키가 삭제됩니다. ui.js:2\"] {same}; 저장할 때 삭제된다고 설명하세요."
        )
    );
    assert!(
        issues[1]
            .as_str()
            .unwrap()
            .starts_with("[reviewed lines 3-3: \"검색 결과가 없습니다. ui.js:1\"] ")
    );
    assert!(review::guidance(&s).get("findings").is_none());
    // Completion gaps and the stored verdict keep the plain issue text.
    assert!(!s.document_review.issues[0].starts_with('['));
}

// Findings without a document quote keep the original same-target rule.
#[test]
fn quoteless_requirement_duplicates_still_merge_into_their_target() {
    let (_dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    let missing = |problem: &str| {
        let mut issue = proposal(problem);
        issue["kind"] = json!("requirement");
        issue["document"] = Value::Null;
        issue["sources"] = json!([]);
        issue["requirement_id"] = json!("R0");
        issue
    };
    submit(
        &mut s,
        vec![
            missing("Missing settings section"),
            missing("No settings guide"),
        ],
    );
    validate(
        &mut s,
        vec![decision("F1", "confirmed"), duplicate_of("F2", "F1")],
    );
    assert_eq!(s.document_review.findings.len(), 1);
    assert_eq!(s.document_review.merged_findings, 1);
}

// A duplicate confirmed only through its target was never judged at its own
// passage, so a later attempt must not reuse that confirmation unvalidated.
#[test]
fn an_inferred_confirmation_is_revalidated_in_a_later_attempt() {
    let (_dir, mut s) = fixture();
    review::request(&mut s).unwrap();
    let other = at("Same defect elsewhere", 3, "검색 결과가 없습니다.");
    submit(&mut s, vec![proposal("Timing issue"), other.clone()]);
    validate(
        &mut s,
        vec![decision("F1", "confirmed"), duplicate_of("F2", "F1")],
    );
    assert_eq!(s.document_review.findings.len(), 2);
    review::request(&mut s).unwrap();
    let mut first = proposal("Timing issue");
    first["previous_id"] = json!("F1");
    let mut again = other;
    again["previous_id"] = json!("F2");
    submit(&mut s, vec![first, again]);
    assert!(s.document_review.validating);
    let p = payload(review::request(&mut s).unwrap());
    let ids: Vec<_> = p["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].clone())
        .collect();
    assert_eq!(ids, [json!("F2")]);
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
    assert_eq!(p["candidates"][0]["problem"], "Timing issue");
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
fn validation_preserves_original_source_evidence_for_short_quotes() {
    // Live F5: grounding the two-line quote removed the error-priority branch
    // from validation, although the original reviewer had seen that branch.
    for end_line in [2, 7] {
        let (dir, mut s) = fixture();
        let source = "const answerText = data.answer || '';\nconst errorText = data.error || '';\n// Empty strings must be filtered.\n// A missing answer is different from a transport failure.\n// Prefer the server's error message.\n// Use a fallback only when both strings are empty.\nanswer = answerText || errorText || (res.ok ? '답변을 만들지 못했습니다.' : answer);\n";
        std::fs::write(dir.path().join("ui.js"), source).unwrap();
        let claim = "서버가 200으로 응답했지만 answer가 비어 있으면 기본 실패 문구가 표시됩니다.";
        std::fs::write(
            &s.project.output,
            format!("# 오류 안내\n{claim} ui.js:1-7\n"),
        )
        .unwrap();
        let original = payload(review::request(&mut s).unwrap());
        let original_evidence = &original["evidence"][0]["numbered_text"];
        assert!(original_evidence.as_str().unwrap().contains("7|answer ="));
        let mut issue = proposal("서버 오류 문구가 우선 표시되는 조건이 누락됐습니다.");
        issue["document"] = json!({"start_line":2,"end_line":2,"quote":claim});
        issue["sources"] = json!([{"path":"ui.js","start_line":1,"end_line":end_line,
            "quote":"const answerText = data.answer || '';\nconst errorText = data.error || '';"}]);
        submit(&mut s, vec![issue]);
        let request = review::request(&mut s).unwrap();
        assert!(mnemoarc::context::count(&request, &s.config.model) <= 24000);
        let validation = payload(request);
        let candidate = &validation["candidates"][0];
        assert!(
            candidate["observed_context"]["sources"]
                .to_string()
                .contains("answer = answerText || errorText || (res.ok ?")
        );
        assert_eq!(candidate["sources"][0]["start_line"], 1);
        assert_eq!(candidate["sources"][0]["end_line"], 2);
        assert_eq!(
            candidate["observed_context"]["sources"][0]["review_evidence"],
            json!([original_evidence])
        );
        review::finish(
            &mut s,
            &json!({"decisions":[decision("F1", "confirmed")]}).to_string(),
        )
        .unwrap();
        assert!(!review::approved(&s));
        assert_eq!(s.document_review.findings.len(), 1);
    }
}

#[test]
fn validation_preserves_related_chunks_without_using_distant_line_hints() {
    for (start_line, end_line, expected_chunks) in [(46, 52, 2), (100, 100, 1)] {
        let (_dir, mut s) = chunked_fallback_fixture();
        let original = payload(review::request(&mut s).unwrap());
        assert!(original["evidence"].as_array().unwrap().len() >= 3);
        submit(&mut s, vec![fallback_proposal(start_line, end_line)]);
        let validation = payload(review::request(&mut s).unwrap());
        let evidence =
            validation["candidates"][0]["observed_context"]["sources"][0]["review_evidence"]
                .as_array()
                .unwrap();
        assert_eq!(evidence.len(), expected_chunks);
        let evidence_text = serde_json::to_string(evidence).unwrap();
        assert!(!evidence_text.contains("UNRELATED_CHUNK_SENTINEL"));
        if expected_chunks == 2 {
            assert!(evidence_text.contains("52|answer ="));
        }
        for chunk in evidence {
            assert!(
                original["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| { &v["numbered_text"] == chunk })
            );
        }
    }
}

#[test]
fn new_original_evidence_for_the_same_quote_requires_revalidation() {
    let (_dir, mut s) = chunked_fallback_fixture();
    review::request(&mut s).unwrap();
    submit(&mut s, vec![fallback_proposal(46, 47)]);
    validate(&mut s, vec![decision("F1", "confirmed")]);

    review::request(&mut s).unwrap();
    let mut first = fallback_proposal(46, 47);
    first["previous_id"] = json!("F1");
    let mut expanded = fallback_proposal(46, 52);
    expanded["previous_id"] = json!("F1");
    submit(&mut s, vec![first, expanded]);
    assert!(s.document_review.validating);
    let validation = payload(review::request(&mut s).unwrap());
    assert_eq!(validation["candidates"][0]["id"], "F1");
    assert!(
        validation["candidates"][0]["observed_context"]["sources"]
            .to_string()
            .contains("52|answer =")
    );
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

const OLD_SCOPE_TEXT: &str =
    "답변이 길거나 특수한 글자가 섞이면 렌더가 실패할 수 있습니다. 그럴 때 말풍선 자체가";
const NEW_SCOPE_TEXT: &str =
    "답변이 길거나 특수한 글자가 섞여 있어 화면에 제대로 표시되지 못할 때가 있습니다. 그럴 때는";

fn scope_fixture() -> (tempfile::TempDir, Session) {
    let (dir, s) = fixture();
    std::fs::write(
        &s.project.output,
        format!(
            "# UI 매뉴얼\n\n## 답변 보기\n\n답변을 선택합니다. ui.js:1\n\n{OLD_SCOPE_TEXT}\n말풍선에서 원문을 확인합니다. ui.js:2\n\n다음 답변을 확인합니다. ui.js:1\n\n## 설정\n\n설정을 저장합니다. ui.js:2\n"
        ),
    )
    .unwrap();
    (dir, s)
}

fn scope_proposal(s: &Session, quote: &str, previous_id: Option<&str>) -> Value {
    let doc = std::fs::read_to_string(&s.project.output).unwrap();
    let start = doc
        .lines()
        .position(|line| line == quote.lines().next().unwrap())
        .unwrap()
        + 1;
    json!({"previous_id":previous_id,"kind":"scope",
        "document":{"start_line":start,"end_line":start + quote.lines().count() - 1,"quote":quote},
        "requirement_id":"R0","sources":[],"problem":"문서가 내부 렌더 용어를 노출합니다.",
        "correction":"사용자가 보는 현상으로 설명하세요.","ui_labels":[]})
}

fn confirm_scope(s: &mut Session) {
    review::request(s).unwrap();
    let issue = scope_proposal(s, OLD_SCOPE_TEXT, None);
    submit(s, vec![issue]);
    validate(s, vec![decision("F1", "confirmed")]);
}

fn rewrite_scope(s: &Session) {
    let doc = std::fs::read_to_string(&s.project.output).unwrap();
    std::fs::write(
        &s.project.output,
        doc.replace(OLD_SCOPE_TEXT, NEW_SCOPE_TEXT),
    )
    .unwrap();
}

// Live call 48 reused F34 after its source-less scope passage was rewritten.
// Keep the ID, but let semantic validation dismiss the now-stale criticism.
#[test]
fn rewritten_source_less_scope_is_revalidated_and_can_be_dismissed() {
    let (_dir, mut s) = scope_fixture();
    confirm_scope(&mut s);
    rewrite_scope(&s);
    // Absolute line numbers may shift without changing the paragraph's identity.
    let doc = std::fs::read_to_string(&s.project.output).unwrap();
    std::fs::write(&s.project.output, format!("\n\n{doc}")).unwrap();
    review::request(&mut s).unwrap();
    let quote = format!("{NEW_SCOPE_TEXT}\n말풍선에서 원문을 확인합니다. ui.js:2");
    let changed = scope_proposal(&s, &quote, Some("F1"));
    submit(&mut s, vec![changed]);
    assert!(
        s.document_review.validating,
        "never reuse the old confirmation"
    );
    let request = payload(review::request(&mut s).unwrap());
    assert_eq!(request["candidates"][0]["id"], "F1");
    assert_eq!(
        request["candidates"][0]["previous_scope"]["document"]["quote"],
        OLD_SCOPE_TEXT
    );
    assert_eq!(
        request["candidates"][0]["previous_scope"]["problem"],
        "문서가 내부 렌더 용어를 노출합니다."
    );
    assert!(
        request["candidates"][0]["document"]["quote"]
            .as_str()
            .unwrap()
            .starts_with(NEW_SCOPE_TEXT)
    );
    review::finish(
        &mut s,
        &json!({"decisions":[decision("F1", "dismissed")]}).to_string(),
    )
    .unwrap();
    assert!(review::approved(&s));
    assert_eq!(s.document_review.resolved_findings, 1);
}

#[test]
fn rewritten_source_less_scope_keeps_its_id_without_fake_progress() {
    let (_dir, mut s) = scope_fixture();
    confirm_scope(&mut s);
    rewrite_scope(&s);
    review::request(&mut s).unwrap();
    let changed = scope_proposal(&s, NEW_SCOPE_TEXT, Some("F1"));
    submit(&mut s, vec![changed]);
    assert!(s.document_review.validating);
    validate(&mut s, vec![decision("F1", "confirmed")]);
    assert_eq!(s.document_review.findings[0].id, "F1");
    assert_eq!(s.document_review.resolved_findings, 0);
    assert_eq!(s.document_review.stalled_attempts, 1);
    assert_eq!(s.document_review.validation_rounds, 2);
    assert!(!review::approved(&s));
    // Once that fresh confirmation succeeds, unchanged evidence still reuses
    // the normal cache instead of adding another validation call.
    review::request(&mut s).unwrap();
    let unchanged = scope_proposal(&s, NEW_SCOPE_TEXT, Some("F1"));
    submit(&mut s, vec![unchanged]);
    assert!(!s.document_review.validating);
    assert_eq!(s.document_review.validation_rounds, 2);
    assert_eq!(s.document_review.stalled_attempts, 2);
}

#[test]
fn unverified_reanchored_scope_leaves_a_gap_instead_of_approval() {
    let (_dir, mut s) = scope_fixture();
    confirm_scope(&mut s);
    rewrite_scope(&s);
    review::request(&mut s).unwrap();
    let changed = scope_proposal(&s, NEW_SCOPE_TEXT, Some("F1"));
    let line = changed["document"]["start_line"].as_u64().unwrap() as usize;
    submit(&mut s, vec![changed]);
    validate(&mut s, vec![decision("F1", "unverified")]);
    assert!(!review::approved(&s));
    assert!(review::unavailable_on_current(&s));
    assert_eq!(review::unavailable_ranges(&s), &[(line, line)]);
    assert_eq!(s.document_review.resolved_findings, 0);
    assert_eq!(s.document_review.dismissed_findings, 0);
    assert!(
        s.document_review.issues.is_empty(),
        "unknown judgments are not repair instructions"
    );
}

#[test]
fn source_less_scope_cannot_reuse_an_id_at_another_location_or_requirement() {
    for change in [
        "context",
        "paragraph",
        "section",
        "requirement",
        "kind",
        "source",
        "duplicate_heading",
        "ambiguous",
    ] {
        let (_dir, mut s) = scope_fixture();
        if change == "duplicate_heading" {
            let doc = std::fs::read_to_string(&s.project.output).unwrap();
            std::fs::write(&s.project.output, doc.replace("## 설정", "## 답변 보기")).unwrap();
        }
        if change == "ambiguous" {
            // The same neighboring blocks identify two paragraphs: neither is unique.
            let doc = std::fs::read_to_string(&s.project.output).unwrap();
            let repeated = format!(
                "\n공통 안내\n\n{OLD_SCOPE_TEXT}\n\n공통 안내\n\nother text\n\n공통 안내\n\nother paragraph\n\n공통 안내\n"
            );
            std::fs::write(
                &s.project.output,
                doc.replace(
                    &format!("\n{OLD_SCOPE_TEXT}\n말풍선에서 원문을 확인합니다. ui.js:2\n"),
                    &repeated,
                ),
            )
            .unwrap();
        }
        confirm_scope(&mut s);
        rewrite_scope(&s);
        let mut doc = std::fs::read_to_string(&s.project.output).unwrap();
        if change == "context" {
            doc = doc.replace("답변을 선택합니다.", "다른 문단 앞의 문맥입니다.");
        } else if change == "section" {
            doc = doc.replace("## 답변 보기", "## 다른 화면");
        }
        std::fs::write(&s.project.output, doc).unwrap();
        review::request(&mut s).unwrap();
        let quote = if change == "paragraph" {
            "다음 답변을 확인합니다. ui.js:1"
        } else {
            NEW_SCOPE_TEXT
        };
        let mut changed = scope_proposal(&s, quote, Some("F1"));
        if change == "requirement" {
            changed["requirement_id"] = json!("audience");
        } else if change == "kind" {
            changed["kind"] = json!("requirement");
        } else if change == "source" {
            changed["sources"] = json!([{"path":"ui.js","start_line":2,"end_line":2,"quote":"function save() { if (clearKey) deleteKey(); }"}]);
        }
        reject(&mut s, vec![changed]);
        assert_eq!(s.document_review.validation_rounds, 1);
        assert!(!review::approved(&s));
    }
}

#[test]
fn a_new_source_less_defect_on_the_same_paragraph_gets_its_own_id() {
    let (_dir, mut s) = scope_fixture();
    confirm_scope(&mut s);
    rewrite_scope(&s);
    review::request(&mut s).unwrap();
    let same = scope_proposal(&s, NEW_SCOPE_TEXT, Some("F1"));
    let mut new = same.clone();
    new["previous_id"] = Value::Null;
    new["problem"] = json!("별개의 범위 문제입니다.");
    new["correction"] = json!("별개의 범위 문제를 수정하세요.");
    submit(&mut s, vec![same, new]);
    validate(
        &mut s,
        vec![decision("F1", "confirmed"), decision("F2", "confirmed")],
    );
    assert_eq!(s.document_review.findings.len(), 2);
    assert_eq!(s.document_review.findings[0].id, "F1");
    assert_eq!(s.document_review.findings[1].id, "F2");
    assert_eq!(s.document_review.resolved_findings, 0);
}

#[test]
fn a_reanchored_scope_gap_survives_an_invalid_sibling_and_empty_retry() {
    let (_dir, mut s) = scope_fixture();
    confirm_scope(&mut s);
    rewrite_scope(&s);
    review::request(&mut s).unwrap();
    let changed = scope_proposal(&s, NEW_SCOPE_TEXT, Some("F1"));
    let line = changed["document"]["start_line"].as_u64().unwrap() as usize;
    let mut bad = changed.clone();
    bad["previous_id"] = Value::Null;
    bad["document"]["quote"] = json!("문서에 없는 인용문");
    reject(&mut s, vec![changed, bad]);
    let retry = payload(review::request(&mut s).unwrap());
    assert_eq!(retry["retry_findings"][0]["id"], "F1");
    submit(&mut s, vec![]);
    validate(&mut s, vec![decision("F1", "unverified")]);
    assert!(review::unavailable_on_current(&s));
    assert_eq!(review::unavailable_ranges(&s), &[(line, line)]);
    assert_eq!(s.document_review.resolved_findings, 0);
    assert!(!review::approved(&s));
}

#[test]
fn unchanged_scope_cannot_be_replaced_by_another_claim_in_the_same_paragraph() {
    for multiline in [false, true] {
        let (_dir, mut s) = scope_fixture();
        let quote = if multiline {
            format!("{OLD_SCOPE_TEXT}\n말풍선에서 원문을 확인합니다. ui.js:2")
        } else {
            OLD_SCOPE_TEXT.to_owned()
        };
        review::request(&mut s).unwrap();
        let original = scope_proposal(&s, &quote, None);
        submit(&mut s, vec![original]);
        validate(&mut s, vec![decision("F1", "confirmed")]);
        let mut other = scope_proposal(&s, "말풍선에서 원문을 확인합니다. ui.js:2", Some("F1"));
        other["problem"] = json!("원문 확인 안내를 더 자세히 써야 합니다.");
        other["correction"] = json!("원문 확인 안내를 확대하세요.");
        if multiline {
            // Line endings and indentation do not remove the old claim.
            let doc = std::fs::read_to_string(&s.project.output).unwrap();
            std::fs::write(
                &s.project.output,
                doc.replace(&quote, &quote.replace('\n', "\n  "))
                    .replace('\n', "\r\n"),
            )
            .unwrap();
        }
        review::request(&mut s).unwrap();
        reject(&mut s, vec![other]);
        assert_eq!(s.document_review.validation_rounds, 1);
        assert_eq!(
            s.document_review.findings[0].proposal.problem,
            "문서가 내부 렌더 용어를 노출합니다."
        );
        assert!(!review::approved(&s));
    }
}

#[test]
fn reanchored_scope_protection_survives_cached_and_later_reviews() {
    for cached_id in [None, Some("F1")] {
        let (_dir, mut s) = scope_fixture();
        confirm_scope(&mut s);
        rewrite_scope(&s);
        review::request(&mut s).unwrap();
        let changed = scope_proposal(&s, NEW_SCOPE_TEXT, Some("F1"));
        submit(&mut s, vec![changed]);
        validate(&mut s, vec![decision("F1", "confirmed")]);

        review::request(&mut s).unwrap();
        let unchanged = scope_proposal(&s, NEW_SCOPE_TEXT, cached_id);
        submit(&mut s, vec![unchanged]);
        assert!(!s.document_review.validating);
        assert_eq!(s.document_review.validation_rounds, 2);

        let doc = std::fs::read_to_string(&s.project.output).unwrap();
        std::fs::write(
            &s.project.output,
            doc.replace(
                "말풍선에서 원문을 확인합니다.",
                "같은 말풍선에서 원문을 볼 수 있습니다.",
            ),
        )
        .unwrap();
        review::request(&mut s).unwrap();
        let unchanged = scope_proposal(&s, NEW_SCOPE_TEXT, Some("F1"));
        submit(&mut s, vec![unchanged]);
        assert!(s.document_review.validating);
        validate(&mut s, vec![decision("F1", "unverified")]);
        assert!(review::unavailable_on_current(&s));
        assert!(!review::unavailable_ranges(&s).is_empty());
        assert_eq!(s.document_review.resolved_findings, 0);
        assert_eq!(s.document_review.best_issue_count, Some(1));
        assert_eq!(s.document_review.stalled_attempts, 3);
    }
}

#[test]
fn mixed_scope_gap_is_reported_without_counting_uncertainty_as_progress() {
    let (_dir, mut s) = scope_fixture();
    review::request(&mut s).unwrap();
    let first = scope_proposal(&s, OLD_SCOPE_TEXT, None);
    let mut second = scope_proposal(&s, "설정을 저장합니다. ui.js:2", None);
    second["problem"] = json!("설정 안내에 별도 문제가 있습니다.");
    submit(&mut s, vec![first, second.clone()]);
    validate(
        &mut s,
        vec![decision("F1", "confirmed"), decision("F2", "confirmed")],
    );
    rewrite_scope(&s);
    review::request(&mut s).unwrap();
    let changed = scope_proposal(&s, NEW_SCOPE_TEXT, Some("F1"));
    let line = changed["document"]["start_line"].as_u64().unwrap() as usize;
    second["previous_id"] = json!("F2");
    submit(&mut s, vec![changed, second]);
    validate(&mut s, vec![decision("F1", "unverified")]);
    assert!(review::rejected_on_current_result(&s));
    assert_eq!(s.document_review.findings.len(), 1);
    assert_eq!(review::unavailable_ranges(&s), &[(line, line)]);
    assert_eq!(
        review::guidance(&s)["unavailable_ranges"],
        json!([[line, line]])
    );
    assert_eq!(s.document_review.resolved_findings, 0);
    assert_eq!(s.document_review.best_issue_count, Some(2));
    assert_eq!(s.document_review.stalled_attempts, 1);

    // The range belongs to that reviewed version, not subsequent edits.
    let doc = std::fs::read_to_string(&s.project.output).unwrap();
    std::fs::write(&s.project.output, format!("\n{doc}")).unwrap();
    assert!(review::unavailable_ranges(&s).is_empty());
    assert!(review::guidance(&s).get("unavailable_ranges").is_none());
    review::request(&mut s).unwrap();
    submit(&mut s, vec![]);
    assert!(review::approved(&s));
    assert!(s.document_review.unavailable_ranges.is_empty());
}

#[test]
fn an_explicit_retry_updates_the_proposal_but_preserves_observed_evidence() {
    for invalid_retry in [false, true] {
        let (_dir, mut s) = fixture();
        review::request(&mut s).unwrap();
        let mut initial = proposal("Initial timing description");
        initial["sources"].as_array_mut().unwrap().push(json!({"path":"ui.js","start_line":1,"end_line":1,"quote":"const message = '검색 결과가 없습니다.';"}));
        initial["correction"] = json!("‘검색 결과가 없습니다.’ 안내를 확인하세요.");
        initial["ui_labels"] = json!(["검색 결과가 없습니다."]);
        let mut bad = initial.clone();
        bad["document"]["quote"] = json!("Absent document quote");
        reject(&mut s, vec![initial, bad.clone()]);
        review::request(&mut s).unwrap();
        let mut corrected = proposal("Corrected timing description");
        corrected["previous_id"] = json!("F1");
        if invalid_retry {
            reject(&mut s, vec![corrected.clone(), bad]);
            assert!(!s.document_review.validating);
            assert!(s.document_review.findings.is_empty());
            review::request(&mut s).unwrap();
            submit(&mut s, vec![]);
        } else {
            submit(&mut s, vec![corrected.clone()]);
        }
        let request = payload(review::request(&mut s).unwrap());
        let candidate = &request["candidates"][0];
        assert_eq!(candidate["id"], "F1");
        assert_eq!(candidate["problem"], corrected["problem"]);
        assert_eq!(candidate["correction"], corrected["correction"]);
        assert_eq!(candidate["ui_labels"], json!([]));
        assert_eq!(candidate["sources"].as_array().unwrap().len(), 2);
        assert_eq!(
            candidate["observed_context"]["sources"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        review::finish(
            &mut s,
            &json!({"decisions":[decision("F1", "confirmed")]}).to_string(),
        )
        .unwrap();
        assert_eq!(
            s.document_review.findings[0].proposal.problem,
            "Corrected timing description"
        );
        assert!(!review::approved(&s));
    }
}

#[test]
fn a_rejected_response_error_is_not_resent_after_a_valid_response() {
    // The error belongs to the retry of the rejected response only; a live
    // run sent it to later pages and to validation.
    let (_dir, mut s) = fixture();
    s.last_error = Some("document_review_invalid: quote is absent on this supplied page".into());
    let retry = payload(review::request(&mut s).unwrap());
    assert!(
        retry["previous_response_error"]
            .as_str()
            .unwrap()
            .contains("quote is absent")
    );
    submit(
        &mut s,
        vec![proposal("문서가 반복 횟수를 잘못 설명합니다.")],
    );
    assert!(s.last_error.is_none());
    assert!(s.document_review.validating);
    let verify = payload(review::request(&mut s).unwrap());
    assert_eq!(verify["previous_response_error"], Value::Null);
    // Other errors are not review feedback and stay in place.
    s.last_error = Some("document_review: unrelated repair note".into());
    validate(&mut s, vec![decision("F1", "confirmed")]);
    assert!(
        s.last_error
            .as_deref()
            .unwrap()
            .starts_with("document_review:")
    );
}
