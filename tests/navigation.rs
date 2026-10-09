use crate::support;
use mnemoarc::{
    config::{Config, Project},
    llm::ToolCall,
    session::Session,
    tools::{self, ToolRegistry},
};
use serde_json::{Value, json};

fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().unwrap();
    for (path, content) in files {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
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
    tools::execute(s, name, args.clone()).unwrap_or_else(|e| panic!("{name} {args}: {e}"))
}
fn symbol(s: &mut Session, path: &str, query: &str) -> Value {
    let result = run(
        s,
        "symbol_search",
        json!({"path":path,"query":query,"match":"exact"}),
    );
    assert_eq!(result["symbols"].as_array().unwrap().len(), 1, "{result}");
    result["symbols"][0].clone()
}
fn relations(s: &mut Session, symbol: &Value, relation: &str) -> Value {
    run(
        s,
        "symbol_relations",
        json!({"path":symbol["path"],"symbol_id":symbol["symbol_id"],"relation":relation}),
    )
}

#[test]
fn workspace_search_uses_ast_for_all_languages_and_shared_symbol_ids() {
    let (_dir, mut s) = setup(&[
        (
            "a.rs",
            "// fn fake() {}\nstruct Store; impl Store { fn load(\n &self\n) {} }",
        ),
        (
            "a.ts",
            "// function fake() {}\nclass Store { load(\n value: number\n) {} }",
        ),
        (
            "a.jsx",
            "export const load = (value) => <div>{value}</div>;",
        ),
        (
            "a.py",
            "# def fake(): pass\nclass Store:\n def load(\n  self\n ):\n  pass\n",
        ),
        ("A.java", "class Store { void load(\n int value\n) {} }"),
        ("A.cs", "class Store { void load(\n int value\n) {} }"),
        ("ignored.txt", "function load() {}"),
    ]);
    let found = run(
        &mut s,
        "symbol_search",
        json!({"query":"load","match":"exact"}),
    );
    assert_eq!(found["symbols"].as_array().unwrap().len(), 6, "{found}");
    assert_eq!(found["engine"], "tree-sitter");
    assert_eq!(found["skipped_files"], 1);
    for symbol in found["symbols"].as_array().unwrap() {
        let body = run(
            &mut s,
            "symbol_read",
            json!({"path":symbol["path"],"symbol_id":symbol["symbol_id"]}),
        );
        assert!(body["content"]["text"].as_str().unwrap().contains("load"));
    }
    assert!(
        run(&mut s, "symbol_search", json!({"query":"fake"}))["symbols"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let methods = run(
        &mut s,
        "symbol_search",
        json!({"path_glob":"*.java","kind":"method","container":"Store"}),
    );
    assert_eq!(methods["symbols"][0]["name"], "load");
    assert!(
        tools::execute(
            &mut s,
            "symbol_search",
            json!({"path":"a.rs","path_glob":"*.rs"})
        )
        .is_err()
    );
}

#[test]
fn local_calls_references_and_nested_functions_are_distinguished() {
    let (_dir, mut s) = setup(&[(
        "a.js",
        r#"
function save() {}
function unused() {}
function main() {
  save();
  const callback = save;
  function inner() { unused(); }
}
"#,
    )]);
    let main = symbol(&mut s, "a.js", "main");
    let save = symbol(&mut s, "a.js", "save");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["relations"].as_array().unwrap().len(), 1, "{calls}");
    assert_eq!(calls["relations"][0]["name"], "save");
    assert_eq!(calls["relations"][0]["resolution"], "resolved");
    let callers = relations(&mut s, &save, "callers");
    assert_eq!(
        callers["relations"].as_array().unwrap().len(),
        1,
        "{callers}"
    );
    let references = relations(&mut s, &save, "references");
    assert_eq!(
        references["relations"].as_array().unwrap().len(),
        2,
        "{references}"
    );
    assert_eq!(references["relations"][1]["kind"], "reference");
    assert_eq!(references["complete_call_graph"], false);
}

#[test]
fn parameters_and_local_assignments_never_bind_to_same_named_functions() {
    for (path, code, main) in [
        (
            "a.js",
            "function save() {}\nfunction main(save) { save(); }",
            "main",
        ),
        (
            "a.py",
            "def save(): pass\ndef main(save):\n save()\n",
            "main",
        ),
        (
            "a.rs",
            "fn save() {} fn main(save: fn()) { save(); }",
            "main",
        ),
        (
            "a.ts",
            "function save() {}\nfunction main(save: () => void) { save(); }",
            "main",
        ),
        (
            "a.js",
            "function save() {}\nfunction main() { let save = factory(); save(); }",
            "main",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, code), ("unrelated.js", "function save() {}")]);
        let target = symbol(&mut s, path, main);
        let calls = relations(&mut s, &target, "calls");
        let save = calls["relations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "save")
            .unwrap();
        assert_ne!(save["resolution"], "resolved", "{path}: {calls}");
        assert!(
            save["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c["symbol_kind"] != "function"),
            "{path}: {calls}"
        );
    }
}

#[test]
fn javascript_explicit_import_alias_and_namespace_link_only_the_exported_file() {
    let (_dir, mut s) = setup(&[
        (
            "a.ts",
            "import { save as persist } from './store'; import * as repo from './store';\nexport function main() { persist(); repo.save(); }",
        ),
        ("store.ts", "export function save() {}"),
        ("other.ts", "export function save() {}"),
    ]);
    let main = symbol(&mut s, "a.ts", "main");
    let store = symbol(&mut s, "store.ts", "save");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["relations"].as_array().unwrap().len(), 2, "{calls}");
    for row in calls["relations"].as_array().unwrap() {
        assert_eq!(row["resolution"], "candidate", "{calls}");
        assert_eq!(row["candidates"][0]["path"], "store.ts", "{calls}");
        assert_eq!(row["candidate_count"], 1);
    }
    assert_eq!(relations(&mut s, &store, "callers")["total_results"], 2);
    let other = symbol(&mut s, "other.ts", "save");
    assert_eq!(relations(&mut s, &other, "callers")["total_results"], 0);
}

#[test]
fn python_and_rust_import_aliases_are_traced_as_candidates() {
    for (path, source, dep, dependency) in [
        (
            "main.py",
            "from .store import save as persist\ndef main():\n persist()\n",
            "store.py",
            "def save(): pass",
        ),
        (
            "src/lib.rs",
            "mod store; use crate::store::{save as persist}; fn main() { persist(); }",
            "src/store.rs",
            "pub fn save() {}",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, source), (dep, dependency)]);
        let main = symbol(&mut s, path, "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(
            calls["relations"][0]["resolution"], "candidate",
            "{path}: {calls}"
        );
        assert_eq!(
            calls["relations"][0]["candidates"][0]["path"], dep,
            "{path}: {calls}"
        );
    }
}

#[test]
fn java_imports_and_csharp_aliases_keep_overloads_ambiguous() {
    for (path, source, dep, dependency) in [
        (
            "Main.java",
            "package app; import repo.Store; class Main { void run() { Store.save(1); } }",
            "Store.java",
            "package repo; public class Store { public static void save(int n) {} public static void save(String n) {} }",
        ),
        (
            "Main.cs",
            "using Repo = Data.Store; class Main { void run() { Repo.save(1); } }",
            "Store.cs",
            "namespace Data; class Store { public static void save(int n) {} public static void save(string n) {} }",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, source), (dep, dependency)]);
        let main = symbol(&mut s, path, "run");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(
            calls["relations"][0]["resolution"], "ambiguous",
            "{path}: {calls}"
        );
        assert_eq!(
            calls["relations"][0]["candidate_count"], 2,
            "{path}: {calls}"
        );
    }
}

#[test]
fn unknown_receivers_and_external_modules_do_not_guess_targets() {
    let (_dir, mut s) = setup(&[
        (
            "main.ts",
            "import { save } from 'external'; class Store { save() {} } function main(obj) { obj.save(); save(); }",
        ),
        ("elsewhere.ts", "export function save() {}"),
    ]);
    let main = symbol(&mut s, "main.ts", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["total_results"], 2);
    for row in calls["relations"].as_array().unwrap() {
        assert_eq!(row["resolution"], "unresolved", "{calls}");
        assert_eq!(row["candidate_count"], 0);
    }
}

#[test]
fn hashes_filters_permissions_and_new_files_invalidate_navigation() {
    let (dir, mut s) = setup(&[
        (
            "a.js",
            "function target() {} function main() { target(); target(); }",
        ),
        ("secret.js", "function hidden() {}"),
    ]);
    let target = symbol(&mut s, "a.js", "target");
    let args =
        json!({"path":"a.js","symbol_id":target["symbol_id"],"relation":"callers","limit":1});
    let first = run(&mut s, "symbol_relations", args.clone());
    assert!(first["next_cursor"].is_string());
    let mut next = args.clone();
    next["cursor"] = first["next_cursor"].clone();
    assert_eq!(
        run(&mut s, "symbol_relations", next.clone())["page_start"],
        1
    );
    let mut changed = next.clone();
    changed["relation"] = json!("references");
    assert!(tools::execute(&mut s, "symbol_relations", changed).is_err());
    std::fs::write(dir.path().join("new.js"), "function extra() {}").unwrap();
    assert!(
        tools::execute(&mut s, "symbol_relations", next)
            .unwrap_err()
            .to_string()
            .contains("cursor_expired")
    );
    std::fs::write(dir.path().join("a.js"), "function target() {}\n").unwrap();
    assert!(
        tools::execute(&mut s, "symbol_relations", args)
            .unwrap_err()
            .to_string()
            .contains("symbol_revision_conflict")
    );
    s.project.exclude.push("secret.js".into());
    assert_eq!(
        run(&mut s, "symbol_search", json!({"query":"hidden"}))["total_results"],
        0
    );
    assert!(
        tools::execute(
            &mut s,
            "symbol_relations",
            json!({"path":"secret.js","symbol_id":target["symbol_id"]})
        )
        .is_err()
    );
}

#[test]
fn relations_are_navigation_and_obey_checkpoint_and_closing_tool_filters() {
    let (_dir, mut s) = setup(&[("a.js", "function save() {} function main() { save(); }")]);
    let main = symbol(&mut s, "a.js", "main");
    let source_count = s.sources.len();
    let result = relations(&mut s, &main, "calls");
    assert_eq!(s.sources.len(), source_count);
    assert!(s.read_coverage.is_empty());
    assert_eq!(result["navigation_only"], true);
    assert!(ToolRegistry::closing_blocked("symbol_relations"));
    assert!(
        ToolRegistry::definitions(&s)
            .iter()
            .any(|d| d["function"]["name"] == "symbol_relations")
    );
    let edit = ToolRegistry::specs()
        .into_iter()
        .find(|t| t.name == "document_edit")
        .unwrap();
    assert!(
        !edit
            .description
            .contains("patch.require_investigation=true")
    );
}

#[test]
fn result_budget_continuation_does_not_skip_relation_or_search_rows() {
    let source = format!(
        "function save() {{}} function main() {{ {} }}",
        "save();".repeat(20)
    );
    let (_dir, mut s) = setup(&[("a.js", &source)]);
    let main = symbol(&mut s, "a.js", "main");
    let mut args =
        json!({"path":"a.js","symbol_id":main["symbol_id"],"relation":"calls","limit":20});
    let mut count = 0;
    loop {
        let call = ToolCall {
            id: format!("page{count}"),
            name: "symbol_relations".into(),
            arguments: args.to_string(),
        };
        let result = tools::run_call(&mut s, &call);
        let limited = tools::limit_result(&mut s, &call, result, 2200);
        let rows = limited["data"]["relations"].as_array().unwrap();
        assert!(!rows.is_empty(), "{limited}");
        assert_eq!(limited["data"]["page_start"], count);
        count += rows.len();
        if limited["data"]["next_cursor"].is_null() {
            break;
        }
        assert_eq!(limited["next_cursor"]["tool"], "symbol_relations");
        args = limited["next_cursor"].clone();
        args.as_object_mut().unwrap().remove("tool");
    }
    assert_eq!(count, 20);
}

#[test]
fn parse_errors_unicode_and_cancellation_are_explicit() {
    let (_dir, mut s) = setup(&[
        ("a.js", "function 저장() {}\nfunction main() { 저장(); }"),
        ("broken.js", "function broken( {"),
    ]);
    let main = symbol(&mut s, "a.js", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["parse_error_count"], 1);
    assert_eq!(calls["relations"][0]["name"], "저장");
    assert_eq!(calls["relations"][0]["name_column"], 19);
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    assert!(
        tools::execute_cancellable(&mut s, "symbol_search", json!({}), &cancel)
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
}

#[test]
fn nested_declarations_shadow_imports_and_callbacks_keep_their_own_call_scope() {
    let (_dir, mut s) = setup(&[
        (
            "a.js",
            "import { save } from './dep.js'; function main() { function save() {} save(); items.map(() => save()); }",
        ),
        ("dep.js", "export function save() {}"),
    ]);
    let main = symbol(&mut s, "a.js", "main");
    let local = symbol(&mut s, "a.js", "save");
    let calls = relations(&mut s, &main, "calls");
    let save = calls["relations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["name"] == "save")
        .collect::<Vec<_>>();
    assert_eq!(save.len(), 1, "{calls}");
    assert_eq!(save[0]["resolution"], "resolved");
    assert_eq!(save[0]["candidates"][0]["symbol_id"], local["symbol_id"]);
    let callers = relations(&mut s, &local, "callers");
    assert_eq!(callers["total_results"], 2, "{callers}");
    assert_eq!(
        callers["relations"][1]["enclosing_callable"]["anonymous"],
        true
    );
    let imported = symbol(&mut s, "dep.js", "save");
    assert_eq!(relations(&mut s, &imported, "callers")["total_results"], 0);
}

#[test]
fn function_scoped_bindings_do_not_leak_false_local_resolutions() {
    for (path, source) in [
        (
            "a.py",
            "def save(): pass\ndef main():\n if condition:\n  save = make()\n save()\n",
        ),
        (
            "a.js",
            "function save() {} function main() { if (condition) { var save = make(); } save(); }",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, source)]);
        let main = symbol(&mut s, path, "main");
        let calls = relations(&mut s, &main, "calls");
        let save = calls["relations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "save")
            .unwrap();
        assert_ne!(save["resolution"], "resolved", "{calls}");
        assert!(
            save["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .all(|candidate| candidate["container"] == "main"),
            "{calls}"
        );
    }
}

#[test]
fn rust_sibling_module_calls_and_callback_references_have_locations() {
    let (_dir, mut s) = setup(&[
        (
            "src/lib.rs",
            "mod store; fn main() { store::save(); let callback = store::save; }",
        ),
        ("src/store.rs", "pub fn save() {}"),
    ]);
    let main = symbol(&mut s, "src/lib.rs", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(
        calls["relations"][0]["candidates"][0]["path"], "src/store.rs",
        "{calls}"
    );
    let save = symbol(&mut s, "src/store.rs", "save");
    let refs = relations(&mut s, &save, "references");
    assert_eq!(refs["total_results"], 2, "{refs}");
}

#[test]
fn incomplete_scopes_and_parse_errors_do_not_produce_confirmed_links() {
    let (dir, mut s) = setup(&[
        (
            "main.js",
            "import {save} from './dep'; function main() { save(); }",
        ),
        ("dep.js", "export function save() {}"),
    ]);
    let main = symbol(&mut s, "main.js", "main");
    let scoped = run(
        &mut s,
        "symbol_relations",
        json!({"path":"main.js","symbol_id":main["symbol_id"],"path_glob":"main.js"}),
    );
    assert_eq!(scoped["scanned_files"], 1);
    assert_eq!(scoped["relations"][0]["resolution"], "unresolved");
    std::fs::write(
        dir.path().join("dep.js"),
        "export function save() {}\nfunction broken( {",
    )
    .unwrap();
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["parse_error_count"], 1);
    assert_eq!(calls["relations"][0]["resolution"], "unresolved");
}

#[test]
fn source_search_pages_expire_on_filter_and_dependency_changes() {
    let (dir, mut s) = setup(&[
        ("a.js", "function one() {} function two() {}"),
        ("b.js", "function three() {}"),
    ]);
    let first = run(
        &mut s,
        "symbol_search",
        json!({"kind":"function","limit":1}),
    );
    let args = json!({"kind":"function","limit":1,"cursor":first["next_cursor"]});
    assert_eq!(
        run(&mut s, "symbol_search", args.clone())["symbols"][0]["name"],
        "two"
    );
    let mut changed = args.clone();
    changed["case_sensitive"] = json!(true);
    assert!(tools::execute(&mut s, "symbol_search", changed).is_err());
    std::fs::write(dir.path().join("b.js"), "function changed() {}").unwrap();
    assert!(tools::execute(&mut s, "symbol_search", args).is_err());
}

#[cfg(unix)]
#[test]
fn workspace_navigation_never_follows_external_symlinks() {
    let (_dir, mut s) = setup(&[("main.js", "function main() {}")]);
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("external.js"), "function hidden() {}").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("external.js"),
        s.project.root.join("linked.js"),
    )
    .unwrap();
    assert_eq!(
        run(&mut s, "symbol_search", json!({"query":"hidden"}))["total_results"],
        0
    );
    assert!(tools::execute(&mut s, "symbol_search", json!({"path":"linked.js"})).is_err());
}

#[test]
fn imported_class_members_never_link_to_same_named_module_functions() {
    for (path, source, dependency, code) in [
        (
            "a.ts",
            "import {Store as Repo} from './store'; function main() { Repo.save(); }",
            "store.ts",
            "export class Store { static save() {} } export function save() {}",
        ),
        (
            "a.py",
            "from .store import Store as Repo\ndef main():\n Repo.save()\n",
            "store.py",
            "class Store:\n def save(self): pass\ndef save(): pass\n",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, source), (dependency, code)]);
        let main = symbol(&mut s, path, "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(calls["relations"][0]["candidate_count"], 1, "{calls}");
        assert_eq!(
            calls["relations"][0]["candidates"][0]["container"], "Store",
            "{calls}"
        );
    }
}

#[test]
fn csharp_namespace_and_static_imports_keep_dispatch_as_candidates() {
    for source in [
        "using Data; class Main { void run() { Store.Save(); } }",
        "namespace Data; class Main { void run() { Store.Save(); } }",
        "using static Data.Store; class Main { void run() { Save(); } }",
    ] {
        let (_dir, mut s) = setup(&[
            ("Main.cs", source),
            (
                "Store.cs",
                "namespace Data; class Store { public static void Save() {} }",
            ),
        ]);
        let main = symbol(&mut s, "Main.cs", "run");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(calls["relations"][0]["resolution"], "candidate", "{calls}");
        assert_eq!(calls["relations"][0]["candidates"][0]["path"], "Store.cs");
    }
}

#[test]
fn computed_calls_are_returned_as_unresolved_with_precise_name_coordinates() {
    let (_dir, mut s) = setup(&[("a.js", "function main(obj, key) {\n obj[\n key\n ]();\n}")]);
    let main = symbol(&mut s, "a.js", "main");
    let result = relations(&mut s, &main, "calls");
    assert_eq!(result["total_results"], 1, "{result}");
    assert_eq!(
        result["relations"][0]["reason"],
        "dynamic_callee_expression"
    );
    assert_eq!(result["relations"][0]["name_line"], 2);
    assert_eq!(result["relations"][0]["name_column"], 2);
    assert_eq!(result["relations"][0]["resolution"], "unresolved");
}

#[test]
fn budgeted_symbol_search_pages_keep_ids_and_advance_without_gaps() {
    let source = (0..24)
        .map(|i| format!("function target{i}() {{}}\n"))
        .collect::<String>();
    let (_dir, mut s) = setup(&[("a.js", &source)]);
    let mut args = json!({"path_glob":"*.js","query":"target","kind":"function","limit":24});
    let mut names = Vec::new();
    loop {
        let call = ToolCall {
            id: format!("search{}", names.len()),
            name: "symbol_search".into(),
            arguments: args.to_string(),
        };
        let result = tools::run_call(&mut s, &call);
        let page = tools::limit_result(&mut s, &call, result, 1800);
        assert_eq!(page["data"]["page_start"], names.len());
        let rows = page["data"]["symbols"].as_array().unwrap();
        assert!(!rows.is_empty());
        names.extend(rows.iter().map(|s| s["name"].as_str().unwrap().to_owned()));
        if page["data"]["next_cursor"].is_null() {
            break;
        }
        assert_eq!(page["next_cursor"]["tool"], "symbol_search");
        args = page["next_cursor"].clone();
        args.as_object_mut().unwrap().remove("tool");
    }
    assert_eq!(
        names,
        (0..24).map(|i| format!("target{i}")).collect::<Vec<_>>()
    );
    assert!(s.read_coverage.is_empty());
}

#[test]
fn unqualified_calls_do_not_treat_python_or_javascript_members_as_local_functions() {
    for (path, source) in [
        (
            "a.py",
            "def save(): pass\nclass Store:\n def save(self): pass\n def run(self):\n  save()\n",
        ),
        (
            "a.js",
            "function save() {} class Store { save() {} run() { save(); } }",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, source)]);
        let main = symbol(&mut s, path, "run");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(calls["relations"][0]["resolution"], "resolved", "{calls}");
        assert_eq!(
            calls["relations"][0]["candidates"][0]["container"], "",
            "{calls}"
        );
    }
}

#[test]
fn wildcard_imports_never_confirm_a_runtime_binding() {
    let (_dir, mut s) = setup(&[(
        "a.py",
        "def save(): pass\nfrom external import *\ndef main():\n save()\n",
    )]);
    let main = symbol(&mut s, "a.py", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["relations"][0]["resolution"], "candidate", "{calls}");
    assert_eq!(
        calls["relations"][0]["reason"],
        "wildcard_import_may_change_binding"
    );
}

#[test]
fn destructured_and_single_arrow_parameters_do_not_link_to_outer_functions() {
    for (path, source) in [
        (
            "a.js",
            "function save() {} function main({save}) { save(); }",
        ),
        ("a.js", "function save() {} const main = save => save();"),
        (
            "a.js",
            "function save() {} function main(save = null) { save(); }",
        ),
        (
            "a.ts",
            "function save() {} function main({save}: Options) { save(); }",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, source)]);
        let main = symbol(&mut s, path, "main");
        let result = relations(&mut s, &main, "calls");
        assert_eq!(result["total_results"], 1, "{source}: {result}");
        assert_eq!(
            result["relations"][0]["resolution"], "unresolved",
            "{source}: {result}"
        );
    }
}

#[test]
fn destructuring_default_expressions_and_keys_are_not_bindings() {
    for (path, source) in [
        (
            "a.js",
            "function save() {} function main({cb = save, save: alias}) { save(); }",
        ),
        (
            "a.ts",
            "function save() {} function main({cb = save, save: alias}: Options) { save(); }",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, source)]);
        let save = symbol(&mut s, path, "save");
        let result = relations(&mut s, &save, "references");
        assert_eq!(result["total_results"], 2, "{source}: {result}");
    }
}

#[test]
fn named_expressions_keep_their_private_names_inside_the_expression() {
    let (_dir, mut s) = setup(&[(
        "a.js",
        "function save() {} const wrapper = function save() { save(); }; const Box = class Hidden {}; function main() { Hidden(); save(); }",
    )]);
    let wrapper = symbol(&mut s, "a.js", "wrapper");
    let calls = relations(&mut s, &wrapper, "calls");
    assert_eq!(calls["relations"][0]["resolution"], "unresolved", "{calls}");
    let main = symbol(&mut s, "a.js", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["relations"][0]["resolution"], "unresolved", "{calls}");
    assert_eq!(calls["relations"][1]["resolution"], "resolved", "{calls}");
}

#[test]
fn python_module_imports_preserve_dotted_paths_and_relative_module_members() {
    let (_dir, mut s) = setup(&[
        (
            "pkg/main.py",
            "import pkg.store\nfrom . import store as repo\ndef main():\n pkg.save()\n pkg.store.save()\n repo.save()\n",
        ),
        ("pkg/store.py", "def save(): pass\n"),
        ("pkg.py", "class store:\n def save(self): pass\n"),
    ]);
    let main = symbol(&mut s, "pkg/main.py", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["relations"][0]["resolution"], "unresolved", "{calls}");
    for call in &calls["relations"].as_array().unwrap()[1..] {
        assert_eq!(call["candidate_count"], 1, "{calls}");
        assert_eq!(call["candidates"][0]["path"], "pkg/store.py", "{calls}");
    }
}

#[test]
fn rust_module_boundaries_and_pattern_bindings_do_not_inherit_unrelated_names() {
    for source in [
        "fn save() {} mod child { fn main() { save(); } }",
        "fn save() {} fn main(value: Option<fn()>) { if let Some(save) = value { save(); } }",
        "fn save() {} fn main(value: Option<fn()>) { match value { Some(save) => save(), _ => {} } }",
    ] {
        let (_dir, mut s) = setup(&[("lib.rs", source)]);
        let main = symbol(&mut s, "lib.rs", "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(
            calls["relations"][0]["resolution"], "unresolved",
            "{source}: {calls}"
        );
    }
}

#[test]
fn python_comprehension_and_context_manager_targets_shadow_outer_functions() {
    for source in [
        "def save(): pass\ndef main(callbacks):\n return [save() for save in callbacks]\n",
        "def save(): pass\ndef main(context):\n with context as save:\n  save()\n",
        "def save(): pass\ndef main():\n try: pass\n except Exception as save: save()\n",
    ] {
        let (_dir, mut s) = setup(&[("a.py", source)]);
        let main = symbol(&mut s, "a.py", "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(
            calls["relations"][0]["resolution"], "unresolved",
            "{source}: {calls}"
        );
    }
}

#[test]
fn export_default_is_recognized_across_whitespace_and_identifier_exports() {
    for source in [
        "export default\nfunction save() {}",
        "export default/*comment*/function save() {}",
        "function save() {} export default save;",
    ] {
        let (_dir, mut s) = setup(&[
            (
                "a.js",
                "import save from './store.js'; function main() { save(); }",
            ),
            ("store.js", source),
        ]);
        let main = symbol(&mut s, "a.js", "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(
            calls["relations"][0]["resolution"], "candidate",
            "{source}: {calls}"
        );
        assert_eq!(
            calls["relations"][0]["candidates"][0]["name"], "save",
            "{calls}"
        );
    }
}

#[cfg(unix)]
#[test]
fn navigation_paths_round_trip_with_literal_backslashes() {
    let (_dir, mut s) = setup(&[("literal\\name.js", "function main() {}")]);
    let main = symbol(&mut s, "literal\\name.js", "main");
    assert_eq!(main["path"], "literal\\name.js");
    let result = relations(&mut s, &main, "calls");
    assert_eq!(result["total_results"], 0);
}

#[test]
fn exact_search_handles_files_larger_than_the_unfiltered_outline_limit() {
    let mut source = (0..20_001)
        .map(|i| format!("function item{i}() {{}}\n"))
        .collect::<String>();
    source.push_str("function wanted() {}\n");
    let (_dir, mut s) = setup(&[("generated.js", &source)]);
    let selected = symbol(&mut s, "generated.js", "wanted");
    let read = run(
        &mut s,
        "symbol_read",
        json!({"path": selected["path"], "symbol_id": selected["symbol_id"]}),
    );
    assert_eq!(
        read["content"]["text"].as_str().unwrap().trim(),
        "function wanted() {}"
    );
}

#[test]
fn catch_loop_and_lambda_bindings_follow_each_languages_name_rules() {
    for (path, source, container, expected_calls) in [
        (
            "A.java",
            "class A { void save() {} void main() { Object f = (save) -> save(); for(Object save : funcs) { save(); } try {} catch(Exception save) { save(); } } }",
            "A",
            3,
        ),
        (
            "A.cs",
            "class A { void save() {} void main() { System.Action<System.Action> f = save => save(); foreach(var save in funcs) { save(); } try {} catch(Exception save) { save(); } } }",
            "A",
            0,
        ),
        (
            "a.rs",
            "fn save() {} fn main(value: Option<fn()>) { match value { Some(save) => save(), _ => {} } }",
            "",
            0,
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, source)]);
        let save = symbol(&mut s, path, "save");
        assert_eq!(save["container"], container);
        let calls = relations(&mut s, &save, "callers");
        assert_eq!(calls["parse_error_count"], 0, "{calls}");
        assert_eq!(calls["total_results"], expected_calls, "{source}: {calls}");
    }
}

#[test]
fn loop_and_pattern_bindings_end_at_their_lexical_scope() {
    for (path, source) in [
        (
            "a.js",
            "function save() {} function main(funcs) { for (let save of funcs) { save(); } save(); }",
        ),
        (
            "a.rs",
            "fn save() {} fn main(value: Option<fn()>) { if let Some(save) = value { save(); } else { save(); } }",
        ),
        (
            "a.py",
            "def save(): pass\ndef main(funcs):\n values = [save() for save in funcs]\n save()\n",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, source)]);
        let main = symbol(&mut s, path, "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(calls["total_results"], 2, "{source}: {calls}");
        assert_eq!(
            calls["relations"][0]["resolution"], "unresolved",
            "{source}: {calls}"
        );
        assert_eq!(
            calls["relations"][1]["resolution"], "resolved",
            "{source}: {calls}"
        );
    }
}

#[test]
fn function_scoped_loop_bindings_remain_visible_after_the_loop() {
    for source in [
        "function save() {} function main(funcs) { for (var save of funcs) { save(); } save(); }",
        "function save() {} function main(funcs) { for (save of funcs) { save(); } save(); }",
    ] {
        let (_dir, mut s) = setup(&[("a.js", source)]);
        let main = symbol(&mut s, "a.js", "main");
        let calls = relations(&mut s, &main, "calls");
        assert!(
            calls["relations"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["resolution"] == "unresolved"),
            "{source}: {calls}"
        );
    }
}

#[test]
fn python_conditional_definitions_and_imports_have_function_scope() {
    for source in [
        "def save(): pass\ndef main():\n if cond:\n  def save(): pass\n save()\n",
        "def save(): pass\ndef main():\n if cond:\n  from external import save\n save()\n",
    ] {
        let (_dir, mut s) = setup(&[("a.py", source)]);
        let main = symbol(&mut s, "a.py", "main");
        let calls = relations(&mut s, &main, "calls");
        assert!(
            calls["relations"][0]["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c["container"] == "main"),
            "{source}: {calls}"
        );
    }
}

#[test]
fn assignments_in_nested_blocks_and_closures_invalidate_outer_targets() {
    for (path, source) in [
        (
            "a.js",
            "function save() {} function main(other) { if (condition) { save = other; } save(); }",
        ),
        (
            "a.ts",
            "import {save} from './dep'; function main(other) { if (condition) { save = other; } save(); }",
        ),
        (
            "a.js",
            "function save() {} function change(other) { save = other; } function main() { save(); }",
        ),
        (
            "a.js",
            "function save() {} function main(other) { const change = () => { save = other; }; save(); }",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, source), ("dep.ts", "export function save() {}")]);
        let main = symbol(&mut s, path, "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(
            calls["relations"][0]["resolution"], "unresolved",
            "{source}: {calls}"
        );
    }
}

#[test]
fn rust_self_paths_use_the_enclosing_impl_as_a_candidate() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "struct Store; impl Store { fn save() {} fn main() { Self::save(); } }",
    )]);
    let main = symbol(&mut s, "lib.rs", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["relations"][0]["resolution"], "candidate", "{calls}");
    assert_eq!(
        calls["relations"][0]["candidates"][0]["qualified_name"], "Store::save",
        "{calls}"
    );
}

#[test]
fn python_global_and_nonlocal_mutations_are_not_confirmed_as_local_bindings() {
    for source in [
        "def save(): pass\ndef change(other):\n global save\n save = other\ndef main():\n save()\n",
        "def main(other):\n def save(): pass\n def change():\n  nonlocal save\n  save = other\n save()\n",
    ] {
        let (_dir, mut s) = setup(&[("a.py", source)]);
        let main = symbol(&mut s, "a.py", "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(
            calls["relations"][0]["resolution"], "unresolved",
            "{source}: {calls}"
        );
        assert_eq!(
            calls["relations"][0]["reason"], "global_or_nonlocal_binding_not_resolved",
            "{calls}"
        );
    }
}

#[test]
fn target_parse_errors_are_reported_without_inconsistent_candidate_counts() {
    let (_dir, mut s) = setup(&[
        (
            "a.ts",
            "import {save} from './store.js'; function main() { save(); }",
        ),
        ("store.js", "export function save() {}"),
        ("store.ts", "export function save() {} !!!"),
    ]);
    let main = symbol(&mut s, "a.ts", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["parse_error_count"], 1, "{calls}");
    assert_eq!(calls["relations"][0]["candidate_count"], 1, "{calls}");
    assert_eq!(calls["relations"][0]["resolution"], "candidate", "{calls}");
}

#[test]
fn python_assignment_expressions_deletions_and_match_captures_shadow_outer_names() {
    for body in [
        " (save := value)\n save()\n",
        " del save\n save()\n",
        " match value:\n  case save:\n   save()\n",
        " match value:\n  case [save]:\n   save()\n",
    ] {
        let code = format!("def save(): pass\ndef main(value):\n{body}");
        let (_dir, mut s) = setup(&[("a.py", &code)]);
        let main = symbol(&mut s, "a.py", "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(calls["parse_error_count"], 0, "{calls}");
        assert_eq!(
            calls["relations"][0]["resolution"], "unresolved",
            "{code}: {calls}"
        );
    }
}

#[test]
fn python_match_keys_and_dotted_values_are_not_bindings_or_bare_references() {
    for pattern in [
        "Box(save=callback)",
        "constants.save",
        "{constants.save: callback}",
    ] {
        let code = format!(
            "def save(): pass\ndef main(value):\n match value:\n  case {pattern}: pass\n save()\n"
        );
        let (_dir, mut s) = setup(&[("a.py", &code)]);
        let save = symbol(&mut s, "a.py", "save");
        let refs = relations(&mut s, &save, "references");
        assert_eq!(refs["parse_error_count"], 0, "{refs}");
        assert_eq!(refs["total_results"], 1, "{code}: {refs}");
        assert_eq!(refs["relations"][0]["kind"], "call", "{refs}");
    }
}

#[test]
fn python_class_bindings_do_not_leak_into_method_or_nested_class_scopes() {
    for class_body in [
        " class save: pass\n def main(self):\n  save()\n",
        " from dep import save\n def main(self):\n  save()\n",
        " save = None\n def main(self):\n  save()\n",
        " class save: pass\n class Nested:\n  def main(self):\n   save()\n",
    ] {
        let code = format!("def save(): pass\nclass Outer:\n{class_body}");
        let (_dir, mut s) = setup(&[("a.py", &code), ("dep.py", "def save(): pass\n")]);
        let main = symbol(&mut s, "a.py", "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(calls["parse_error_count"], 0, "{calls}");
        assert_eq!(
            calls["relations"][0]["resolution"], "resolved",
            "{code}: {calls}"
        );
        assert_eq!(
            calls["relations"][0]["candidates"][0]["path"], "a.py",
            "{calls}"
        );
        assert_eq!(
            calls["relations"][0]["candidates"][0]["container"], "",
            "{calls}"
        );
    }
}

#[test]
fn python_defaults_and_first_comprehension_iterables_use_the_outer_scope() {
    for code in [
        "def save(): pass\ndef main(save=save()):\n save()\n",
        "def save(): pass\ndef main():\n return [save() for save in save()]\n",
        "def save(): pass\ndef main():\n return (save() for save in save())\n",
    ] {
        let (_dir, mut s) = setup(&[("a.py", code)]);
        let save = symbol(&mut s, "a.py", "save");
        let calls = relations(&mut s, &save, "callers");
        assert_eq!(calls["total_results"], 1, "{code}: {calls}");
        assert_eq!(calls["relations"][0]["resolution"], "resolved", "{calls}");
    }
}

#[test]
fn rust_bindings_do_not_shadow_their_own_initializer_or_for_iterable() {
    for code in [
        "fn save() {} fn main() { let save = save(); save(); }",
        "fn save() {} fn main() { for save in save() { save(); } }",
    ] {
        let (_dir, mut s) = setup(&[("lib.rs", code)]);
        let main = symbol(&mut s, "lib.rs", "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(calls["total_results"], 2, "{calls}");
        assert_eq!(
            calls["relations"][0]["resolution"], "resolved",
            "{code}: {calls}"
        );
        assert_eq!(
            calls["relations"][1]["resolution"], "unresolved",
            "{code}: {calls}"
        );
    }
}

#[test]
fn shorthand_values_are_references_but_rust_field_labels_are_not() {
    for (path, code) in [
        ("a.js", "function save() {} const exported = {save};"),
        ("a.ts", "function save() {} const exported = {save};"),
    ] {
        let (_dir, mut s) = setup(&[(path, code)]);
        let save = symbol(&mut s, path, "save");
        let refs = relations(&mut s, &save, "references");
        assert_eq!(refs["total_results"], 1, "{code}: {refs}");
        assert_eq!(refs["relations"][0]["kind"], "reference");
    }
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "fn save() {} fn main(value: Item) { let Item { save: callback } = value; let item = Item { save: callback }; }",
    )]);
    let save = symbol(&mut s, "lib.rs", "save");
    let refs = relations(&mut s, &save, "references");
    assert_eq!(refs["total_results"], 0, "{refs}");
}

#[test]
fn csharp_anonymous_delegates_have_separate_call_owners() {
    let (_dir, mut s) = setup(&[(
        "A.cs",
        "class A { void Save() {} void Main() { System.Action action = delegate() { Save(); }; } }",
    )]);
    let main = symbol(&mut s, "A.cs", "Main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["total_results"], 0, "{calls}");
    let save = symbol(&mut s, "A.cs", "Save");
    let callers = relations(&mut s, &save, "callers");
    assert_eq!(callers["total_results"], 1, "{callers}");
    assert_eq!(
        callers["relations"][0]["enclosing_callable"]["anonymous"], true,
        "{callers}"
    );
}

#[test]
fn python_class_defaults_and_bodies_can_still_use_class_bindings() {
    for (body, expected_calls) in [
        (" def save(): pass\n value = save()\n", 1),
        (" def save(): pass\n def run(value=save()): pass\n", 1),
        (
            " def save(): pass\n values = [value for value in save()]\n",
            1,
        ),
        (
            " def save(): pass\n values = [save() for value in values]\n",
            0,
        ),
        (" def save(): pass\n class Inner:\n  value = save()\n", 0),
    ] {
        let code = format!(
            "def outer():\n class Store:\n{}",
            body.lines()
                .map(|line| format!(" {line}\n"))
                .collect::<String>()
        );
        let (_dir, mut s) = setup(&[("a.py", &code)]);
        let save = symbol(&mut s, "a.py", "save");
        let callers = relations(&mut s, &save, "callers");
        assert_eq!(callers["parse_error_count"], 0, "{code}: {callers}");
        assert_eq!(
            callers["total_results"], expected_calls,
            "{code}: {callers}"
        );
    }
}

#[test]
fn shorthand_references_respect_shadowing_and_comprehensions_respect_outer_locals() {
    let (_dir, mut s) = setup(&[(
        "a.js",
        "function save() {} function main(save) { return {save}; }",
    )]);
    let save = symbol(&mut s, "a.js", "save");
    assert_eq!(relations(&mut s, &save, "references")["total_results"], 0);
    let (_dir, mut s) = setup(&[(
        "a.py",
        "def main():\n def save(): pass\n return [save() for save in save()]\n",
    )]);
    let save = symbol(&mut s, "a.py", "save");
    assert_eq!(relations(&mut s, &save, "callers")["total_results"], 1);
}

#[test]
fn foreach_bindings_preserve_language_specific_method_lookup() {
    for (path, code) in [
        (
            "A.cs",
            "class A { object save() { return null; } void Main() { foreach(var save in save()) { save(); } } }",
        ),
        (
            "A.java",
            "class A { Object save() { return null; } void Main() { for(Object save : save()) { save(); } } }",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, code)]);
        let main = symbol(&mut s, path, "Main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(
            calls["relations"][0]["resolution"], "candidate",
            "{code}: {calls}"
        );
        assert_eq!(
            calls["relations"][1]["resolution"],
            if path.ends_with(".java") {
                "candidate"
            } else {
                "unresolved"
            },
            "{code}: {calls}"
        );
    }
}

#[cfg(unix)]
#[test]
fn directory_search_does_not_walk_unreadable_sibling_directories() {
    use std::os::unix::fs::PermissionsExt;
    // Root can read mode-000 directories, so this fixture cannot reproduce the
    // access boundary when the suite runs with that identity.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    struct Restore(std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }
    let (_dir, mut s) = setup(&[
        ("src/main.js", "function main() {}"),
        ("unrelated/private.js", "function hidden() {}"),
    ]);
    let restore = Restore(s.project.root.join("unrelated"));
    std::fs::set_permissions(&restore.0, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = run(
        &mut s,
        "symbol_search",
        json!({"path":"src","query":"main"}),
    );
    assert_eq!(result["total_results"], 1, "{result}");
    assert_eq!(result["scanned_files"], 1);
}

#[test]
fn declaration_excerpt_quality_reports_cut_lines_and_keeps_navigation_only() {
    for (comment, complete) in [("short".to_owned(), true), ("저장".repeat(500), false)] {
        let code = format!("function main() {{ /* {comment} */ }}");
        let (_dir, mut s) = setup(&[("a.js", &code)]);
        let main = symbol(&mut s, "a.js", "main");
        // Source omits true quality flags from JSON; absence means complete.
        assert_eq!(
            main["source"]["line_end_complete"]
                .as_bool()
                .unwrap_or(true),
            complete,
            "{main}"
        );
        assert_eq!(main["source"]["evidence_truncated"], true);
        assert!(s.read_coverage.is_empty());
    }
}

#[test]
fn directory_scoping_preserves_parent_ignore_rules_and_generated_directory_exclusions() {
    let (_dir, mut s) = setup(&[
        (".ignore", "src/ignored.js\n"),
        ("src/ignored.js", "function ignored() {}"),
        ("src/main.js", "function main() {}"),
        ("src/node_modules/vendor.js", "function vendor() {}"),
        ("outside/other.js", "function other() {}"),
    ]);
    let result = run(&mut s, "symbol_search", json!({"path":"src"}));
    assert_eq!(result["total_results"], 1, "{result}");
    assert_eq!(result["symbols"][0]["name"], "main");
    assert_eq!(result["matched_files"], 1);
}

#[test]
fn java_unqualified_method_calls_and_constructors_ignore_value_name_collisions() {
    for (code, expected_kind) in [
        (
            "class A { void save() {} void main(int save) { save(); } }",
            "method",
        ),
        (
            "class A { int save; void save() {} void main() { save(); } }",
            "method",
        ),
        (
            "import static dep.Tools.save; class A { void main(int save) { save(); } }",
            "method",
        ),
        (
            "class Store {} class A { void main(Object Store) { new Store(); } }",
            "class",
        ),
    ] {
        let (_dir, mut s) = setup(&[
            ("A.java", code),
            (
                "dep/Tools.java",
                "package dep; class Tools { static int save; static void save() {} }",
            ),
        ]);
        let main = symbol(&mut s, "A.java", "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(calls["parse_error_count"], 0, "{calls}");
        assert_eq!(
            calls["relations"][0]["candidate_count"], 1,
            "{code}: {calls}"
        );
        assert_eq!(
            calls["relations"][0]["candidates"][0]["symbol_kind"], expected_kind,
            "{code}: {calls}"
        );
    }
    let (_dir, mut s) = setup(&[
        (
            "A.java",
            "import dep.Tools; class A { void main(Object Tools) { Tools.save(); } }",
        ),
        (
            "dep/Tools.java",
            "package dep; class Tools { static void save() {} }",
        ),
    ]);
    let main = symbol(&mut s, "A.java", "main");
    assert_eq!(
        relations(&mut s, &main, "calls")["relations"][0]["resolution"],
        "unresolved"
    );
}

#[test]
fn csharp_constructor_type_name_is_not_shadowed_by_a_local_value() {
    let (_dir, mut s) = setup(&[(
        "A.cs",
        "class Store {} class A { void Main(object Store) { var value = new Store(); } }",
    )]);
    let main = symbol(&mut s, "A.cs", "Main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["parse_error_count"], 0, "{calls}");
    assert_eq!(calls["total_results"], 1, "{calls}");
    assert_eq!(calls["relations"][0]["resolution"], "candidate", "{calls}");
    assert_eq!(
        calls["relations"][0]["candidates"][0]["symbol_kind"], "class",
        "{calls}"
    );
}

#[test]
fn csharp_constructor_type_can_come_from_a_using_namespace() {
    let (_dir, mut s) = setup(&[
        (
            "A.cs",
            "using Shop; class A { void Main() { var value = new Store(); } }",
        ),
        ("Shop/Store.cs", "namespace Shop { class Store {} }"),
    ]);
    let main = symbol(&mut s, "A.cs", "Main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["parse_error_count"], 0, "{calls}");
    assert_eq!(calls["relations"][0]["resolution"], "candidate", "{calls}");
    assert_eq!(
        calls["relations"][0]["candidates"][0]["qualified_name"], "Shop::Store",
        "{calls}"
    );
}

#[test]
fn java_constructor_type_can_come_from_the_same_package() {
    let (_dir, mut s) = setup(&[
        (
            "shop/A.java",
            "package shop; class A { void main() { new Store(); } }",
        ),
        ("shop/Store.java", "package shop; class Store {}"),
    ]);
    let main = symbol(&mut s, "shop/A.java", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["parse_error_count"], 0, "{calls}");
    assert_eq!(calls["relations"][0]["resolution"], "candidate", "{calls}");
    assert_eq!(
        calls["relations"][0]["candidates"][0]["qualified_name"], "Store",
        "{calls}"
    );
}

#[test]
fn csharp_same_namespace_types_are_candidates_inside_nested_classes() {
    let (_dir, mut s) = setup(&[
        (
            "A.cs",
            "namespace Shop { class Outer { class Inner { void Main() { new Store(); Store.Open(); } } } }",
        ),
        (
            "Store.cs",
            "namespace Shop { class Store { public static void Open() {} } }",
        ),
    ]);
    let main = symbol(&mut s, "A.cs", "Main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["parse_error_count"], 0, "{calls}");
    assert_eq!(calls["total_results"], 2, "{calls}");
    for relation in calls["relations"].as_array().unwrap() {
        assert_eq!(relation["resolution"], "candidate", "{calls}");
        assert_eq!(relation["candidate_count"], 1, "{calls}");
    }
    assert_eq!(
        calls["relations"][0]["candidates"][0]["qualified_name"], "Shop::Store",
        "{calls}"
    );
    assert_eq!(
        calls["relations"][1]["candidates"][0]["qualified_name"], "Shop::Store::Open",
        "{calls}"
    );
}

#[test]
fn java_and_csharp_type_references_ignore_same_named_values() {
    for (path, code) in [
        (
            "A.java",
            "class Store {} class A { void main(Object Store) { Store value = null; } }",
        ),
        (
            "A.cs",
            "class Store {} class A { void Main(object Store) { Store value = null; } }",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, code)]);
        let store = symbol(&mut s, path, "Store");
        let refs = relations(&mut s, &store, "references");
        assert_eq!(refs["parse_error_count"], 0, "{code}: {refs}");
        assert_eq!(refs["total_results"], 1, "{code}: {refs}");
        assert_eq!(refs["relations"][0]["resolution"], "resolved", "{refs}");
    }
}

#[test]
fn rust_type_references_ignore_same_named_value_bindings() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "struct Store; fn main(Store: i32) { let _: Store; }",
    )]);
    let store = symbol(&mut s, "lib.rs", "Store");
    let refs = relations(&mut s, &store, "references");
    assert_eq!(refs["parse_error_count"], 0, "{refs}");
    assert_eq!(refs["total_results"], 1, "{refs}");
    assert_eq!(refs["relations"][0]["resolution"], "resolved", "{refs}");
}

#[test]
fn generic_type_parameters_do_not_link_to_same_named_outer_types() {
    for (path, code) in [
        (
            "a.ts",
            "type Store = string; function main<Store>(value: Store): Store { return value; }",
        ),
        (
            "a.tsx",
            "type Store = string; function main<Store>(value: Store): Store { return value; }",
        ),
        ("lib.rs", "struct Store; fn main<Store>() { let _: Store; }"),
        (
            "A.java",
            "class Store {} class A { <Store> void main(Store value) {} }",
        ),
        (
            "A.cs",
            "class Store {} class A { void Main<Store>(Store value) {} }",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, code)]);
        let store = symbol(&mut s, path, "Store");
        let refs = relations(&mut s, &store, "references");
        assert_eq!(refs["parse_error_count"], 0, "{code}: {refs}");
        assert_eq!(refs["total_results"], 0, "{code}: {refs}");
    }
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "struct Store; fn generic<Store>() { let _: Store; } fn main() { let _: Store; }",
    )]);
    let store = symbol(&mut s, "lib.rs", "Store");
    let refs = relations(&mut s, &store, "references");
    assert_eq!(refs["total_results"], 1, "{refs}");
    assert_eq!(refs["relations"][0]["resolution"], "resolved", "{refs}");
}

#[test]
fn typescript_type_references_ignore_values_but_runtime_receivers_remain_values() {
    let (_dir, mut s) = setup(&[(
        "a.ts",
        "type Store = string; function main(Store: number): Store { return ''; }",
    )]);
    let store = run(
        &mut s,
        "symbol_search",
        json!({"path":"a.ts","query":"Store","match":"exact","kind":"type"}),
    )["symbols"][0]
        .clone();
    let refs = relations(&mut s, &store, "references");
    assert_eq!(refs["total_results"], 1, "{refs}");
    assert_eq!(refs["relations"][0]["resolution"], "resolved", "{refs}");

    let (_dir, mut s) = setup(&[(
        "a.ts",
        "const Store = { save() {} }; function main<Store>() { Store.save(); }",
    )]);
    let main = symbol(&mut s, "a.ts", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["total_results"], 1, "{calls}");
    assert_ne!(
        calls["relations"][0]["reason"], "type_parameter_without_symbol",
        "{calls}"
    );
}

#[test]
fn qualified_generic_type_calls_do_not_link_to_outer_types() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "trait Save { fn save(); } struct Store; impl Store { fn save() {} } fn main<Store: Save>() { Store::save(); }",
    )]);
    let main = symbol(&mut s, "lib.rs", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["parse_error_count"], 0, "{calls}");
    assert_eq!(calls["relations"][0]["resolution"], "unresolved", "{calls}");
}

#[test]
fn rust_scoped_relation_keeps_crate_root_when_entry_file_is_outside_glob() {
    let (_dir, mut s) = setup(&[
        ("src/lib.rs", "mod store; mod other;"),
        ("src/store.rs", "pub fn main() { crate::other::save(); }"),
        ("src/other.rs", "pub fn save() {}"),
    ]);
    let main = symbol(&mut s, "src/store.rs", "main");
    let calls = run(
        &mut s,
        "symbol_relations",
        json!({"path":main["path"],"symbol_id":main["symbol_id"],"path_glob":"src/[so]*.rs"}),
    );
    assert_eq!(calls["scanned_files"], 2, "{calls}");
    assert_eq!(calls["relations"][0]["candidate_count"], 1, "{calls}");
    assert_eq!(
        calls["relations"][0]["candidates"][0]["path"], "src/other.rs",
        "{calls}"
    );
}

#[test]
fn rust_scoped_relation_cursor_expires_when_crate_entry_disappears() {
    let (dir, mut s) = setup(&[
        ("src/lib.rs", "mod store; mod other;"),
        (
            "src/store.rs",
            "pub fn main() { crate::other::save(); crate::other::save(); }",
        ),
        ("src/other.rs", "pub fn save() {}"),
    ]);
    let main = symbol(&mut s, "src/store.rs", "main");
    let args = json!({"path":main["path"],"symbol_id":main["symbol_id"],"path_glob":"src/[so]*.rs","limit":1});
    let first = run(&mut s, "symbol_relations", args.clone());
    assert_eq!(first["total_results"], 2, "{first}");
    let mut next = args;
    next["cursor"] = first["next_cursor"].clone();
    std::fs::remove_file(dir.path().join("src/lib.rs")).unwrap();
    let error = tools::execute(&mut s, "symbol_relations", next).unwrap_err();
    assert!(error.to_string().starts_with("cursor_expired"), "{error}");
}

#[test]
fn python_global_and_nonlocal_only_taint_their_own_scopes() {
    for directive in ["global save", "nonlocal save"] {
        let first = if directive == "global save" {
            format!("def first():\n {directive}\n save()\n")
        } else {
            format!("def outer():\n def save(): pass\n def first():\n  {directive}\n  save()\n")
        };
        let source = format!("def save(): pass\n{first}def second():\n save()\n");
        let (_dir, mut s) = setup(&[("a.py", &source)]);
        let first = symbol(&mut s, "a.py", "first");
        let second = symbol(&mut s, "a.py", "second");
        let first_calls = relations(&mut s, &first, "calls");
        assert_eq!(
            first_calls["relations"][0]["resolution"], "unresolved",
            "{directive}: {first_calls}"
        );
        let second_calls = relations(&mut s, &second, "calls");
        assert_eq!(
            second_calls["relations"][0]["resolution"], "resolved",
            "{directive}: {second_calls}"
        );
    }
}

#[test]
fn python_global_and_nonlocal_mutations_respect_closer_local_names() {
    for source in [
        "def save(): pass\ndef change(other):\n global save\n save = other\ndef main():\n def save(): pass\n save()\n",
        "def outer():\n def save(): pass\n def change(other):\n  nonlocal save\n  save = other\n def main():\n  def save(): pass\n  save()\n",
    ] {
        let (_dir, mut s) = setup(&[("a.py", source)]);
        let main = symbol(&mut s, "a.py", "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(
            calls["relations"][0]["resolution"], "resolved",
            "{source}: {calls}"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn workspace_search_skips_non_utf8_paths_without_hiding_other_results() {
    use std::os::unix::ffi::OsStringExt;

    let (dir, mut s) = setup(&[("valid.rs", "fn found() {}")]);
    let mut name = b"invalid-".to_vec();
    name.push(0xff);
    name.extend_from_slice(b".rs");
    std::fs::write(
        dir.path().join(std::ffi::OsString::from_vec(name)),
        "fn hidden() {}",
    )
    .unwrap();

    let result = run(&mut s, "symbol_search", json!({"query":"found"}));
    assert_eq!(result["total_results"], 1, "{result}");
    assert_eq!(result["symbols"][0]["path"], "valid.rs");
    assert_eq!(result["skipped_files"], 1);
}

#[test]
fn statement_labels_are_not_function_references() {
    for (path, source) in [
        (
            "a.js",
            "function save() {} function main() { save: for (;;) { break save; } }",
        ),
        (
            "A.java",
            "class A { void save() {} void main() { save: while (true) { break save; } } }",
        ),
        (
            "A.cs",
            "class A { void save() {} void main() { save: goto save; } }",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, source)]);
        let save = symbol(&mut s, path, "save");
        let references = relations(&mut s, &save, "references");
        assert_eq!(references["parse_error_count"], 0, "{path}: {references}");
        assert_eq!(references["total_results"], 0, "{path}: {references}");
    }
}

#[test]
fn non_value_names_are_not_references_to_same_named_symbols() {
    let (_dir, mut s) = setup(&[(
        "a.py",
        "def save(): pass\ndef receiver(save): pass\ndef main():\n receiver(save=save)\n",
    )]);
    let save = symbol(&mut s, "a.py", "save");
    let references = relations(&mut s, &save, "references");
    assert_eq!(references["total_results"], 1, "{references}");
    assert_eq!(references["relations"][0]["expression"], "save");

    let (_dir, mut s) = setup(&[(
        "A.cs",
        "class A { void save() {} void receiver(int save) {} void main() { receiver(save: 1); } }",
    )]);
    let save = symbol(&mut s, "A.cs", "save");
    let references = relations(&mut s, &save, "references");
    assert_eq!(references["parse_error_count"], 0, "{references}");
    assert_eq!(references["total_results"], 0, "{references}");

    let (_dir, mut s) = setup(&[(
        "A.java",
        "@interface Tag { int save(); } class A { void save() {} @Tag(save = 1) void main() {} }",
    )]);
    let found = run(
        &mut s,
        "symbol_search",
        json!({"path":"A.java","query":"save","match":"exact"}),
    );
    let target = found["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["qualified_name"] == "A::save")
        .unwrap()
        .clone();
    let references = relations(&mut s, &target, "references");
    assert_eq!(references["parse_error_count"], 0, "{references}");
    assert_eq!(references["total_results"], 0, "{references}");

    let (_dir, mut s) = setup(&[(
        "B.cs",
        "class TagAttribute : System.Attribute { public int save { get; set; } } class A { void save() {} [Tag(save = 1)] void main() {} }",
    )]);
    let found = run(
        &mut s,
        "symbol_search",
        json!({"path":"B.cs","query":"save","match":"exact"}),
    );
    let target = found["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["qualified_name"] == "A::save")
        .unwrap()
        .clone();
    let references = relations(&mut s, &target, "references");
    assert_eq!(references["parse_error_count"], 0, "{references}");
    assert_eq!(references["total_results"], 0, "{references}");

    let (_dir, mut s) = setup(&[(
        "Tuple.cs",
        "class A { void save() {} void main() { (int save, int other) value = (1, 2); } }",
    )]);
    let save = symbol(&mut s, "Tuple.cs", "save");
    let references = relations(&mut s, &save, "references");
    assert_eq!(references["parse_error_count"], 0, "{references}");
    assert_eq!(references["total_results"], 0, "{references}");
}

#[test]
fn control_flow_labels_do_not_hide_real_calls_or_goto_case_values() {
    let (_dir, mut s) = setup(&[(
        "A.java",
        "class A { void save() {} void main() { save: while (true) { save(); break save; } } }",
    )]);
    let save = symbol(&mut s, "A.java", "save");
    let references = relations(&mut s, &save, "references");
    assert_eq!(references["total_results"], 1, "{references}");
    assert_eq!(references["relations"][0]["kind"], "call");

    let (_dir, mut s) = setup(&[(
        "A.cs",
        "class A { const int save = 1; void main(int x) { switch (x) { case 0: goto case save; case 1: break; } } }",
    )]);
    let save = symbol(&mut s, "A.cs", "save");
    let references = relations(&mut s, &save, "references");
    assert_eq!(references["total_results"], 1, "{references}");
}

#[test]
fn export_aliases_and_reexports_do_not_reference_unrelated_local_functions() {
    for path in ["a.js", "a.ts"] {
        for (export, expected_save_references) in [
            ("export { save as alias };", 1),
            ("export { save as alias } from './dep';", 0),
            ("export * as alias from './dep';", 0),
        ] {
            let source = format!("function save() {{}} function alias() {{}} {export}");
            let (_dir, mut s) = setup(&[(path, &source), ("dep.js", "export function save() {}")]);
            let alias = symbol(&mut s, path, "alias");
            let alias_references = relations(&mut s, &alias, "references");
            assert_eq!(
                alias_references["parse_error_count"], 0,
                "{alias_references}"
            );
            assert_eq!(
                alias_references["total_results"], 0,
                "{path} {export}: {alias_references}"
            );
            let save = symbol(&mut s, path, "save");
            let save_references = relations(&mut s, &save, "references");
            assert_eq!(
                save_references["total_results"], expected_save_references,
                "{path} {export}: {save_references}"
            );
        }
    }
}

#[test]
fn typescript_type_only_exports_do_not_reference_same_named_runtime_values() {
    for (path, export) in [
        ("a.ts", "export type { Store };"),
        ("a.ts", "export { type Store };"),
        ("a.tsx", "export type { Store };"),
        ("a.tsx", "export { type Store };"),
    ] {
        let source = format!("type Store = {{ value: number }}; function Store() {{}} {export}");
        let (_dir, mut s) = setup(&[(path, &source)]);
        let found = run(
            &mut s,
            "symbol_search",
            json!({"path":path,"query":"Store","match":"exact"}),
        );
        assert_eq!(found["symbols"].as_array().unwrap().len(), 2, "{found}");
        let symbols = found["symbols"].as_array().unwrap();
        let runtime = symbols
            .iter()
            .find(|s| s["symbol_kind"] == "function")
            .unwrap()
            .clone();
        let type_alias = symbols
            .iter()
            .find(|s| s["symbol_kind"] == "type")
            .unwrap()
            .clone();
        let runtime_refs = relations(&mut s, &runtime, "references");
        assert_eq!(runtime_refs["parse_error_count"], 0, "{runtime_refs}");
        assert_eq!(runtime_refs["total_results"], 0, "{export}: {runtime_refs}");
        let type_refs = relations(&mut s, &type_alias, "references");
        assert_eq!(type_refs["total_results"], 1, "{export}: {type_refs}");
    }
}

#[test]
fn typescript_type_only_imports_and_exports_do_not_supply_runtime_calls() {
    for (declaration, import, main_path, expected_runtime_candidates) in [
        (
            "class Store {} export type { Store };",
            "import { Store } from './dep';",
            "main.ts",
            0,
        ),
        (
            "class Store {} export { type Store };",
            "import { Store } from './dep';",
            "main.ts",
            0,
        ),
        (
            "export type Store = {};",
            "import { Store } from './dep';",
            "main.ts",
            0,
        ),
        (
            "export class Store {}",
            "import type { Store } from './dep';",
            "main.ts",
            0,
        ),
        (
            "export class Store {}",
            "import { type Store } from './dep';",
            "main.ts",
            0,
        ),
        (
            "export class Store {}",
            "import { Store } from './dep';",
            "main.ts",
            1,
        ),
        (
            "export class Store {}",
            "import type { Store } from './dep';",
            "main.tsx",
            0,
        ),
    ] {
        let source = format!("{import} function main() {{ new Store(); let value: Store; }}");
        let (_dir, mut s) = setup(&[("dep.ts", declaration), (main_path, &source)]);
        let main = symbol(&mut s, main_path, "main");
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(calls["parse_error_count"], 0, "{calls}");
        let constructor = calls["relations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "Store")
            .unwrap();
        assert_eq!(
            constructor["candidate_count"], expected_runtime_candidates,
            "{declaration} {import}: {calls}"
        );

        let store = symbol(&mut s, "dep.ts", "Store");
        let references = relations(&mut s, &store, "references");
        assert!(
            references["total_results"].as_u64().unwrap() >= 1,
            "{declaration} {import}: {references}"
        );
    }
}

#[test]
fn rust_macro_tokens_are_not_function_calls_or_references() {
    let source = "macro_rules! save { () => {} } macro_rules! discard { ($e:expr) => {} } fn save() {} fn main() { save!(); discard!(save()); save(); crate::save(); }";
    let (_dir, mut s) = setup(&[("lib.rs", source)]);
    let found = run(
        &mut s,
        "symbol_search",
        json!({"path":"lib.rs","query":"save","match":"exact"}),
    );
    let function = found["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["symbol_kind"] == "function")
        .unwrap()
        .clone();
    let references = relations(&mut s, &function, "references");
    assert_eq!(references["parse_error_count"], 0, "{references}");
    assert_eq!(references["total_results"], 2, "{references}");
    for site in references["relations"].as_array().unwrap() {
        assert_eq!(site["kind"], "call");
        assert_eq!(site["candidate_count"], 1, "{references}");
        assert_eq!(site["candidates"][0]["symbol_kind"], "function");
    }
    let main = symbol(&mut s, "lib.rs", "main");
    assert_eq!(relations(&mut s, &main, "calls")["total_results"], 2);

    let (_dir, mut s) = setup(&[("lib.rs", "fn save() {} #[allow(save)] fn main() {}")]);
    let save = symbol(&mut s, "lib.rs", "save");
    let references = relations(&mut s, &save, "references");
    assert_eq!(references["parse_error_count"], 0, "{references}");
    assert_eq!(references["total_results"], 0, "{references}");
}

#[test]
fn annotation_type_names_do_not_reference_same_named_methods() {
    for (path, source, method_name) in [
        (
            "A.java",
            "@interface Save {} class A { void Save() {} @Save void Main() {} }",
            "Save",
        ),
        (
            "A.cs",
            "class SaveAttribute : System.Attribute {} class A { void Save() {} [Save] void Main() {} }",
            "Save",
        ),
        (
            "A.java",
            "package demo; @interface Save {} class A { void Save() {} @demo.Save void Main() {} }",
            "Save",
        ),
        (
            "A.cs",
            "namespace Demo { class SaveAttribute : System.Attribute {} class A { void Save() {} [Demo.Save] void Main() {} } }",
            "Save",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, source)]);
        let found = run(
            &mut s,
            "symbol_search",
            json!({"path":path,"query":method_name,"match":"exact"}),
        );
        let method = found["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| {
                s["qualified_name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with("A::Save"))
            })
            .unwrap()
            .clone();
        let references = relations(&mut s, &method, "references");
        assert_eq!(references["parse_error_count"], 0, "{references}");
        assert_eq!(references["total_results"], 0, "{path}: {references}");
        if path == "A.cs" {
            let attribute = symbol(&mut s, path, "SaveAttribute");
            let references = relations(&mut s, &attribute, "references");
            assert_eq!(references["total_results"], 1, "{references}");
        }
    }
}

#[test]
fn annotation_type_names_still_reference_the_declared_type() {
    for (path, source, type_name) in [
        (
            "A.java",
            "@interface Save {} class A { @Save void Main() {} }",
            "Save",
        ),
        (
            "A.java",
            "package demo; @interface Save {} class A { @demo.Save void Main() {} }",
            "Save",
        ),
        (
            "A.cs",
            "class SaveAttribute : System.Attribute {} class A { [SaveAttribute] void Main() {} }",
            "SaveAttribute",
        ),
        (
            "A.cs",
            "class SaveAttribute : System.Attribute {} class A { [Save] void Main() {} }",
            "SaveAttribute",
        ),
        (
            "A.cs",
            "namespace Demo { class SaveAttribute : System.Attribute {} class A { [Demo.Save] void Main() {} } }",
            "SaveAttribute",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, source)]);
        let declared = symbol(&mut s, path, type_name);
        let references = relations(&mut s, &declared, "references");
        assert_eq!(references["parse_error_count"], 0, "{references}");
        assert_eq!(references["total_results"], 1, "{path}: {references}");
    }
}

#[test]
fn csharp_verbatim_attribute_name_uses_only_the_exact_type() {
    let (_dir, mut s) = setup(&[(
        "A.cs",
        "class Save : System.Attribute {} class SaveAttribute : System.Attribute {} class A { [@Save] void Main() {} }",
    )]);
    let exact = symbol(&mut s, "A.cs", "Save");
    let suffixed = symbol(&mut s, "A.cs", "SaveAttribute");
    let references = relations(&mut s, &exact, "references");
    assert_eq!(references["parse_error_count"], 0, "{references}");
    assert_eq!(references["total_results"], 1, "{references}");
    assert_eq!(
        relations(&mut s, &suffixed, "references")["total_results"],
        0
    );

    let (_dir, mut s) = setup(&[(
        "A.cs",
        "class @Save : System.Attribute {} class A { [Save] void Main() {} }",
    )]);
    let declared = symbol(&mut s, "A.cs", "@Save");
    let references = relations(&mut s, &declared, "references");
    assert_eq!(references["parse_error_count"], 0, "{references}");
    assert_eq!(references["total_results"], 1, "{references}");
}

// Live run 2026-10-09: a model sent symbol names as pattern, the legacy
// alias of path_glob, five times in a row, and was told to drop a path_glob
// it never sent.
#[test]
fn a_symbol_name_sent_as_pattern_is_named_as_such() {
    let (_dir, mut s) = setup(&[("src/web.rs", "fn headless() {}\nfn serve_app() {}\n")]);
    let error = tools::execute(
        &mut s,
        "symbol_search",
        json!({"path":"src/web.rs","pattern":"headless","kind":"function"}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.starts_with(
            "conflicting_path_filters: pattern \"headless\" is the legacy alias of path_glob"
        ),
        "{error}"
    );
    assert!(error.contains("send the name as query"), "{error}");
    let error = tools::execute(&mut s, "symbol_search", json!({"pattern":"serve_app|api"}))
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("invalid_argument_value: pattern"),
        "{error}"
    );
    // A real file glob still filters files.
    let found = run(
        &mut s,
        "symbol_search",
        json!({"pattern":"src/*.rs","query":"headless"}),
    );
    assert_eq!(found["symbols"].as_array().unwrap().len(), 1, "{found}");
    let schema = ToolRegistry::specs()
        .into_iter()
        .find(|spec| spec.name == "symbol_search")
        .unwrap()
        .parameters;
    assert!(
        schema["properties"]["pattern"]["description"]
            .as_str()
            .unwrap()
            .contains("never a symbol name"),
        "{schema}"
    );
}
