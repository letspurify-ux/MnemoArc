//! One-line descriptions of tool calls for the progress a running task
//! shows. They are built from each call's own arguments, so they cost no
//! model tokens and say what was actually requested; the model never sees
//! them.
use serde_json::Value;
use std::path::Path;

/// Longest description shown; longer ones end in "…".
const MAX_CHARS: usize = 120;
/// Headings named for one document edit.
const MAX_HEADINGS: usize = 3;

/// The tool's label with what this call reads, searches or changes, such as
/// "파일 읽기 src/ui/menu.rs:100-299". Paths under `root` are shown relative
/// to it.
pub fn call_summary(root: &Path, name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let path = |key: &str| relative(root, args[key].as_str().unwrap_or(""));
    let detail = match name {
        // symbol_read also carries a symbol_id, an opaque hash of about 140
        // characters; its path says where the symbol is.
        "file_read" | "symbol_read" => {
            let target = path("path");
            // A continuation may carry only its cursor.
            if target.is_empty() {
                if args["cursor"].is_string() {
                    "이어 읽기".to_owned()
                } else {
                    String::new()
                }
            } else {
                // Without start_line, symbol_read starts at the symbol's first
                // line, not line 1 of the file.
                let from_symbol = name == "symbol_read" && args["start_line"].is_null();
                let range = if from_symbol {
                    String::new()
                } else {
                    range(&args)
                };
                format!("{target}{range}")
            }
        }
        "file_list" => args["path_glob"]
            .as_str()
            .or(args["pattern"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| path("path")),
        "code_outline" => join([path("path"), quoted(&args["query"])]),
        "source_search" | "symbol_search" | "memory_find" => {
            let query = match &args["queries"] {
                Value::Array(queries) => queries
                    .iter()
                    .map(quoted)
                    .filter(|query| !query.is_empty())
                    .collect::<Vec<_>>()
                    .join(" | "),
                // pattern is a legacy name for query that the runtime accepts.
                _ => match quoted(&args["query"]) {
                    query if query.is_empty() => quoted(&args["pattern"]),
                    query => query,
                },
            };
            let scope = args["path_glob"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| path("path"));
            join([query, scope])
        }
        "symbol_relations" => join([
            args["relation"].as_str().unwrap_or("").to_owned(),
            path("path"),
        ]),
        "document_edit" => edit(&args),
        "document_edit_batch" => {
            let edits: Vec<Value> = match &args["edits"] {
                Value::Array(edits) => edits.clone(),
                Value::String(text) => serde_json::from_str(text).unwrap_or_default(),
                _ => Vec::new(),
            };
            let parts: Vec<String> = edits.iter().map(edit).collect();
            if parts.is_empty() {
                String::new()
            } else {
                format!("{}건: {}", parts.len(), parts.join("; "))
            }
        }
        "document_inspect" => args["section"].as_str().map_or_else(
            || "개요".to_owned(),
            |section| heading_text(section).to_owned(),
        ),
        "task_plan" => plan(&args),
        "file_write" | "file_edit" => path("path"),
        "file_patch" => {
            let parts: Vec<String> = args["operations"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|operation| {
                    let verb = match operation["action"].as_str().unwrap_or("") {
                        "add" => "추가",
                        "update" => "수정",
                        "replace" => "교체",
                        "move" => "이동",
                        "delete" => "삭제",
                        other => other,
                    };
                    let from = relative(root, operation["path"].as_str().unwrap_or(""));
                    match operation["to_path"].as_str() {
                        Some(to) if !from.is_empty() => {
                            join([verb.to_owned(), format!("{from} → {}", relative(root, to))])
                        }
                        Some(to) => join([verb.to_owned(), relative(root, to)]),
                        None => join([verb.to_owned(), from]),
                    }
                })
                .collect();
            if parts.is_empty() {
                String::new()
            } else {
                format!("{}건: {}", parts.len(), parts.join("; "))
            }
        }
        "db_query" => match args["action"].as_str() {
            Some("list") => "목록 보기".to_owned(),
            _ => args["id"].as_str().unwrap_or("").to_owned(),
        },
        "db_execute" => join([
            args["mode"].as_str().unwrap_or("").to_owned(),
            args["name"]
                .as_str()
                .or(args["sql"].as_str())
                .unwrap_or("")
                .to_owned(),
        ]),
        "memory_write" => args["title"]
            .as_str()
            .or(args["key"].as_str())
            .unwrap_or("")
            .to_owned(),
        "memory_read" => args["id"]
            .as_str()
            .or(args["key"].as_str())
            .unwrap_or("")
            .to_owned(),
        _ => String::new(),
    };
    clip(&join([label(name).to_owned(), detail]))
}

/// The same labels as the frontend's tool names (frontend/src/api.js).
fn label(name: &str) -> &str {
    match name {
        "document_inspect" => "문서 구조 조회",
        "document_audit" => "문서 근거 점검",
        // Each tool keeps its own name: the settings list its toggles by it.
        "document_edit" => "문서 편집",
        "document_edit_batch" => "문서 일괄 편집",
        "file_write" => "파일 쓰기",
        "file_edit" => "파일 수정",
        "file_patch" => "파일 패치",
        "db_query" => "DB 조회",
        "db_execute" => "DB 실행",
        "symbol_search" => "심볼 검색",
        "symbol_relations" => "호출·참조 추적",
        "code_outline" => "코드 구조 조회",
        "symbol_read" => "심볼 본문 읽기",
        "file_list" => "파일 목록",
        "file_read" => "파일 읽기",
        "source_search" => "소스 검색",
        "memory_write" => "기억 저장",
        "memory_read" => "기억 읽기",
        "memory_find" => "기억 검색",
        "memory_manage" => "기억 정리",
        "task_state" => "목표·진행 관리",
        "task_plan" => "할 일 목록 관리",
        "history" => "원문 조회",
        "source_lookup" => "기존 출처 조회",
        "checkpoint_complete" => "체크포인트 확인",
        "tool_catalog" => "도구 조회",
        "tool_select" => "도구 선택",
        other => other,
    }
}

/// One document edit: what it does and where.
fn edit(args: &Value) -> String {
    let text = args["text"].as_str().unwrap_or("");
    // Markdown headings only: code in the text (#[derive], #include) is not.
    let headings: Vec<String> = super::documentation::headings(text)
        .into_iter()
        .take(MAX_HEADINGS)
        .map(|heading| heading_text(&heading.heading).to_owned())
        .collect();
    let section = args["section"].as_str().map_or("", heading_text);
    let action = args["action"].as_str().unwrap_or("");
    let (verb, place) = match action {
        "create" => ("문서 생성", format!("{}줄", text.lines().count())),
        "write" => ("문서 전체 쓰기", format!("{}줄", text.lines().count())),
        "fix_word" => ("깨진 낱말 수정", String::new()),
        "append" | "insert_before" | "insert_after" | "insert_first_child"
        | "insert_last_child"
            if !headings.is_empty() =>
        {
            ("절 추가", headings.join(", "))
        }
        "append" => ("끝에 추가", String::new()),
        "insert_before" | "insert_after" | "insert_first_child" | "insert_last_child" => {
            ("내용 추가", section.to_owned())
        }
        "section" => ("절 교체", section.to_owned()),
        "delete_text" => ("문구 삭제", section.to_owned()),
        "patch" | "replace_text" | "insert_before_text" | "insert_after_text" => {
            ("문구 수정", section.to_owned())
        }
        _ => ("편집", section.to_owned()),
    };
    match action {
        _ if place.is_empty() => verb.to_owned(),
        "create" | "write" => format!("{verb} ({place})"),
        _ => format!("{verb}: {place}"),
    }
}

/// What one task_plan call changes, such as "T1 완료, T2 완료".
fn plan(args: &Value) -> String {
    let operations: Vec<Value> = match &args["operations"] {
        Value::Array(operations) => operations.clone(),
        Value::String(text) => serde_json::from_str(text).unwrap_or_default(),
        Value::Object(_) => vec![args["operations"].clone()],
        _ if args["action"] == "list" => return "목록 보기".to_owned(),
        _ => return String::new(),
    };
    operations
        .iter()
        .map(|operation| match operation {
            // An operation may come as quoted JSON, as the runtime accepts.
            Value::String(text) => serde_json::from_str(text).unwrap_or_default(),
            operation => operation.clone(),
        })
        .map(|operation: Value| {
            let id = operation["id"].as_str().unwrap_or("");
            match operation["op"].as_str().unwrap_or("") {
                "insert" => format!(
                    "할 일 {}개 추가",
                    operation["texts"].as_array().map_or(1, Vec::len)
                ),
                "complete" => format!("{id} 완료"),
                "update" => format!("{id} 수정"),
                "split" => format!("{id} 나누기"),
                "move" => format!("{id} 이동"),
                "remove" => format!("{id} 제거"),
                "reopen" => format!("{id} 다시 열기"),
                other => join([id.to_owned(), other.to_owned()]),
            }
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// ":start-end" from start_line with max_lines or end_line; ":start-" when
/// only the start is given; "" for a whole file. Providers send numbers as
/// strings too.
fn range(args: &Value) -> String {
    let number = |key: &str| match &args[key] {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    };
    let start = number("start_line");
    // limit is a name for max_lines that file_read accepts.
    let lines = number("max_lines").or_else(|| number("limit"));
    // Saturating: this runs outside the tool's panic guard, and a model can
    // send any number.
    let end = number("end_line")
        .or_else(|| lines.map(|lines| start.unwrap_or(1).saturating_add(lines.saturating_sub(1))));
    match (start, end) {
        (None, None) => String::new(),
        (start, Some(end)) => format!(":{}-{end}", start.unwrap_or(1)),
        (Some(start), None) => format!(":{start}-"),
    }
}

/// `path` without the project root and its separator. Both are compared in
/// display form, so a Windows root kept as \\?\C:\… matches the C:\… a model
/// sends, and Windows paths match regardless of letter case. The root itself
/// reads as "" (the tool's label alone).
fn relative(root: &Path, path: &str) -> String {
    let root = crate::paths::display_path(root);
    let root = root.trim_end_matches(['/', '\\']);
    let path = crate::paths::display_text(path);
    let windows = root.contains('\\');
    let same = |head: &str| {
        if windows {
            // A model may write C:/work/app for C:\work\app.
            head.replace('/', "\\").eq_ignore_ascii_case(root)
        } else {
            head == root
        }
    };
    match (path.get(..root.len()), path.get(root.len()..)) {
        (Some(head), Some(rest))
            if !root.is_empty()
                && same(head)
                && (rest.is_empty() || rest.starts_with(['/', '\\'])) =>
        {
            rest.trim_start_matches(['/', '\\']).to_owned()
        }
        _ => path.into_owned(),
    }
}

/// A heading or section name without its opening hashes and, as Markdown
/// reads it, a closing run of hashes after a space ("## B ##" is "B", while
/// "## C#" stays "C#").
fn heading_text(heading: &str) -> &str {
    let text = heading.trim().trim_start_matches('#').trim();
    let without = text.trim_end_matches('#');
    if without.len() < text.len() && (without.is_empty() || without.ends_with([' ', '\t'])) {
        without.trim_end()
    } else {
        text
    }
}

fn quoted(value: &Value) -> String {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .map_or_else(String::new, |text| format!("\"{text}\""))
}

fn join<const N: usize>(parts: [String; N]) -> String {
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn clip(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= MAX_CHARS {
        return text;
    }
    let mut clipped: String = text.chars().take(MAX_CHARS - 1).collect();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn summary(name: &str, args: Value) -> String {
        call_summary(Path::new("/work/app"), name, &args.to_string())
    }

    #[test]
    fn reads_and_searches_name_their_path_range_and_query() {
        assert_eq!(
            summary(
                "file_read",
                json!({"path":"/work/app/src/ui/menu.rs","start_line":"100","max_lines":200})
            ),
            "파일 읽기 src/ui/menu.rs:100-299"
        );
        assert_eq!(
            summary("file_read", json!({"path":"src/main.rs"})),
            "파일 읽기 src/main.rs"
        );
        assert_eq!(
            summary(
                "source_search",
                json!({"query":"fn show","path":"/work/app/src/ui/connection_dialog.rs"})
            ),
            "소스 검색 \"fn show\" src/ui/connection_dialog.rs"
        );
        assert_eq!(
            summary("source_search", json!({"queries":["agent","run"]})),
            "소스 검색 \"agent\" | \"run\""
        );
    }

    #[test]
    fn windows_paths_are_shown_under_a_verbatim_root() {
        let root = Path::new(r"\\?\C:\work\app");
        let read = |path: &str| call_summary(root, "file_read", &json!({"path":path}).to_string());
        assert_eq!(read(r"C:\work\app\src\main.rs"), r"파일 읽기 src\main.rs");
        assert_eq!(read(r"c:\Work\App\src\main.rs"), r"파일 읽기 src\main.rs");
        assert_eq!(
            read(r"\\?\C:\work\app\src\main.rs"),
            r"파일 읽기 src\main.rs"
        );
        // A sibling that only shares the root's prefix stays whole.
        assert_eq!(
            read(r"C:\work\application\x.rs"),
            r"파일 읽기 C:\work\application\x.rs"
        );
        assert_eq!(
            summary("file_list", json!({"path":"/work/app"})),
            "파일 목록"
        );
        assert_eq!(
            summary("file_read", json!({"path":"/work/application/x.rs"})),
            "파일 읽기 /work/application/x.rs"
        );
    }

    #[test]
    fn edits_name_the_sections_they_add_or_change() {
        assert_eq!(
            summary(
                "document_edit",
                json!({"action":"insert_after","section":"## 메인","text":"\n## 메뉴 구조\n### File 메뉴\n본문\n"})
            ),
            "문서 편집 절 추가: 메뉴 구조, File 메뉴"
        );
        assert_eq!(
            summary(
                "document_edit",
                json!({"action":"create","text":"# 매뉴얼\n\n## 메인\n"})
            ),
            "문서 편집 문서 생성 (3줄)"
        );
        // A batch sent as quoted JSON text is described too.
        let edits = json!([
            {"action":"replace_text","section":"## SQL 에디터","old_text":"a","text":"b"},
            {"action":"fix_word","fix_id":"W1","text":"word"}
        ]);
        assert_eq!(
            summary("document_edit_batch", json!({"edits":edits.to_string()})),
            "문서 일괄 편집 2건: 문구 수정: SQL 에디터; 깨진 낱말 수정"
        );
    }

    #[test]
    fn code_in_an_edit_is_not_taken_for_headings() {
        assert_eq!(
            summary(
                "document_edit",
                json!({"action":"append","text":"\n## 진입점\n```rust\n#[tokio::main]\n# not a heading\n```\n#include\n"})
            ),
            "문서 편집 절 추가: 진입점"
        );
        assert_eq!(
            summary(
                "document_edit",
                json!({"action":"append","text":"```c\n#include <x.h>\n```\n"})
            ),
            "문서 편집 끝에 추가"
        );
        // Unreadable batches and cursor-only reads still show their tool.
        assert_eq!(
            summary("document_edit_batch", json!({"edits":"[{broken"})),
            "문서 일괄 편집"
        );
        assert_eq!(
            summary("file_read", json!({"cursor":"abc:2"})),
            "파일 읽기 이어 읽기"
        );
    }

    #[test]
    fn odd_arguments_neither_panic_nor_show_stray_parts() {
        assert_eq!(
            summary(
                "file_read",
                json!({"path":"a.rs","start_line":2,"max_lines":u64::MAX})
            ),
            format!("파일 읽기 a.rs:2-{}", u64::MAX)
        );
        assert_eq!(summary("file_read", json!({"max_lines":20})), "파일 읽기");
        assert_eq!(
            summary("source_search", json!({"queries":["", "run", 3]})),
            "소스 검색 \"run\""
        );
        assert_eq!(
            summary("document_inspect", json!({"section":"## 2. 실행 루프"})),
            "문서 구조 조회 2. 실행 루프"
        );
        assert_eq!(
            summary(
                "task_plan",
                json!({"action":"apply","operations":["{\"op\":\"complete\",\"id\":\"T2\"}"]})
            ),
            "할 일 목록 관리 T2 완료"
        );
        assert_eq!(summary("file_read", json!("not an object")), "파일 읽기");
        assert_eq!(call_summary(Path::new("/"), "file_read", "{"), "파일 읽기");
    }

    #[test]
    fn symbol_reads_show_the_file_not_the_symbol_hash() {
        let id = format!("{}:{}:120:480", "a".repeat(64), "b".repeat(64));
        assert_eq!(
            summary(
                "symbol_read",
                json!({"path":"/work/app/src/agent.rs","symbol_id":id,"max_lines":30})
            ),
            "심볼 본문 읽기 src/agent.rs"
        );
        assert_eq!(
            summary(
                "symbol_read",
                json!({"path":"src/agent.rs","symbol_id":id,"start_line":140,"max_lines":30})
            ),
            "심볼 본문 읽기 src/agent.rs:140-169"
        );
        assert_eq!(
            summary(
                "symbol_relations",
                json!({"path":"src/agent.rs","symbol_id":id,"relation":"callers"})
            ),
            "호출·참조 추적 callers src/agent.rs"
        );
    }

    #[test]
    fn accepted_aliases_and_heading_marks_are_read_as_the_runtime_reads_them() {
        assert_eq!(
            summary(
                "file_read",
                json!({"path":"src/a.rs","start_line":100,"limit":50})
            ),
            "파일 읽기 src/a.rs:100-149"
        );
        assert_eq!(
            summary("source_search", json!({"pattern":"fn main"})),
            "소스 검색 \"fn main\""
        );
        assert_eq!(
            summary(
                "task_plan",
                json!({"action":"apply","operations":["{bad",{"op":"complete","id":"T1"}]})
            ),
            "할 일 목록 관리 T1 완료"
        );
        assert_eq!(
            summary(
                "document_edit",
                json!({"action":"append","text":"## B ##\n## C#\n"})
            ),
            "문서 편집 절 추가: B, C#"
        );
        assert_eq!(
            call_summary(
                Path::new(r"C:\work\app"),
                "file_read",
                &json!({"path":"C:/work/app/src/main.rs"}).to_string()
            ),
            "파일 읽기 src/main.rs"
        );
    }

    #[test]
    fn project_file_writes_and_database_calls_name_their_targets() {
        assert_eq!(
            summary(
                "file_edit",
                json!({"path":"/work/app/src/a.rs","old_text":"x","new_text":"y"})
            ),
            "파일 수정 src/a.rs"
        );
        assert_eq!(
            summary(
                "file_patch",
                json!({"operations":[
                    {"action":"update","path":"src/a.rs"},
                    {"action":"move","path":"/work/app/src/b.rs","to_path":"/work/app/src/c.rs"}]})
            ),
            "파일 패치 2건: 수정 src/a.rs; 이동 src/b.rs → src/c.rs"
        );
        assert_eq!(
            summary("db_query", json!({"action":"run","id":"orders_by_day"})),
            "DB 조회 orders_by_day"
        );
        assert_eq!(
            summary(
                "db_execute",
                json!({"mode":"query","sql":"SELECT 1 FROM dual"})
            ),
            "DB 실행 query SELECT 1 FROM dual"
        );
    }

    // The two tables drifted once: a label added only to the frontend
    // named two settings toggles alike.
    #[test]
    fn labels_match_the_frontend_tool_names() {
        let api = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("frontend/src/api.js"),
        )
        .unwrap();
        let table = &api[api.find("export const toolLabels").unwrap()..];
        let table = &table[..table.find("};").unwrap()];
        let entry = regex::Regex::new(r#"(?m)^\s+(\w+): "([^"]+)","#).unwrap();
        let frontend: Vec<(&str, &str)> = entry
            .captures_iter(table)
            .map(|c| (c.get(1).unwrap().as_str(), c.get(2).unwrap().as_str()))
            .collect();
        assert!(frontend.len() > 20, "{frontend:?}");
        for (name, frontend_label) in frontend {
            assert_eq!(label(name), frontend_label, "{name}");
        }
    }

    #[test]
    fn plan_changes_name_their_items_and_long_lines_are_cut() {
        assert_eq!(
            summary(
                "task_plan",
                json!({"action":"apply","operations":[{"op":"complete","id":"T1","result":"done"},{"op":"insert","texts":["a","b"]}]})
            ),
            "할 일 목록 관리 T1 완료, 할 일 2개 추가"
        );
        assert_eq!(
            summary("task_plan", json!({"action":"list"})),
            "할 일 목록 관리 목록 보기"
        );
        // An apply that lost its operations is not a listing.
        assert_eq!(
            summary("task_plan", json!({"action":"apply"})),
            "할 일 목록 관리"
        );
        assert_eq!(
            summary("checkpoint_complete", json!({"id":"x"})),
            "체크포인트 확인"
        );
        let long = summary("source_search", json!({"query":"x".repeat(300)}));
        assert_eq!(long.chars().count(), MAX_CHARS);
        assert!(long.ends_with('…'));
    }
}
