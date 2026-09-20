//! Explicit paid-model regressions. Oracles check facts, not prose keywords.
use mnemoarc::{
    agent,
    config::{Config, Project, Secret},
    llm::{OpenAiClient, ToolCall},
    session::Session,
    tools,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

const BRANCHES: &str = "pub fn route(key: Option<&str>, cached: bool) -> &'static str {\n let Some(key) = key else { return \"missing\"; };\n if key.is_empty() { return \"empty\"; }\n if cached { return \"cached\"; }\n \"fetch\"\n}\n";
const SERVICE: &str = "package sample;\npublic class Service {\n public String load(String value) {\n  return Parser.parse(value);\n }\n public String load(int id) {\n  if (id < 0) throw new IllegalArgumentException(\"negative\");\n  return load(String.valueOf(id));\n }\n public static class Inner {\n  public String load(String value) {\n   return \"inner:\" + value;\n  }\n }\n}\n";
const PARSER: &str = "package sample;\npublic final class Parser {\n public static String parse(String value) {\n  if (value == null) return \"\";\n  return value.trim();\n }\n}\n";

fn facts(case: &str) -> Value {
    match case {
        "branches" => {
            json!({"missing_cached":"missing","empty_cached":"empty","valid_cached":"cached","valid_uncached":"fetch","cache_before_validation":false})
        }
        "semantics" => json!({"same_error_instance":true,"always_new_abort_error":false,
            "normal_close_aborts":false,"cancels_underlying_promise":false,
            "signal_passed_before_response":true}),
        "recovery" => json!({"receiver":"res","registration_method":"once",
            "event":"close","normal_close_aborts":false}),
        "java" => json!({"direct_load_overloads":2,"string_delegate":"Parser.parse",
            "zero_accepted":true,"negative_throws":true,"nested_prefix":"inner:","null_parse_result":""}),
        _ => unreachable!(),
    }
}

// Generous regression ceilings, not a claim of optimality. Provider latency is
// reported separately: network variance must not masquerade as navigation cost.
fn check_efficiency(case: &str, calls: &[Value], input_tokens: usize) -> Vec<String> {
    let (max_calls, max_input) = match case {
        "semantics" => (8, 65_000),
        "recovery" => (4, 35_000),
        "java" => (8, 40_000),
        "branches" => (4, 35_000),
        _ => unreachable!(),
    };
    let mut failures = Vec::new();
    if calls.len() > max_calls {
        failures.push(format!(
            "navigation_call_budget:{}>{max_calls}",
            calls.len()
        ));
    }
    if input_tokens > max_input {
        failures.push(format!(
            "navigation_input_budget:{input_tokens}>{max_input}"
        ));
    }
    if calls.iter().any(|call| {
        call["name"] == "file_read"
            && call["args"].get("cursor").is_none()
            && (call["args"].get("start_line").is_none()
                || (call["args"].get("max_lines").is_none() && call["args"].get("limit").is_none()))
    }) {
        failures.push("untargeted_source_read".into());
    }
    failures
}

#[test]
fn efficiency_oracle_rejects_excess_cost_and_untargeted_reads() {
    let read = json!({"name":"file_read","args":{"path":"a.rs","start_line":1,"max_lines":30}});
    assert!(check_efficiency("recovery", std::slice::from_ref(&read), 35_000).is_empty());
    assert_eq!(
        check_efficiency("recovery", std::slice::from_ref(&read), 35_001),
        ["navigation_input_budget:35001>35000"]
    );
    assert_eq!(
        check_efficiency("recovery", &vec![read; 5], 35_000),
        ["navigation_call_budget:5>4"]
    );
    assert_eq!(
        check_efficiency(
            "semantics",
            &[json!({"name":"file_read","args":{"path":"a.rs"}})],
            1
        ),
        ["untargeted_source_read"]
    );
    assert!(
        check_efficiency(
            "semantics",
            &[json!({"name":"file_read","args":{"cursor":"opaque"}})],
            1
        )
        .is_empty()
    );
}

fn check_facts(case: &str, answer: &Value) -> Vec<String> {
    facts(case)
        .as_object()
        .unwrap()
        .iter()
        .filter(|(key, expected)| answer.get(*key) != Some(*expected))
        .map(|(key, _)| format!("incorrect_or_missing:{key}"))
        .collect()
}

fn check_citations(project: &Project, answer: &Value) -> Vec<String> {
    let Some(citations) = answer["citations"].as_array().filter(|c| !c.is_empty()) else {
        return vec!["missing_citations".into()];
    };
    let root = project.root.canonicalize().unwrap();
    citations
        .iter()
        .filter_map(|citation| {
            let valid = (|| {
                let path = Path::new(citation["path"].as_str()?);
                if path.is_absolute() {
                    return None;
                }
                let resolved = root.join(path).canonicalize().ok()?;
                if !resolved.starts_with(&root) {
                    return None;
                }
                let text = std::fs::read_to_string(resolved).ok()?;
                let start = citation["start"].as_u64()?;
                let end = citation["end"].as_u64()?;
                (start > 0 && end >= start && end <= text.lines().count() as u64).then_some(())
            })()
            .is_some();
            (!valid).then(|| "invalid_citation_range_or_path".into())
        })
        .collect()
}

fn observed_line(value: &Value, path: &Path, line: u64) -> bool {
    match value {
        Value::Object(fields) => {
            (value["origin"] == "file"
                && value["path"].as_str().is_some_and(|p| Path::new(p) == path)
                && value["start_line"].as_u64().is_some_and(|n| n <= line)
                && value["end_line"].as_u64().is_some_and(|n| n >= line))
                || fields.values().any(|v| observed_line(v, path, line))
        }
        Value::Array(values) => values.iter().any(|v| observed_line(v, path, line)),
        _ => false,
    }
}

fn check_evidence(case: &str, project: &Project, answer: &Value, results: &[Value]) -> Vec<String> {
    let anchors = match case {
        "branches" => vec![
            ("src/routing.rs", "let Some(key)"),
            ("src/routing.rs", "key.is_empty()"),
            ("src/routing.rs", "if cached"),
            ("src/routing.rs", "\"fetch\""),
        ],
        "semantics" => vec![
            ("backend/src/abort.js", "signal.reason instanceof Error"),
            ("backend/src/abort.js", "error.name = 'AbortError'"),
            ("backend/src/abort.js", "Promise.resolve(promise).then"),
            (
                "backend/src/server.js",
                "if (!res.writableEnded) controller.abort()",
            ),
            ("backend/src/server.js", "signal: controller.signal"),
            ("backend/src/server.js", "if (stream) stream.end"),
        ],
        "recovery" => vec![
            ("backend/src/server.js", "res.once('close', onClose)"),
            (
                "backend/src/server.js",
                "if (!res.writableEnded) controller.abort()",
            ),
        ],
        "java" => vec![
            ("src/sample/Service.java", "return Parser.parse(value)"),
            ("src/sample/Service.java", "if (id < 0)"),
            ("src/sample/Service.java", "return \"inner:\""),
            ("src/sample/Parser.java", "if (value == null)"),
        ],
        _ => unreachable!(),
    };
    let mut failures = Vec::new();
    for (relative, needle) in anchors {
        let path = project.root.join(relative).canonicalize().unwrap();
        let source = std::fs::read_to_string(&path).unwrap();
        let line = source
            .lines()
            .position(|s| s.contains(needle))
            .expect("oracle source anchor changed") as u64
            + 1;
        // A contract comment may be cited, but its implementation must still
        // have been delivered: observation below always checks the code line.
        let alternative = (needle == "Promise.resolve(promise).then")
            .then(|| {
                source
                    .lines()
                    .position(|s| s.contains("promise 자체를 취소하는 함수는 아니다"))
            })
            .flatten()
            .map(|n| n as u64 + 1);
        let cited = answer["citations"].as_array().is_some_and(|citations| {
            citations.iter().any(|c| {
                c["path"] == relative
                    && std::iter::once(line).chain(alternative).any(|line| {
                        c["start"].as_u64().is_some_and(|n| n <= line)
                            && c["end"].as_u64().is_some_and(|n| n >= line)
                    })
            })
        });
        if !cited {
            failures.push(format!("missing_fact_citation:{relative}:{line}"));
        }
        if !results.iter().any(|r| observed_line(r, &path, line)) {
            failures.push(format!("missing_delivered_evidence:{relative}:{line}"));
        }
    }
    failures
}

fn parse_answer(text: &str) -> Value {
    let trimmed = text.trim();
    let trimmed = if trimmed.starts_with("```") {
        trimmed
            .split_once('\n')
            .and_then(|(_, body)| body.split_once("```"))
            .map(|(body, _)| body.trim())
            .unwrap_or(trimmed)
    } else {
        trimmed
    };
    serde_json::from_str(trimmed).unwrap_or(Value::Null)
}

#[test]
fn answer_parser_separates_fenced_json_from_extra_explanation() {
    assert_eq!(
        parse_answer("```json\n{\"ok\":true}\n```\n근거 설명"),
        json!({"ok":true})
    );
    assert_eq!(parse_answer("{\"ok\":true}"), json!({"ok":true}));
    assert!(parse_answer("This is not JSON: {ok: true}").is_null());
    assert!(parse_answer("```json\n{broken}\n```").is_null());
}

fn prompt(case: &str) -> &'static str {
    match case {
        "branches" => {
            "src/routing.rs의 route를 확인해줘. 캐시 분기가 입력 검증보다 먼저인지와 early return 순서를 구분해. 문서 수정 없이 JSON만 답해: missing_cached(route(None,true) 반환 문자열), empty_cached(route(Some(\"\"),true)), valid_cached(route(Some(\"x\"),true)), valid_uncached(route(Some(\"x\"),false)), cache_before_validation(boolean), citations({path,start,end} 배열)."
        }
        "semantics" => {
            "backend/src/server.js, agent.js, abort.js를 읽고 취소 동작을 검증해줘. 특히 기존 오류의 처리, 정상 응답 종료, 원래 promise 취소 여부와 신호 전달 순서를 확인해. 문서 수정 없이 JSON 객체만 답해. 필드: same_error_instance(신호의 reason이 Error일 때 같은 객체를 다시 던지는가), always_new_abort_error(취소는 항상 새 AbortError를 만드는가), normal_close_aborts(정상 응답 종료도 취소하는가), cancels_underlying_promise(abortable 자체가 원래 promise를 취소하는가), signal_passed_before_response(최종 완료 응답 본문을 쓰기 전에 agent로 신호를 전달하는가; 스트림 헤더 전송과 구분). 각 값은 boolean. citations는 근거의 {path,start,end} 배열이며 프로젝트 상대 경로와 실제 줄 번호를 써."
        }
        "recovery" => {
            "backend/src/server.js에서 /api/chat 연결 종료 리스너를 찾아 정상 응답 종료도 취소하는지 확인해줘. 앞선 검색이 비었으면 반환된 안내를 활용해 검색을 복구하고 필요한 코드를 읽어. 파일은 수정하지 마. JSON만 답해: receiver(리스너 등록 대상 식별자), registration_method(등록 메서드 이름), event(이벤트 이름), normal_close_aborts(boolean), citations({path,start,end} 배열, 프로젝트 상대 경로)."
        }
        "java" => {
            "src/sample/Service.java와 Parser.java를 구조 탐색으로 조사해줘. Service에 직접 선언된 load 오버로드와 Inner의 동명 메서드를 구분하고, String 인자 처리의 파일 간 호출, 정수 0과 음수 처리, Parser의 null 처리를 구현에서 확인해. 파일 수정 없이 JSON만 답해: direct_load_overloads(직접 선언된 오버로드 수), string_delegate(호출 대상만 클래스.메서드 형식으로; 화살표·호출부 이름 제외), zero_accepted(boolean), negative_throws(boolean), nested_prefix(Inner.load가 붙이는 문자열), null_parse_result(Parser.parse(null)의 반환 문자열), citations({path,start,end} 배열, 프로젝트 상대 경로)."
        }
        _ => unreachable!(),
    }
}

fn source_files(project: &Project, case: &str) -> BTreeMap<String, String> {
    let paths = if case == "branches" {
        vec!["src/routing.rs"]
    } else if case == "java" {
        vec!["src/sample/Service.java", "src/sample/Parser.java"]
    } else {
        vec![
            "backend/src/agent.js",
            "backend/src/server.js",
            "backend/src/abort.js",
        ]
    };
    paths
        .into_iter()
        .map(|p| {
            (
                p.into(),
                std::fs::read_to_string(project.root.join(p)).unwrap(),
            )
        })
        .collect()
}

#[test]
fn semantic_oracle_rejects_missing_and_inverted_facts() {
    for case in ["semantics", "recovery", "java", "branches"] {
        let good = facts(case);
        assert!(check_facts(case, &good).is_empty());
        assert!(!check_facts(case, &Value::Null).is_empty());
        for key in good.as_object().unwrap().keys() {
            let mut wrong = good.clone();
            wrong[key] = Value::Null;
            assert_eq!(
                check_facts(case, &wrong),
                [format!("incorrect_or_missing:{key}")]
            );
        }
    }
    let mut wrong = facts("semantics");
    wrong["always_new_abort_error"] = json!(true);
    assert_eq!(
        check_facts("semantics", &wrong),
        ["incorrect_or_missing:always_new_abort_error"]
    );
}

#[test]
fn citation_oracle_rejects_invalid_and_external_ranges() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "one\ntwo\n").unwrap();
    let project = Project {
        root: dir.path().into(),
        ..Default::default()
    };
    assert!(
        check_citations(
            &project,
            &json!({"citations":[{"path":"a.rs","start":1,"end":2}]})
        )
        .is_empty()
    );
    for citation in [
        json!({"path":"a.rs","start":0,"end":2}),
        json!({"path":"a.rs","start":1,"end":3}),
        json!({"path":"a.rs","start":2,"end":1}),
        json!({"path":"missing.rs","start":1,"end":1}),
        json!({"path":dir.path().join("a.rs"),"start":1,"end":1}),
    ] {
        assert!(!check_citations(&project, &json!({"citations":[citation]})).is_empty());
    }
    assert!(!check_citations(&project, &json!({"citations":[]})).is_empty());
}

#[test]
fn correct_facts_and_valid_citations_still_require_delivered_implementation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/sample")).unwrap();
    std::fs::write(dir.path().join("src/routing.rs"), BRANCHES).unwrap();
    std::fs::write(dir.path().join("src/sample/Service.java"), SERVICE).unwrap();
    std::fs::write(dir.path().join("src/sample/Parser.java"), PARSER).unwrap();
    let project = Project {
        root: dir.path().into(),
        ..Default::default()
    };
    let mut answer = facts("java");
    answer["citations"] = json!([
        {"path":"src/sample/Service.java","start":1,"end":15},
        {"path":"src/sample/Parser.java","start":1,"end":7}
    ]);
    assert!(check_facts("java", &answer).is_empty());
    assert!(check_citations(&project, &answer).is_empty());
    assert_eq!(check_evidence("java", &project, &answer, &[]).len(), 4);
    let mut s = Session::new(project.clone(), Config::default());
    s.active_tools = ["code_outline", "file_read"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let outline = tools::execute(
        &mut s,
        "code_outline",
        json!({"path":"src/sample/Service.java","view":"compact"}),
    )
    .unwrap();
    assert_eq!(
        check_evidence("java", &project, &answer, &[outline]).len(),
        4
    );
    let reads: Vec<_> = ["src/sample/Service.java", "src/sample/Parser.java"]
        .iter()
        .map(|path| tools::execute(&mut s, "file_read", json!({"path":path})).unwrap())
        .collect();
    assert!(check_evidence("java", &project, &answer, &reads).is_empty());
    answer["citations"][0]["end"] = json!(5);
    assert_eq!(check_evidence("java", &project, &answer, &reads).len(), 2);
}

#[test]
fn contract_citation_does_not_replace_reading_the_implementation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("backend/src")).unwrap();
    std::fs::write(dir.path().join("backend/src/abort.js"), "// promise 자체를 취소하는 함수는 아니다\nif (signal.reason instanceof Error) throw signal.reason;\nerror.name = 'AbortError';\nPromise.resolve(promise).then(resolve, reject);\n").unwrap();
    std::fs::write(dir.path().join("backend/src/server.js"), "if (!res.writableEnded) controller.abort();\nhandleQuestion({signal: controller.signal});\nif (stream) stream.end();\n").unwrap();
    let project = Project {
        root: dir.path().into(),
        ..Default::default()
    };
    let mut s = Session::new(project.clone(), Config::default());
    s.active_tools.insert("file_read".into());
    let answer = json!({"citations":[
        {"path":"backend/src/abort.js","start":1,"end":3},
        {"path":"backend/src/server.js","start":1,"end":3}
    ]});
    let short = tools::execute(
        &mut s,
        "file_read",
        json!({"path":"backend/src/abort.js","max_lines":3}),
    )
    .unwrap();
    let server =
        tools::execute(&mut s, "file_read", json!({"path":"backend/src/server.js"})).unwrap();
    assert_eq!(
        check_evidence("semantics", &project, &answer, &[short, server.clone()]),
        ["missing_delivered_evidence:backend/src/abort.js:4"]
    );
    let full = tools::execute(&mut s, "file_read", json!({"path":"backend/src/abort.js"})).unwrap();
    assert!(check_evidence("semantics", &project, &answer, &[full, server]).is_empty());
}

#[tokio::test]
#[ignore = "paid provider; explicitly set MNEMOARC_LIVE_TEST=1"]
async fn live_navigation_regressions() {
    assert_eq!(std::env::var("MNEMOARC_LIVE_TEST").as_deref(), Ok("1"));
    let path = PathBuf::from(std::env::var("MNEMOARC_LIVE_CONFIG").unwrap_or("config.toml".into()));
    let mut config = Config::load(&path, &BTreeMap::new()).unwrap();
    if path.with_extension("credentials.json").exists() {
        let keys: BTreeMap<String, String> = serde_json::from_slice(
            &std::fs::read(path.with_extension("credentials.json")).unwrap(),
        )
        .unwrap();
        config.api_key = keys.get(&config.api_key_env).cloned().map(Secret);
    }
    if let Ok(mode) = std::env::var("MNEMOARC_NAV_REVIEW") {
        assert!(["on", "off"].contains(&mode.as_str()));
        config.source_answer_review = mode == "on";
    }
    config.run_tokens = 200_000;
    config.run_timeout_secs = 600;
    let registered = config
        .projects
        .iter()
        .find(|p| p.name == "llm_agent")
        .expect("registered llm_agent project")
        .clone();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/sample")).unwrap();
    std::fs::write(dir.path().join("src/routing.rs"), BRANCHES).unwrap();
    std::fs::write(dir.path().join("src/sample/Service.java"), SERVICE).unwrap();
    std::fs::write(dir.path().join("src/sample/Parser.java"), PARSER).unwrap();
    let java = Project {
        name: "java-navigation-fixture".into(),
        root: dir.path().into(),
        output: dir.path().join("unused.md"),
        ..Default::default()
    };
    let repetitions: usize = std::env::var("MNEMOARC_NAV_REPETITIONS")
        .unwrap_or("3".into())
        .parse()
        .unwrap();
    assert!((1..=5).contains(&repetitions));
    let mut reports = Vec::new();
    let cases =
        std::env::var("MNEMOARC_NAV_CASES").unwrap_or("semantics,recovery,java,branches".into());
    for case in cases.split(',') {
        assert!(
            ["semantics", "recovery", "java", "branches"].contains(&case),
            "unknown navigation case"
        );
        for repetition in 1..=repetitions {
            let mut project = if ["java", "branches"].contains(&case) {
                java.clone()
            } else {
                registered.clone()
            };
            project.output = dir.path().join(format!("unused-{case}-{repetition}.md"));
            let before = source_files(&project, case);
            if !["java", "branches"].contains(&case) {
                assert!(
                    before["backend/src/abort.js"]
                        .contains("if (signal.reason instanceof Error) throw signal.reason;")
                );
                assert!(
                    before["backend/src/server.js"]
                        .contains("if (!res.writableEnded) controller.abort()")
                );
                assert!(before["backend/src/server.js"].contains("res.once('close', onClose)"));
            }
            let mut s = Session::new(project.clone(), config.clone());
            s.add_user(prompt(case).into());
            if case == "recovery" {
                let call = ToolCall {
                    id: "seed-empty".into(),
                    name: "source_search".into(),
                    arguments:
                        json!({"path_glob":"backend/src/server.js","query":"close|req.on|res.on"})
                            .to_string(),
                };
                let result = tools::run_call(&mut s, &call);
                assert_eq!(result["data"]["empty_reason"], "no_matching_lines");
                assert!(
                    result["data"]["guidance"]
                        .as_str()
                        .unwrap()
                        .contains("regex:true")
                );
                s.history.push(vec![json!({"role":"assistant","content":"","tool_calls":[{"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}]}),
                    json!({"role":"tool","tool_call_id":call.id,"content":result.to_string()})], true);
            }
            let (tx, mut rx) = tokio::sync::mpsc::channel(128);
            let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
            let started = std::time::Instant::now();
            let s =
                agent::run_session(s, Arc::new(OpenAiClient), CancellationToken::new(), tx).await;
            let elapsed_ms = started.elapsed().as_millis();
            drain.await.unwrap();
            let messages: Vec<_> = s.history.bundles.iter().flat_map(|b| &b.messages).collect();
            let answer_text = messages
                .iter()
                .rev()
                .find(|m| m["role"] == "assistant" && m.get("tool_calls").is_none())
                .and_then(|m| m["content"].as_str())
                .unwrap_or("");
            let answer = parse_answer(answer_text);
            let mut failures = if answer.is_object() {
                let mut failures = check_facts(case, &answer);
                failures.extend(check_citations(&project, &answer));
                failures
            } else {
                vec!["invalid_answer_json".into()]
            };
            let calls: Vec<_> = messages.iter().flat_map(|m| m["tool_calls"].as_array().into_iter().flatten()).filter(|c| c["id"] != "seed-empty").map(|c| json!({"name":c["function"]["name"],"args":serde_json::from_str::<Value>(c["function"]["arguments"].as_str().unwrap()).unwrap_or(Value::Null)})).collect();
            let results: Vec<Value> = messages
                .iter()
                .filter(|m| m["role"] == "tool" && m["tool_call_id"] != "seed-empty")
                .filter_map(|m| serde_json::from_str(m["content"].as_str()?).ok())
                .collect();
            let tool_errors = results.iter().filter(|r| r["status"] != "ok").count();
            if answer.is_object() {
                failures.extend(check_evidence(case, &project, &answer, &results));
            }
            let successful_search = results
                .iter()
                .any(|r| r["data"]["total_matching_lines"].as_u64().unwrap_or(0) > 0);
            failures.extend(check_efficiency(case, &calls, s.input_tokens));
            if s.status != "complete" {
                failures.push(format!("status:{}", s.status));
            }
            if tool_errors > 0 {
                failures.push("tool_errors".into());
            }
            if case == "recovery" && !successful_search {
                failures.push("search_not_recovered".into());
            }
            if case == "java" && !calls.iter().any(|c| c["name"] == "code_outline") {
                failures.push("structure_not_used".into());
            }
            assert_eq!(source_files(&project, case), before, "source changed");
            if project.output.exists() {
                failures.push("unexpected_document_write".into());
            }
            eprintln!(
                "{case}/{repetition}: calls={} failures={failures:?} input={} output={}",
                calls.len(),
                s.input_tokens,
                s.output_tokens
            );
            reports.push(json!({"case":case,"repetition":repetition,"model":config.model,"failures":failures,"calls":calls,"tool_errors":tool_errors,"errors":results.iter().filter(|r|r["status"]!="ok").collect::<Vec<_>>(),"input_tokens":s.input_tokens,"output_tokens":s.output_tokens,"usage_estimated":s.usage_incomplete,"answer":answer,"answer_text":answer_text,"source_unchanged":true,"elapsed_ms":elapsed_ms,"tool_setup":"session_defaults","review_enabled":config.source_answer_review,"review_completed":s.answer_reviewed,"review_input_tokens":s.answer_review_input_tokens,"review_output_tokens":s.answer_review_output_tokens,"session_error":s.last_error,"draft":s.answer_review_original,"draft_fact_failures":s.answer_review_original.as_deref().map(|draft|check_facts(case,&parse_answer(draft)))}));
            if let Ok(path) = std::env::var("MNEMOARC_NAV_REPORT") {
                std::fs::write(path, serde_json::to_vec_pretty(&reports).unwrap()).unwrap();
            }
        }
    }
    assert!(
        reports
            .iter()
            .all(|r| r["failures"].as_array().unwrap().is_empty()),
        "navigation regressions failed; see aggregate report"
    );
}
