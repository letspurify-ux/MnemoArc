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
fn unknown_symbol_means_current_hash_but_no_matching_symbol_id() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), "fn real() {}\n").unwrap();
    let outline = run(&mut s, "code_outline", json!({"path":"a.rs"}));
    let observed = outline["symbols"][0]["symbol_id"].as_str().unwrap();
    let hash = observed.split(':').next().unwrap();
    let call = mnemoarc::llm::ToolCall {
        id: "unobserved-symbol".into(),
        name: "symbol_read".into(),
        arguments: json!({"path":"a.rs","symbol_id":format!("{hash}:999:1000")}).to_string(),
    };
    let result = tools::run_call(&mut s, &call);
    assert_eq!(result["recovery"]["code"], "unknown_symbol");
    assert_eq!(result["recovery"]["class"], "invalid_input");
    assert_eq!(result["recovery"]["action"], "copy_observed_symbol_id");
    assert_eq!(
        result["recovery"]["tools"],
        json!(["code_outline", "symbol_read"])
    );
    assert_eq!(result["recovery"]["automatic_retry"], false);
    let valid = run(
        &mut s,
        "symbol_read",
        json!({"path":"a.rs","symbol_id":observed}),
    );
    assert!(
        valid["content"]["text"]
            .as_str()
            .unwrap()
            .contains("fn real")
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
    assert_eq!(methods[0]["qualified_name"], "Store::읽기");
    let missing = run(
        &mut s,
        "code_outline",
        json!({"path":"Store.java","kind":"method","container":"Store.Loader","view":"compact"}),
    );
    assert_eq!(missing["empty_reason"], "no_matching_symbols");
    assert!(
        missing["available_containers"]
            .as_array()
            .unwrap()
            .contains(&json!("Store::Loader"))
    );
    assert_eq!(missing["total_symbols"], 0);
    let inner = run(
        &mut s,
        "code_outline",
        json!({"path":"Store.java","kind":"interface","query":"Loader","view":"compact"}),
    );
    assert_eq!(inner["symbols"][0]["qualified_name"], "Store::Loader");
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

#[test]
fn budgeted_outline_pages_keep_symbol_ids_without_skipping_or_archiving() {
    let (dir, mut s) = setup();
    let source: String = (0..63)
        .map(|i| format!("fn target_{i:03}() -> usize {{ {i} }}\n"))
        .collect();
    std::fs::write(dir.path().join("large.rs"), &source).unwrap();
    let all = run(
        &mut s,
        "code_outline",
        json!({"path":"large.rs","query":"target","limit":100}),
    );
    let expected: Vec<_> = all["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["symbol_id"].clone())
        .collect();
    let mut args = json!({"path":"large.rs","query":"TARGET","limit":17});
    let mut seen = Vec::new();
    let mut first_cursor = Value::Null;
    for page_index in 0..100 {
        let call = mnemoarc::llm::ToolCall {
            id: format!("outline-{page_index}"),
            name: "code_outline".into(),
            arguments: args.to_string(),
        };
        let raw = tools::run_call(&mut s, &call);
        let mut page = tools::limit_result(&mut s, &call, raw, 1800);
        // Exercise repeated bounding, as well as pages starting at nonzero offsets.
        page = tools::limit_result(&mut s, &call, page, 1200);
        assert!(tools::result_tokens(&call, &page, &s.config.model) <= 1200);
        assert!(page.get("archive_id").is_none(), "{page}");
        let symbols = page["data"]["symbols"].as_array().unwrap();
        assert!(!symbols.is_empty(), "{page}");
        seen.extend(symbols.iter().map(|v| v["symbol_id"].clone()));
        if page_index == 0 {
            assert_eq!(page["next_cursor"]["tool"], "code_outline");
            first_cursor = page["data"]["next_cursor"].clone();
        }
        if page["next_cursor"]["tool"] == "code_outline" {
            args = page["next_cursor"].clone();
            args.as_object_mut().unwrap().remove("tool");
            args["limit"] = json!(17);
        } else if page["data"]["next_cursor"].is_string() {
            args["cursor"] = page["data"]["next_cursor"].clone();
        } else {
            break;
        }
    }
    assert_eq!(seen, expected);
    assert!(s.history.bundles.is_empty());
    let body = run(
        &mut s,
        "symbol_read",
        json!({"path":"large.rs","symbol_id":seen[0]}),
    );
    assert!(
        body["content"]["text"]
            .as_str()
            .unwrap()
            .contains("target_000")
    );
    assert!(
        tools::execute(
            &mut s,
            "code_outline",
            json!({"path":"large.rs","query":"other","cursor":first_cursor})
        )
        .is_err()
    );
    std::fs::write(dir.path().join("large.rs"), format!("{source}\n")).unwrap();
    assert!(
        tools::execute(
            &mut s,
            "code_outline",
            json!({"path":"large.rs","query":"target","cursor":first_cursor})
        )
        .is_err()
    );
}

#[test]
fn outline_with_no_room_for_one_symbol_uses_lossless_archive() {
    let (dir, mut s) = setup();
    std::fs::write(
        dir.path().join("large.rs"),
        format!("fn {}() {{}}", "long_name_".repeat(60)),
    )
    .unwrap();
    let call = mnemoarc::llm::ToolCall {
        id: "tiny-outline".into(),
        name: "code_outline".into(),
        arguments: json!({"path":"large.rs"}).to_string(),
    };
    let raw = tools::run_call(&mut s, &call);
    let expected = raw["data"]["symbols"].clone();
    let page = tools::limit_result(&mut s, &call, raw, 200);
    assert!(tools::result_tokens(&call, &page, &s.config.model) <= 200);
    assert_eq!(page["next_cursor"]["tool"], "history");
    let archive = s
        .history
        .read(page["next_cursor"]["id"].as_u64().unwrap())
        .unwrap();
    assert_eq!(archive.messages[0]["result"]["data"]["symbols"], expected);
}

#[test]
fn compact_overview_and_exact_function_lookup_lead_to_the_same_body() {
    let (dir, mut s) = setup();
    let source = "export const Load = (n) => {\n  const local = n;\n  return local;\n};\nfunction loadMore() {}\nclass Store {\n  constructor() {}\n  load() { const local = 1; return local; }\n}\n";
    std::fs::write(dir.path().join("a.js"), source).unwrap();
    let overview = run(
        &mut s,
        "code_outline",
        json!({"path":"a.js","view":"compact","max_depth":0}),
    );
    let names: Vec<_> = overview["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Load", "loadMore", "Store"]);
    assert!(
        s.sources.is_empty(),
        "compact navigation must not issue source evidence"
    );
    for symbol in overview["symbols"].as_array().unwrap() {
        assert!(symbol.get("signature").is_none());
        assert!(symbol.get("source").is_none());
        assert_eq!(symbol["depth"], 0);
    }
    let function = run(
        &mut s,
        "code_outline",
        json!({"path":"a.js","query":"load","match":"exact","kind":"function"}),
    );
    assert_eq!(function["total_symbols"], 1);
    let symbol = &function["symbols"][0];
    assert_eq!(symbol["symbol_id"], overview["symbols"][0]["symbol_id"]);
    assert_eq!(symbol["kind"], "variable_declarator");
    assert_eq!(symbol["symbol_kind"], "function");
    assert_eq!(symbol["signature"], "export const Load = (n) =>");
    let body = run(
        &mut s,
        "symbol_read",
        json!({"path":"a.js","symbol_id":symbol["symbol_id"]}),
    );
    assert!(
        body["content"]["text"]
            .as_str()
            .unwrap()
            .starts_with("export const Load")
    );
    assert_eq!(
        run(
            &mut s,
            "code_outline",
            json!({"path":"a.js","query":"load","match":"exact","case_sensitive":true,"kind":"function"})
        )["total_symbols"],
        0
    );
    let methods = run(
        &mut s,
        "code_outline",
        json!({"path":"a.js","kind":"method","container":"Store","max_depth":1}),
    );
    assert_eq!(methods["total_symbols"], 1);
    assert_eq!(methods["symbols"][0]["name"], "load");
    let constructors = run(
        &mut s,
        "code_outline",
        json!({"path":"a.js","kind":"constructor"}),
    );
    assert_eq!(constructors["symbols"][0]["name"], "constructor");
    assert_eq!(
        run(
            &mut s,
            "code_outline",
            json!({"path":"a.js","container":"store"})
        )["total_symbols"],
        0
    );
}

#[test]
fn java_field_signatures_preserve_modifiers_and_ids_distinguish_declarators() {
    let (dir, mut s) = setup();
    let source = "class Store {\n  private final int first = 1, second = 2;\n  int load() { return first; }\n  class Inner { int load() { return second; } }\n}\n";
    std::fs::write(dir.path().join("Store.java"), source).unwrap();
    let fields = run(
        &mut s,
        "code_outline",
        json!({"path":"Store.java","kind":"field","container":"Store"}),
    );
    assert_eq!(fields["total_symbols"], 2);
    let fields = fields["symbols"].as_array().unwrap();
    assert_ne!(fields[0]["symbol_id"], fields[1]["symbol_id"]);
    for field in fields {
        assert!(
            field["signature"]
                .as_str()
                .unwrap()
                .starts_with("private final int")
        );
        let body = run(
            &mut s,
            "symbol_read",
            json!({"path":"Store.java","symbol_id":field["symbol_id"]}),
        );
        assert!(
            body["content"]["text"]
                .as_str()
                .unwrap()
                .contains("private final int")
        );
    }
    let nested = run(
        &mut s,
        "code_outline",
        json!({"path":"Store.java","kind":"method","container":"Store::Inner"}),
    );
    assert_eq!(nested["total_symbols"], 1);
    assert_eq!(nested["symbols"][0]["depth"], 2);
    assert_eq!(
        run(
            &mut s,
            "code_outline",
            json!({"path":"Store.java","kind":"method","max_depth":1})
        )["total_symbols"],
        1
    );
}

#[test]
fn normalized_kinds_distinguish_functions_methods_and_fields_across_languages() {
    let (dir, mut s) = setup();
    for (path, source) in [
        (
            "a.rs",
            "struct Store { value: i32 } impl Store { fn load(&self) { fn nested() {} } } fn top() {}",
        ),
        (
            "a.py",
            "class Store:\n    def load(self):\n        def nested(): pass\ndef top(): pass\n",
        ),
        (
            "a.ts",
            "class Store { value: number; load() { function nested() {} } } const top = () => 1;",
        ),
    ] {
        std::fs::write(dir.path().join(path), source).unwrap();
        let method = run(&mut s, "code_outline", json!({"path":path,"kind":"method"}));
        assert_eq!(method["has_parse_errors"], false);
        assert_eq!(method["total_symbols"], 1, "{path}: {method}");
        assert_eq!(method["symbols"][0]["name"], "load");
        let functions = run(
            &mut s,
            "code_outline",
            json!({"path":path,"kind":"function","max_depth":0}),
        );
        assert_eq!(functions["total_symbols"], 1, "{path}: {functions}");
        assert_eq!(functions["symbols"][0]["name"], "top");
        if path != "a.py" {
            let fields = run(&mut s, "code_outline", json!({"path":path,"kind":"field"}));
            assert_eq!(fields["total_symbols"], 1, "{path}: {fields}");
        }
    }
}

#[test]
fn budgeted_cursors_preserve_all_filters_and_reject_changed_options() {
    let (dir, mut s) = setup();
    let source = format!(
        "class Store {{\n{}\n}}",
        (0..60)
            .map(|i| format!("int load(int p{i}) {{ return p{i}; }}\n"))
            .collect::<String>()
    );
    std::fs::write(dir.path().join("Store.java"), source).unwrap();
    let filters = json!({"path":"Store.java","query":"load","match":"exact","case_sensitive":true,"kind":"method","container":"Store","max_depth":1,"view":"compact","limit":100});
    let mut args = filters.clone();
    let mut ids = std::collections::BTreeSet::new();
    let mut cursor = Value::Null;
    for index in 0..60 {
        let call = mnemoarc::llm::ToolCall {
            id: format!("page-{index}"),
            name: "code_outline".into(),
            arguments: args.to_string(),
        };
        let raw = tools::run_call(&mut s, &call);
        let page = tools::limit_result(&mut s, &call, raw, 1600);
        assert!(tools::result_tokens(&call, &page, &s.config.model) <= 1600);
        for symbol in page["data"]["symbols"].as_array().unwrap() {
            assert!(ids.insert(symbol["symbol_id"].as_str().unwrap().to_owned()));
        }
        if index == 0 {
            cursor = page["data"]["next_cursor"].clone();
            for key in [
                "query",
                "match",
                "case_sensitive",
                "kind",
                "container",
                "max_depth",
                "view",
            ] {
                assert_eq!(page["next_cursor"][key], filters[key]);
            }
        }
        if page["data"]["next_cursor"].is_null() {
            break;
        }
        args = if page["next_cursor"]["tool"] == "code_outline" {
            let mut next = page["next_cursor"].clone();
            next.as_object_mut().unwrap().remove("tool");
            next
        } else {
            args["cursor"] = page["data"]["next_cursor"].clone();
            args
        };
        args["limit"] = json!(20);
    }
    assert_eq!(ids.len(), 60);
    for (key, value) in [
        ("match", json!("contains")),
        ("case_sensitive", json!(false)),
        ("kind", json!("field")),
        ("container", json!("")),
        ("max_depth", json!(2)),
        ("view", json!("detailed")),
        ("query", json!("Load")),
    ] {
        let mut changed = filters.clone();
        changed["cursor"] = cursor.clone();
        changed[key] = value;
        assert!(
            tools::execute(&mut s, "code_outline", changed).is_err(),
            "changed {key}"
        );
    }
}

#[test]
fn multiline_signatures_have_their_own_evidence_and_mark_truncation() {
    let (dir, mut s) = setup();
    let source =
        "interface Store {\n    int load(\n        int value,\n        String name\n    );\n}\n";
    std::fs::write(dir.path().join("Store.java"), source).unwrap();
    let outline = run(
        &mut s,
        "code_outline",
        json!({"path":"Store.java","query":"load","match":"exact"}),
    );
    let method = &outline["symbols"][0];
    assert!(
        method["signature"]
            .as_str()
            .unwrap()
            .contains("String name")
    );
    assert_eq!(method["signature_truncated"], false);
    assert_eq!(method["signature_source"]["start_line"], 2);
    assert_eq!(method["signature_source"]["end_line"], 5);
    assert_eq!(method["source"]["end_line"], 2);
    let source_id = method["signature_source"]["id"].as_str().unwrap();
    assert!(s.sources[source_id].excerpt.contains("String name"));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn load(\n    value: i32,\n) -> i32 {\n    value + 1\n}\n",
    )
    .unwrap();
    let outline = run(&mut s, "code_outline", json!({"path":"a.rs"}));
    let function = &outline["symbols"][0];
    assert_eq!(function["signature_end_line"], 3);
    assert!(
        !function["signature_source"]["excerpt"]
            .as_str()
            .unwrap()
            .contains("value + 1")
    );
    std::fs::write(
        dir.path().join("a.rs"),
        format!("fn load({}: i32) {{}}", "long_parameter_".repeat(60)),
    )
    .unwrap();
    let outline = run(&mut s, "code_outline", json!({"path":"a.rs"}));
    assert_eq!(outline["symbols"][0]["signature_truncated"], true);
    assert_eq!(
        outline["symbols"][0]["signature"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        500
    );
}

#[test]
fn symbol_read_slices_use_absolute_lines_and_cannot_escape_symbol() {
    let (dir, mut s) = setup();
    let source = "// header\n\nfn target() {\n    let first = 1;\n    let second = 2;\n    let third = 3;\n}\nfn next() {}\n";
    let path = dir.path().join("a.rs");
    std::fs::write(&path, source).unwrap();
    let outline = run(
        &mut s,
        "code_outline",
        json!({"path":"a.rs","query":"target","match":"exact"}),
    );
    let id = &outline["symbols"][0]["symbol_id"];
    let first = run(
        &mut s,
        "symbol_read",
        json!({"path":"a.rs","symbol_id":id,"max_lines":2}),
    );
    assert_eq!(first["content"]["line_start"], 3);
    assert_eq!(first["content"]["line_end"], 4);
    assert_eq!(first["next_line"], 5);
    let middle = run(
        &mut s,
        "symbol_read",
        json!({"path":"a.rs","symbol_id":id,"start_line":5,"max_lines":1}),
    );
    assert_eq!(middle["content"]["text"], "    let second = 2;");
    assert_eq!(middle["source"]["start_line"], 5);
    assert_eq!(middle["source"]["end_line"], 5);
    let tail = run(
        &mut s,
        "symbol_read",
        json!({"path":"a.rs","symbol_id":id,"start_line":6,"max_lines":2000}),
    );
    assert_eq!(tail["content"]["line_end"], 7);
    assert!(!tail["content"]["text"].as_str().unwrap().contains("next"));
    let before = s.sources.len();
    for extra in [
        json!({"start_line":0}),
        json!({"start_line":2}),
        json!({"start_line":8}),
        json!({"start_line":u64::MAX}),
        json!({"max_lines":0}),
        json!({"max_lines":2001}),
        json!({"max_lines":u64::MAX}),
    ] {
        let mut args = json!({"path":"a.rs","symbol_id":id});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(
            tools::execute(&mut s, "symbol_read", args)
                .unwrap_err()
                .to_string()
                .contains("invalid_symbol_range")
        );
        assert_eq!(s.sources.len(), before);
    }
    std::fs::write(&path, format!("{source}\n")).unwrap();
    assert!(
        tools::execute(
            &mut s,
            "symbol_read",
            json!({"path":"a.rs","symbol_id":id,"max_lines":1})
        )
        .unwrap_err()
        .to_string()
        .contains("revision_conflict")
    );
}

#[test]
fn budgeted_symbol_slice_cursors_complete_only_the_requested_range() {
    let (dir, mut s) = setup();
    let source = format!(
        "fn target() {{\n{}\n}}\nfn next() {{}}",
        format!("    // {}\n", "한글 설명 ".repeat(120)).repeat(10)
    );
    std::fs::write(dir.path().join("a.rs"), &source).unwrap();
    let outline = run(
        &mut s,
        "code_outline",
        json!({"path":"a.rs","query":"target"}),
    );
    let expected = source
        .lines()
        .skip(1)
        .take(3)
        .collect::<Vec<_>>()
        .join("\n");
    let mut call = mnemoarc::llm::ToolCall {
        id:"slice".into(), name:"symbol_read".into(),
        arguments:json!({"path":"a.rs","symbol_id":outline["symbols"][0]["symbol_id"],"start_line":2,"max_lines":3}).to_string(),
    };
    let mut combined = String::new();
    for i in 0..40 {
        let raw = tools::run_call(&mut s, &call);
        let page = tools::limit_result(&mut s, &call, raw, 1200);
        assert_eq!(page["status"], "ok", "{page}");
        let shown = page["data"]["content"]["text"].as_str().unwrap();
        if let Some(excerpt) = page["data"]["source"]["excerpt"].as_str() {
            assert_eq!(excerpt, shown.chars().take(2000).collect::<String>());
        }
        let source_id = page["data"]["source"]["id"].as_str().unwrap();
        assert_eq!(
            s.sources[source_id].excerpt,
            shown.chars().take(2000).collect::<String>()
        );
        let model = tools::model_result(&page);
        assert!(model["data"]["content"].get("text").is_none());
        assert_eq!(model["next_cursor"], page["next_cursor"]);
        let numbered = model["data"]["content"]["numbered_text"].as_str().unwrap();
        let start = page["data"]["content"]["line_start"].as_u64().unwrap();
        let restored = numbered
            .split_inclusive('\n')
            .enumerate()
            .map(|(index, line)| {
                let (label, text) = line.split_once('|').unwrap();
                assert_eq!(label.parse::<u64>().unwrap(), start + index as u64);
                text
            })
            .collect::<String>();
        assert_eq!(restored, shown);
        assert!(tools::result_tokens(&call, &page, &s.config.model) <= 1200);
        assert_eq!(page["data"]["read_start"], 2);
        assert_eq!(page["data"]["read_max_lines"], 3);
        assert!(page["data"]["source"]["end_line"].as_u64().unwrap() <= 4);
        tools::record_delivered_read(&mut s, &call, &page);
        combined.push_str(page["data"]["content"]["text"].as_str().unwrap());
        if page["next_cursor"]["tool"] != "file_read" {
            break;
        }
        call = mnemoarc::llm::ToolCall {
            id: format!("slice-{i}"),
            name: "file_read".into(),
            arguments: json!({"cursor":page["next_cursor"]["cursor"]}).to_string(),
        };
    }
    assert_eq!(combined, expected);
    let coverage = s.read_coverage.values().next().unwrap();
    assert_eq!(
        coverage.ranges,
        vec![(
            source.lines().next().unwrap().len() + 1,
            source.lines().next().unwrap().len() + 1 + expected.chars().count() + 1
        )]
    );
}

#[test]
fn model_line_labels_preserve_blank_lines_pipes_and_partial_boundaries() {
    let raw = json!({"status":"ok","data":{"read_start":10,"read_offset":3,
        "content":{"text":"한글|x\n\n끝\n","line_start":12,"line_end":14,
        "line_offsets":[0,5,6,8],"first_line_complete":false,"last_line_complete":true}},
        "next_cursor":{"tool":"file_read","cursor":"R7"}});
    let shown = tools::model_result(&raw);
    assert_eq!(
        shown["data"]["content"]["numbered_text"],
        "12|한글|x\n13|\n14|끝\n"
    );
    assert!(shown["data"]["content"].get("line_offsets").is_none());
    assert_eq!(shown["data"]["content"]["first_line_complete"], false);
    assert_eq!(shown["next_cursor"], raw["next_cursor"]);
    assert_eq!(shown["data"]["read_offset"], 3);
    assert_eq!(raw["data"]["content"]["text"], "한글|x\n\n끝\n");
    assert_eq!(tools::model_result(&shown), shown);
    for raw in [
        json!({"status":"ok","data":{"read_start":1,"content":{"text":"","line_start":1}}}),
        json!({"status":"ok","data":{"content":{"text":"a\nb","line_start":1}}}),
        json!({"status":"error","data":{"read_start":1,"content":{"text":"a","line_start":1}}}),
    ] {
        assert_eq!(tools::model_result(&raw), raw);
    }
}

#[test]
fn java_overloads_and_nested_methods_have_unambiguous_locations() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("Store.java"),
        "class Store {\n int load() { return 1; }\n int load(int value) { return value; }\n class Inner {\n  int load() { return 2; }\n }\n}\n").unwrap();
    let outline = run(
        &mut s,
        "code_outline",
        json!({"path":"Store.java","view":"compact","kind":"method","container":"Store","query":"load","match":"exact"}),
    );
    let methods = outline["symbols"].as_array().unwrap();
    assert_eq!(methods.len(), 2);
    assert_eq!(methods[0]["location"], "Store.java:2-2");
    assert_eq!(methods[1]["location"], "Store.java:3-3");
    assert_ne!(methods[0]["symbol_id"], methods[1]["symbol_id"]);
    assert!(
        s.sources.is_empty(),
        "compact locations are navigation, not body evidence"
    );
    let nested = run(
        &mut s,
        "code_outline",
        json!({"path":"Store.java","kind":"method","container":"Store::Inner"}),
    );
    assert_eq!(nested["symbols"][0]["location"], "Store.java:5-5");
    let body = run(
        &mut s,
        "symbol_read",
        json!({"path":"Store.java","symbol_id":methods[1]["symbol_id"],"max_lines":1}),
    );
    assert_eq!(body["source"]["start_line"], 3);
    assert!(
        body["content"]["text"]
            .as_str()
            .unwrap()
            .contains("return value")
    );
}

#[test]
fn csharp_file_namespace_members_signatures_and_partial_reads() {
    let (dir, mut s) = setup();
    let source = r#"using System;
namespace Demo.Core;
public interface IStore { string Load(int id); }
public partial class Store : IStore {
 public const int Limit = 10;
 private int count, limit;
 public Store(int count = 0) { this.count = count; }
 public string Name { get; private set; } = "";
 public int Value { get => count; init { count = value; } }
 public string this[int index] => index.ToString();
 public event Action Changed;
 public event Action Updated { add { } remove { } }
 [Obsolete]
 public string Load(
     int id = 0
 ) => id.ToString();
 public string Load(string text) {
     return text;
 }
 public class Inner { public int Read() => 1; }
 public int Run() { int Local(int n) => n + 1; return Local(count); }
 ~Store() { }
 public static Store operator +(Store a, Store b) => a;
 public static implicit operator int(Store value) => value.count;
}
public record Item(int Id);
public readonly record struct Point(int X, int Y);
public struct Counter { public int Value; }
public enum Mode { Fast, Slow }
public delegate void Handler(string value);
"#;
    std::fs::write(dir.path().join("Store.cs"), source).unwrap();
    let outline = run(&mut s, "code_outline", json!({"path":"Store.cs"}));
    assert_eq!(outline["language"], "csharp");
    assert_eq!(outline["has_parse_errors"], false, "{outline}");
    let symbols = outline["symbols"].as_array().unwrap();
    for (name, kind, container) in [
        ("Demo.Core", "module", ""),
        ("IStore", "interface", "Demo.Core"),
        ("Store", "class", "Demo.Core"),
        ("Store", "constructor", "Demo.Core::Store"),
        ("Limit", "constant", "Demo.Core::Store"),
        ("count", "field", "Demo.Core::Store"),
        ("limit", "field", "Demo.Core::Store"),
        ("Name", "property", "Demo.Core::Store"),
        ("get", "accessor", "Demo.Core::Store::Name"),
        ("set", "accessor", "Demo.Core::Store::Name"),
        ("init", "accessor", "Demo.Core::Store::Value"),
        ("this", "property", "Demo.Core::Store"),
        ("Changed", "event", "Demo.Core::Store"),
        ("Updated", "event", "Demo.Core::Store"),
        ("add", "accessor", "Demo.Core::Store::Updated"),
        ("remove", "accessor", "Demo.Core::Store::Updated"),
        ("Read", "method", "Demo.Core::Store::Inner"),
        ("Local", "function", "Demo.Core::Store::Run"),
        ("~Store", "destructor", "Demo.Core::Store"),
        ("operator +", "operator", "Demo.Core::Store"),
        ("implicit operator int", "operator", "Demo.Core::Store"),
        ("Item", "record", "Demo.Core"),
        ("Point", "record", "Demo.Core"),
        ("Counter", "struct", "Demo.Core"),
        ("Fast", "enum_member", "Demo.Core::Mode"),
        ("Handler", "delegate", "Demo.Core"),
    ] {
        assert!(
            symbols.iter().any(|v| v["name"] == name
                && v["symbol_kind"] == kind
                && v["container"] == container),
            "missing {container}::{name} ({kind}): {outline}"
        );
    }
    let field = symbols.iter().find(|v| v["name"] == "count").unwrap();
    assert_eq!(field["signature"], "private int count, limit;");
    let changed = symbols.iter().find(|v| v["name"] == "Changed").unwrap();
    assert_eq!(changed["signature"], "public event Action Changed;");
    let value = symbols
        .iter()
        .find(|v| v["name"] == "Value" && v["symbol_kind"] == "property")
        .unwrap();
    assert_eq!(value["signature"], "public int Value");
    let first = run(
        &mut s,
        "code_outline",
        json!({"path":"Store.cs","kind":"method","container":"Demo.Core::Store","query":"Load","match":"exact","limit":1}),
    );
    assert_eq!(first["total_symbols"], 2);
    let method = &first["symbols"][0];
    assert_eq!(method["depth"], 2);
    assert_eq!(
        method["signature"],
        "[Obsolete]\n public string Load(\n     int id = 0\n )"
    );
    assert_eq!(method["signature_source"]["start_line"], 13);
    assert_eq!(method["signature_source"]["end_line"], 16);
    let next = run(
        &mut s,
        "code_outline",
        json!({"path":"Store.cs","kind":"method","container":"Demo.Core::Store","query":"Load","match":"exact","limit":1,"cursor":first["next_cursor"]}),
    );
    assert_ne!(method["symbol_id"], next["symbols"][0]["symbol_id"]);
    let body = run(
        &mut s,
        "symbol_read",
        json!({"path":"Store.cs","symbol_id":next["symbols"][0]["symbol_id"],"start_line":18,"max_lines":1}),
    );
    assert_eq!(body["content"]["text"], "     return text;");
    assert_eq!(body["source"]["start_line"], 18);
    let read = run(
        &mut s,
        "code_outline",
        json!({"path":"Store.cs","kind":"accessor","container":"Demo.Core::Store::Value","query":"get","match":"exact"}),
    );
    let getter = run(
        &mut s,
        "symbol_read",
        json!({"path":"Store.cs","symbol_id":read["symbols"][0]["symbol_id"]}),
    );
    assert!(
        getter["content"]["text"]
            .as_str()
            .unwrap()
            .contains("get => count")
    );
    std::fs::write(dir.path().join("Store.cs"), format!("{source}\n")).unwrap();
    assert!(
        tools::execute(
            &mut s,
            "symbol_read",
            json!({"path":"Store.cs","symbol_id":method["symbol_id"]})
        )
        .unwrap_err()
        .to_string()
        .contains("revision_conflict")
    );
}

#[test]
fn csharp_block_namespaces_and_conditional_declarations_keep_scope() {
    let (dir, mut s) = setup();
    let source = "namespace Outer {\n namespace Inner { public class Store { public int Read() => 1; } }\n}\npublic class Outside {}\n#if DEBUG\nclass DebugType {}\n#else\nclass ReleaseType {}\n#endif\n";
    std::fs::write(dir.path().join("Scopes.cs"), source).unwrap();
    let outline = run(
        &mut s,
        "code_outline",
        json!({"path":"Scopes.cs","view":"compact"}),
    );
    assert_eq!(outline["has_parse_errors"], false);
    let symbols = outline["symbols"].as_array().unwrap();
    for (name, container) in [
        ("Inner", "Outer"),
        ("Store", "Outer::Inner"),
        ("Read", "Outer::Inner::Store"),
        ("Outside", ""),
        ("DebugType", ""),
        ("ReleaseType", ""),
    ] {
        assert!(
            symbols
                .iter()
                .any(|v| v["name"] == name && v["container"] == container),
            "{outline}"
        );
    }
    assert!(s.sources.is_empty());
    std::fs::write(
        dir.path().join("Scopes.cs"),
        "namespace Broken { class Store { void Load( { }",
    )
    .unwrap();
    let broken = run(&mut s, "code_outline", json!({"path":"Scopes.cs"}));
    assert_eq!(broken["has_parse_errors"], true);
}

#[test]
fn long_symbol_names_preserve_exact_matching_and_container_paths() {
    let (dir, mut s) = setup();
    let name = "N".repeat(510);
    for (path, source, container) in [
        (
            "Long.cs",
            format!("namespace {name}; class Store {{ public void Read() {{}} }}"),
            format!("{name}::Store"),
        ),
        (
            "long.rs",
            format!("mod {name} {{ fn read() {{}} }}"),
            name.clone(),
        ),
    ] {
        std::fs::write(dir.path().join(path), source).unwrap();
        let outline = run(
            &mut s,
            "code_outline",
            json!({"path":path,"query":name,"match":"exact"}),
        );
        assert_eq!(outline["symbols"].as_array().unwrap().len(), 1, "{outline}");
        assert_eq!(outline["symbols"][0]["qualified_name"], name);
        let members = run(
            &mut s,
            "code_outline",
            json!({"path":path,"container":container}),
        );
        assert_eq!(members["symbols"].as_array().unwrap().len(), 1, "{members}");
        assert_eq!(members["symbols"][0]["container"], container);
    }
}
