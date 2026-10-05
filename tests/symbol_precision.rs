mod support;
use mnemoarc::{
    config::{Config, Project},
    session::Session,
    tools,
};
use serde_json::{Value, json};

fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().unwrap();
    for (path, source) in files {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let session = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            ..support::compact_config()
        },
    );
    (dir, session)
}

fn run(s: &mut Session, name: &str, args: Value) -> Value {
    tools::execute(s, name, args.clone()).unwrap_or_else(|error| panic!("{name} {args}: {error}"))
}

fn symbol(s: &mut Session, path: &str, name: &str) -> Value {
    let result = run(
        s,
        "symbol_search",
        json!({"path":path,"query":name,"match":"exact","limit":100}),
    );
    assert_eq!(result["parse_error_count"], 0, "{result}");
    assert_eq!(result["total_results"], 1, "{result}");
    result["symbols"][0].clone()
}

fn relations(s: &mut Session, symbol: &Value, relation: &str) -> Value {
    run(
        s,
        "symbol_relations",
        json!({"path":symbol["path"],"symbol_id":symbol["symbol_id"],"relation":relation,"limit":100}),
    )
}

#[test]
fn completed_symbol_reads_do_not_offer_a_line_outside_the_symbol() {
    let (_dir, mut s) = setup(&[("a.rs", "fn first() {\n    let x = 1;\n}\nfn second() {}\n")]);
    let first = symbol(&mut s, "a.rs", "first");
    let read = run(
        &mut s,
        "symbol_read",
        json!({"path":first["path"],"symbol_id":first["symbol_id"]}),
    );
    assert_eq!(read["content"]["line_end"], first["end_line"]);
    assert_eq!(read["next_line"], Value::Null, "{read}");
    let head = run(
        &mut s,
        "symbol_read",
        json!({"path":first["path"],"symbol_id":first["symbol_id"],"max_lines":2}),
    );
    assert_eq!(head["next_line"], 3);
    let tail = run(
        &mut s,
        "symbol_read",
        json!({"path":first["path"],"symbol_id":first["symbol_id"],"start_line":3,"max_lines":20}),
    );
    assert_eq!(tail["content"]["text"], "}");
    assert_eq!(tail["next_line"], Value::Null);
}

#[test]
fn increment_and_decrement_writes_invalidate_function_bindings() {
    for (path, mutation) in [("a.js", "target++"), ("a.ts", "--target")] {
        let source = format!("function target() {{}} function main() {{ {mutation}; target(); }}");
        let (_dir, mut s) = setup(&[(path, &source)]);
        let main = symbol(&mut s, path, "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(calls["total_results"], 1, "{calls}");
        assert_eq!(calls["relations"][0]["resolution"], "unresolved", "{calls}");
        let target = symbol(&mut s, path, "target");
        assert_eq!(relations(&mut s, &target, "callers")["total_results"], 0);
    }
}

#[test]
fn parenthesized_callees_keep_calls_and_references_distinct() {
    for (path, source) in [
        (
            "a.js",
            "function target() {} function main() { ((target))(); }",
        ),
        (
            "a.ts",
            "function target() {} function main() { ((target))(); }",
        ),
        ("a.py", "def target(): pass\ndef main():\n ((target))()\n"),
        ("lib.rs", "fn target() {} fn main() { ((target))(); }"),
    ] {
        let (_dir, mut s) = setup(&[(path, source)]);
        let main = symbol(&mut s, path, "main");
        let target = symbol(&mut s, path, "target");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(
            calls["relations"][0]["resolution"], "resolved",
            "{path}: {calls}"
        );
        assert_eq!(calls["relations"][0]["name"], "target");
        let callers = relations(&mut s, &target, "callers");
        assert_eq!(callers["total_results"], 1, "{path}: {callers}");
        let references = relations(&mut s, &target, "references");
        assert_eq!(references["total_results"], 1, "{path}: {references}");
        assert_eq!(references["relations"][0]["kind"], "call");
    }
}

#[test]
fn csharp_nested_namespaces_search_and_read_use_the_same_container() {
    let (_dir, mut s) = setup(&[("A.cs", "namespace One.Two { class A { void Target() {} } }")]);
    let target = symbol(&mut s, "A.cs", "Target");
    let read = run(
        &mut s,
        "symbol_read",
        json!({"path":target["path"],"symbol_id":target["symbol_id"]}),
    );
    assert_eq!(read["symbol"]["container"], target["container"]);
    assert_eq!(target["container"], "One.Two::A");
}

#[test]
fn rust_generic_impl_members_are_candidates_of_the_declared_type() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "struct Store<T>(T); impl<T> Store<T> { fn target() {} } fn main() { Store::target(); }",
    )]);
    let main = symbol(&mut s, "lib.rs", "main");
    let target = symbol(&mut s, "lib.rs", "target");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["relations"][0]["resolution"], "candidate", "{calls}");
    assert_eq!(
        calls["relations"][0]["candidates"][0]["symbol_id"], target["symbol_id"],
        "{calls}"
    );
    assert_eq!(target["container"], "Store");
    let implementation = run(
        &mut s,
        "code_outline",
        json!({"path":"lib.rs","query":"Store","match":"exact","kind":"impl"}),
    );
    assert_eq!(implementation["total_symbols"], 1);
    assert!(
        implementation["symbols"][0]["signature"]
            .as_str()
            .unwrap()
            .contains("Store<T>")
    );
    let read = run(
        &mut s,
        "symbol_read",
        json!({"path":"lib.rs","symbol_id":target["symbol_id"]}),
    );
    assert_eq!(read["symbol"]["container"], target["container"]);
}

#[test]
fn local_class_field_initializers_are_not_calls_of_the_enclosing_function() {
    for (path, source, main_name) in [
        (
            "a.js",
            "function target() {} function main() { class Local { value = target(); } }",
            "main",
        ),
        (
            "A.java",
            "class A { static int target() { return 1; } void main() { class Local { int value = target(); } } }",
            "main",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, source)]);
        let main = symbol(&mut s, path, main_name);
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(calls["total_results"], 0, "{path}: {calls}");
        let field = symbol(&mut s, path, "value");
        let field_calls = relations(&mut s, &field, "calls");
        assert_eq!(field_calls["total_results"], 1, "{field_calls}");
        assert_eq!(
            field_calls["relations"][0]["enclosing_symbol"]["symbol_id"],
            field["symbol_id"]
        );
        let target = symbol(&mut s, path, "target");
        let callers = relations(&mut s, &target, "callers");
        assert_eq!(callers["total_results"], 1, "{callers}");
        assert_eq!(
            callers["relations"][0]["enclosing_symbol"]["symbol_id"],
            field["symbol_id"]
        );
    }
}

#[test]
fn typescript_namespace_symbols_preserve_search_read_and_relation_scope() {
    for path in ["a.ts", "a.tsx"] {
        let (_dir, mut s) = setup(&[(
            path,
            "namespace Store { export function target() {} } function main() { Store.target(); }",
        )]);
        let namespace = symbol(&mut s, path, "Store");
        assert_eq!(namespace["symbol_kind"], "module");
        let target = symbol(&mut s, path, "target");
        assert_eq!(target["container"], "Store");
        assert_eq!(target["depth"], 1);
        let top_level = run(
            &mut s,
            "symbol_search",
            json!({"path":path,"query":"target","match":"exact","max_depth":0}),
        );
        assert_eq!(top_level["total_results"], 0);
        let scoped = run(
            &mut s,
            "symbol_search",
            json!({"path":path,"query":"target","match":"exact","container":"Store"}),
        );
        assert_eq!(scoped["symbols"][0]["symbol_id"], target["symbol_id"]);
        let read = run(
            &mut s,
            "symbol_read",
            json!({"path":path,"symbol_id":target["symbol_id"]}),
        );
        assert_eq!(read["symbol"]["container"], target["container"]);
        let main = symbol(&mut s, path, "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(calls["relations"][0]["resolution"], "candidate", "{calls}");
        assert_eq!(
            calls["relations"][0]["candidates"][0]["symbol_id"],
            target["symbol_id"]
        );
    }
}

#[test]
fn updates_invalidate_imports_but_do_not_escape_a_closer_local_binding() {
    let (_dir, mut s) = setup(&[
        ("target.js", "export function target() {}"),
        (
            "main.js",
            "import {target} from './target.js'; function main() { if (true) { target++; } target(); }",
        ),
    ]);
    let main = symbol(&mut s, "main.js", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["relations"][0]["resolution"], "unresolved", "{calls}");
    let target = symbol(&mut s, "target.js", "target");
    assert_eq!(relations(&mut s, &target, "callers")["total_results"], 0);

    let (_dir, mut s) = setup(&[(
        "a.js",
        "function target() {} function main() { let target = 0; target++; } function control() { const object = {}; object.target++; -target; target(); }",
    )]);
    let found = run(
        &mut s,
        "symbol_search",
        json!({"path":"a.js","query":"target","match":"exact","kind":"function"}),
    );
    assert_eq!(found["total_results"], 1);
    let callers = relations(&mut s, &found["symbols"][0], "callers");
    assert_eq!(callers["total_results"], 1, "{callers}");
    assert_eq!(callers["relations"][0]["resolution"], "resolved");
}

#[test]
fn parentheses_preserve_member_candidates_without_guessing_dynamic_targets() {
    let (_dir, mut s) = setup(&[(
        "a.js",
        "class Store { static target() {} } function factory() { return Store.target; } function main() { ((Store.target))(); (factory())(); }",
    )]);
    let main = symbol(&mut s, "a.js", "main");
    let target = symbol(&mut s, "a.js", "target");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["total_results"], 3, "{calls}");
    assert_eq!(calls["relations"][0]["resolution"], "candidate");
    assert_eq!(calls["relations"][0]["expression"], "((Store.target))()");
    assert_eq!(calls["relations"][1]["resolution"], "unresolved");
    assert_eq!(calls["relations"][1]["reason"], "dynamic_callee_expression");
    assert_eq!(relations(&mut s, &target, "callers")["total_results"], 1);
}

#[test]
fn generic_specializations_remain_ambiguous_and_respect_local_type_shadowing() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "struct Store<T>(T); impl Store<u8> { fn target() {} } impl Store<u16> { fn target() {} } fn main() { Store::<u8>::target(); } fn local() { struct Store; Store::target(); }",
    )]);
    let main = symbol(&mut s, "lib.rs", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["relations"][0]["resolution"], "ambiguous", "{calls}");
    assert_eq!(calls["relations"][0]["candidate_count"], 2);
    assert_eq!(calls["relations"][0]["expression"], "Store::<u8>::target()");
    let local = symbol(&mut s, "lib.rs", "local");
    let calls = relations(&mut s, &local, "calls");
    assert_eq!(calls["relations"][0]["resolution"], "unresolved", "{calls}");
}

#[test]
fn computed_class_keys_stay_with_the_enclosing_function_and_callbacks_stay_separate() {
    let (_dir, mut s) = setup(&[(
        "a.js",
        "function target() { return 'value'; } function main() { class Local { [target()] = target(); callback = () => target(); } }",
    )]);
    let main = symbol(&mut s, "a.js", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["total_results"], 1, "{calls}");
    assert_eq!(
        calls["relations"][0]["enclosing_symbol"]["symbol_id"],
        main["symbol_id"]
    );
    let callback = symbol(&mut s, "a.js", "callback");
    let calls = relations(&mut s, &callback, "calls");
    assert_eq!(calls["total_results"], 1, "{calls}");
    assert_eq!(
        calls["relations"][0]["enclosing_symbol"]["symbol_id"],
        callback["symbol_id"]
    );
    let target = symbol(&mut s, "a.js", "target");
    assert_eq!(relations(&mut s, &target, "callers")["total_results"], 3);
}

#[test]
fn typescript_module_declarations_do_not_leak_members_into_outer_scope() {
    let (_dir, mut s) = setup(&[(
        "a.ts",
        "module Store { export function target() {} } function main() { Store.target(); target(); }",
    )]);
    let store = symbol(&mut s, "a.ts", "Store");
    assert_eq!(store["symbol_kind"], "module");
    let main = symbol(&mut s, "a.ts", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["total_results"], 2, "{calls}");
    assert_eq!(calls["relations"][0]["resolution"], "candidate");
    assert_eq!(calls["relations"][1]["resolution"], "unresolved");
}
