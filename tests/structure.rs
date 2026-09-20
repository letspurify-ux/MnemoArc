use mnemoarc::{
    config::{Config, Project},
    session::Session,
    tools::{self, ToolRegistry},
};
use serde_json::{Value, json};

fn setup() -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            ..Default::default()
        },
    );
    s.active_tools = ToolRegistry::optional_names();
    (dir, s)
}
fn run(s: &mut Session, name: &str, args: Value) -> Value {
    tools::execute(s, name, args).unwrap()
}

#[test]
fn rust_structure_tracks_impl_methods_multiline_signatures_and_exact_body() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("lib.rs"),"// fn fake() {}\npub struct Store;\nimpl Store {\n    pub fn load(\n        &self,\n    ) -> bool {\n        true\n    }\n}\n").unwrap();
    let result = run(&mut s, "code_outline", json!({"path":"lib.rs"}));
    assert_eq!(result["engine"], "tree-sitter");
    assert_eq!(result["has_parse_errors"], false);
    assert_eq!(result["total_symbols"], 3);
    let method = &result["symbols"][2];
    assert_eq!(method["name"], "load");
    assert_eq!(method["container"], "Store");
    assert_eq!(method["start_line"], 4);
    assert_eq!(method["end_line"], 8);
    assert_eq!(method["name_column"], 12);
    assert!(method["signature"].as_str().unwrap().contains("&self"));
    let body = run(
        &mut s,
        "symbol_read",
        json!({"path":"lib.rs","symbol_id":method["symbol_id"]}),
    );
    assert_eq!(body["content"]["line_start"], 4);
    assert_eq!(body["content"]["line_end"], 8);
    assert!(body["content"]["text"].as_str().unwrap().contains("true"));
    assert!(
        !body["content"]["text"]
            .as_str()
            .unwrap()
            .contains("impl Store")
    );
}

#[test]
fn all_grammars_find_methods_and_decorated_functions_without_comment_symbols() {
    let (dir, mut s) = setup();
    for (file, source, expected) in [
        (
            "a.js",
            "// function fake() {}\nexport class Store { async load() { return 1; } }\nconst go = () => 2;",
            vec!["Store", "load", "go"],
        ),
        (
            "a.ts",
            "export interface Store { load(): number; }\nexport const go = (x: number): number => x;",
            vec!["Store", "load", "go"],
        ),
        (
            "a.tsx",
            "export function View() { return <div>Hello</div>; }",
            vec!["View"],
        ),
        (
            "a.py",
            "# def fake(): pass\nclass Store:\n    @property\n    def value(self):\n        return 1\n",
            vec!["Store", "value"],
        ),
        (
            "a.java",
            "// class Fake {}\nclass Store { Store() {} int load() { return 1; } }",
            vec!["Store", "Store", "load"],
        ),
    ] {
        std::fs::write(dir.path().join(file), source).unwrap();
        let outline = run(&mut s, "code_outline", json!({"path":file}));
        assert_eq!(outline["has_parse_errors"], false, "{file}: {outline}");
        let names: Vec<_> = outline["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, expected, "{file}");
        if file == "a.py" {
            assert_eq!(outline["symbols"][1]["start_line"], 3);
        }
    }
}

#[test]
fn java_structure_tracks_nested_types_overloads_and_annotated_method_body() {
    let (dir, mut s) = setup();
    let source = "package example;\npublic class Store {\n    private int count, limit;\n    public Store() {}\n    @Deprecated\n    public int 읽기(\n        int amount\n    ) {\n        return amount;\n    }\n    public int 읽기() { return 0; }\n    interface Loader { int load(); }\n    enum Mode { FAST, SLOW }\n    record Item(int value) { Item { if (value < 0) throw new IllegalArgumentException(); } }\n    @interface Label { String value(); }\n}\n";
    std::fs::write(dir.path().join("Store.java"), source).unwrap();
    let outline = run(&mut s, "code_outline", json!({"path":"Store.java"}));
    assert_eq!(outline["language"], "java");
    assert_eq!(outline["has_parse_errors"], false);
    let symbols = outline["symbols"].as_array().unwrap();
    for (name, kind, container) in [
        ("Store", "class_declaration", ""),
        ("Store", "constructor_declaration", "Store"),
        ("count", "variable_declarator", "Store"),
        ("limit", "variable_declarator", "Store"),
        ("Loader", "interface_declaration", "Store"),
        ("load", "method_declaration", "Store::Loader"),
        ("Mode", "enum_declaration", "Store"),
        ("FAST", "enum_constant", "Store::Mode"),
        ("Item", "record_declaration", "Store"),
        ("Item", "compact_constructor_declaration", "Store::Item"),
        ("Label", "annotation_type_declaration", "Store"),
        (
            "value",
            "annotation_type_element_declaration",
            "Store::Label",
        ),
    ] {
        assert!(
            symbols
                .iter()
                .any(|v| v["name"] == name && v["kind"] == kind && v["container"] == container),
            "missing {container}::{name} ({kind}): {outline}"
        );
    }
    let methods: Vec<_> = symbols.iter().filter(|v| v["name"] == "읽기").collect();
    assert_eq!(methods.len(), 2);
    assert_ne!(methods[0]["symbol_id"], methods[1]["symbol_id"]);
    let method = methods[0];
    assert_eq!(method["start_line"], 5);
    assert_eq!(method["end_line"], 10);
    assert_eq!(method["name_line"], 6);
    assert_eq!(method["name_column"], 16);
    assert!(method["signature"].as_str().unwrap().contains("int amount"));
    let body = run(
        &mut s,
        "symbol_read",
        json!({"path":"Store.java","symbol_id":method["symbol_id"]}),
    );
    assert_eq!(
        body["content"]["text"].as_str().unwrap().trim_end(),
        source
            .lines()
            .skip(4)
            .take(6)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn java_pagination_stale_symbols_and_syntax_errors() {
    let (dir, mut s) = setup();
    let path = dir.path().join("Store.java");
    std::fs::write(&path, "class Store { void load() {} void load(int n) {} }").unwrap();
    let first = run(
        &mut s,
        "code_outline",
        json!({"path":"Store.java","query":"LOAD","limit":1}),
    );
    assert_eq!(first["total_symbols"], 2);
    let second = run(
        &mut s,
        "code_outline",
        json!({"path":"Store.java","query":"LOAD","cursor":first["next_cursor"]}),
    );
    assert_ne!(
        first["symbols"][0]["symbol_id"],
        second["symbols"][0]["symbol_id"]
    );
    std::fs::write(&path, "class Store { void load( {").unwrap();
    assert!(
        tools::execute(
            &mut s,
            "symbol_read",
            json!({"path":"Store.java","symbol_id":first["symbols"][0]["symbol_id"]})
        )
        .unwrap_err()
        .to_string()
        .contains("revision_conflict")
    );
    assert!(
        tools::execute(
            &mut s,
            "code_outline",
            json!({"path":"Store.java","query":"LOAD","cursor":first["next_cursor"]})
        )
        .is_err()
    );
    assert_eq!(
        run(&mut s, "code_outline", json!({"path":"Store.java"}))["has_parse_errors"],
        true
    );
}

#[test]
fn structure_cursors_ids_errors_and_project_boundaries() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), "fn first() {}\nfn second() {}\n").unwrap();
    let first = run(&mut s, "code_outline", json!({"path":"a.rs","limit":1}));
    let next = run(
        &mut s,
        "code_outline",
        json!({"path":"a.rs","cursor":first["next_cursor"]}),
    );
    assert_eq!(next["symbols"][0]["name"], "second");
    assert!(
        tools::execute(
            &mut s,
            "code_outline",
            json!({"path":"a.rs","query":"first","cursor":first["next_cursor"]})
        )
        .is_err()
    );
    std::fs::write(dir.path().join("a.rs"), "fn first() {\n").unwrap();
    assert!(
        tools::execute(
            &mut s,
            "symbol_read",
            json!({"path":"a.rs","symbol_id":first["symbols"][0]["symbol_id"]})
        )
        .unwrap_err()
        .to_string()
        .contains("revision_conflict")
    );
    assert!(
        tools::execute(
            &mut s,
            "code_outline",
            json!({"path":"a.rs","cursor":first["next_cursor"]})
        )
        .is_err()
    );
    assert_eq!(
        run(&mut s, "code_outline", json!({"path":"a.rs"}))["has_parse_errors"],
        true
    );
    s.project.exclude.push("*.rs".into());
    assert!(tools::execute(&mut s, "code_outline", json!({"path":"a.rs"})).is_err());
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
    assert!(
        tools::execute(&mut s, "code_outline", json!({"path":"a.txt"}))
            .unwrap_err()
            .to_string()
            .contains("unsupported_language")
    );
}

#[test]
fn symbol_body_obeys_final_budget_and_file_cursor_without_skipping_text() {
    let (dir, mut s) = setup();
    let body = format!(
        "fn long() {{\n{}\n}}",
        "    // 한글 설명 및 본문\n".repeat(300)
    );
    std::fs::write(dir.path().join("a.rs"), &body).unwrap();
    let outline = run(&mut s, "code_outline", json!({"path":"a.rs"}));
    let call = mnemoarc::llm::ToolCall {
        id: "symbol-body".into(),
        name: "symbol_read".into(),
        arguments: json!({"path":"a.rs","symbol_id":outline["symbols"][0]["symbol_id"]})
            .to_string(),
    };
    let result = tools::run_call(&mut s, &call);
    let page = tools::limit_result(&mut s, &call, result, 1200);
    assert_eq!(page["next_cursor"]["tool"], "file_read");
    assert!(tools::result_tokens(&call, &page, &s.config.model) <= 1200);
    tools::record_delivered_read(&mut s, &call, &page);
    assert!(!s.read_coverage.is_empty());
    let prefix = page["data"]["content"]["text"].as_str().unwrap();
    assert!(body.starts_with(prefix));
    let next = run(
        &mut s,
        "file_read",
        json!({"cursor":page["next_cursor"]["cursor"]}),
    );
    let combined = format!("{prefix}{}", next["content"]["text"].as_str().unwrap());
    assert!(body.starts_with(&combined));
}
