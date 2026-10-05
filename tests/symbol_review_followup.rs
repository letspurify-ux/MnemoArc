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
    tools::execute(s, name, args).unwrap()
}

fn find(s: &mut Session, path: &str, name: &str) -> Vec<Value> {
    let result = run(
        s,
        "symbol_search",
        json!({"path":path,"query":name,"match":"exact","limit":100}),
    );
    assert_eq!(result["parse_error_count"], 0, "{result}");
    result["symbols"].as_array().unwrap().clone()
}

fn symbol(s: &mut Session, path: &str, name: &str) -> Value {
    let symbols = find(s, path, name);
    assert_eq!(symbols.len(), 1, "{symbols:?}");
    symbols[0].clone()
}

fn relations(s: &mut Session, symbol: &Value, relation: &str) -> Value {
    run(
        s,
        "symbol_relations",
        json!({"path":symbol["path"],"symbol_id":symbol["symbol_id"],"relation":relation,"limit":100}),
    )
}

fn candidates(result: &Value) -> Vec<Value> {
    result["relations"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|r| r["candidates"].as_array().unwrap().iter().cloned())
        .collect()
}

#[test]
fn python_definition_defaults_belong_to_the_enclosing_execution() {
    let (_dir, mut s) = setup(&[(
        "a.py",
        "def helper(): pass\ndef outer():\n def inner(x=helper()):\n  pass\n return inner\n",
    )]);
    let outer = symbol(&mut s, "a.py", "outer");
    let inner = symbol(&mut s, "a.py", "inner");
    let calls = relations(&mut s, &outer, "calls");
    assert_eq!(calls["total_results"], 1, "{calls}");
    assert_eq!(
        calls["relations"][0]["enclosing_symbol"]["symbol_id"],
        outer["symbol_id"]
    );
    assert_eq!(relations(&mut s, &inner, "calls")["total_results"], 0);
}

#[test]
fn python_lambda_defaults_belong_to_the_enclosing_execution() {
    let (_dir, mut s) = setup(&[(
        "a.py",
        "def helper(): pass\ndef outer():\n return lambda x=helper(): helper()\n",
    )]);
    let outer = symbol(&mut s, "a.py", "outer");
    let calls = relations(&mut s, &outer, "calls");
    assert_eq!(calls["total_results"], 1, "{calls}");
    let helper = symbol(&mut s, "a.py", "helper");
    let callers = relations(&mut s, &helper, "callers");
    assert_eq!(callers["total_results"], 2);
    assert_eq!(
        callers["relations"][0]["enclosing_symbol"]["symbol_id"],
        outer["symbol_id"]
    );
    assert_eq!(
        callers["relations"][1]["enclosing_callable"]["anonymous"],
        true
    );
}

#[test]
fn python_receiver_shadows_an_outer_import() {
    let (_dir, mut s) = setup(&[
        ("self.py", "def target(): pass\n"),
        (
            "a.py",
            "import self\nclass A:\n def target(self): pass\n def main(self):\n  self.target()\n",
        ),
    ]);
    let main = symbol(&mut s, "a.py", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(candidates(&calls).len(), 1, "{calls}");
    assert_eq!(candidates(&calls)[0]["qualified_name"], "A::target");
}

#[test]
fn repeated_local_class_names_do_not_mix_members_between_blocks() {
    let (_dir, mut s) = setup(&[(
        "a.js",
        "function main() {\n { class A { static target() {} } A.target(); }\n { class A { static target() {} } A.target(); }\n}\n",
    )]);
    let targets = find(&mut s, "a.js", "target");
    assert_eq!(targets.len(), 2);
    for target in targets {
        let callers = relations(&mut s, &target, "callers");
        assert_eq!(callers["total_results"], 1, "{callers}");
        assert_eq!(callers["relations"][0]["line"], target["name_line"]);
        assert_eq!(callers["relations"][0]["candidate_count"], 1);
    }
}

#[test]
fn repeated_local_rust_types_do_not_mix_impl_members_between_blocks() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "fn main() {\n { struct A; impl A { fn target() {} } A::target(); }\n { struct A; impl A { fn target() {} } A::target(); }\n}\n",
    )]);
    let targets = find(&mut s, "lib.rs", "target");
    assert_eq!(targets.len(), 2);
    for target in targets {
        let callers = relations(&mut s, &target, "callers");
        assert_eq!(callers["total_results"], 1, "{callers}");
        assert_eq!(callers["relations"][0]["line"], target["name_line"]);
        assert_eq!(callers["relations"][0]["candidate_count"], 1);
    }
}

#[test]
fn rust_lifetimes_and_loop_labels_are_not_value_references() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "fn target() {}\nfn borrow<'target>(x: &'target str) -> &'target str { x }\nfn main() { 'target: loop { target(); break 'target; } }\n",
    )]);
    let target = symbol(&mut s, "lib.rs", "target");
    let refs = relations(&mut s, &target, "references");
    assert_eq!(refs["total_results"], 1, "{refs}");
    assert_eq!(refs["relations"][0]["kind"], "call");
}

#[test]
fn rust_const_parameters_shadow_outer_values() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "const SIZE: usize = 1;\nfn generic<const SIZE: usize>() { let _ = SIZE; }\nfn main() { let _ = SIZE; }\n",
    )]);
    let size = symbol(&mut s, "lib.rs", "SIZE");
    let refs = relations(&mut s, &size, "references");
    assert_eq!(refs["total_results"], 1, "{refs}");
    assert_eq!(refs["relations"][0]["line"], 3);
}

#[test]
fn rust_paths_ignore_whitespace_and_comments_between_segments() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "mod store { pub fn target() {} } fn main() { crate :: store :: target(); crate /* here */ :: store::target(); }",
    )]);
    let main = symbol(&mut s, "lib.rs", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["total_results"], 2);
    assert_eq!(candidates(&calls).len(), 2, "{calls}");
}

#[test]
fn csharp_accessors_exclude_nested_callbacks_and_keep_named_owners() {
    let (_dir, mut s) = setup(&[(
        "A.cs",
        "class A { static int Target() => 1; static void Nested() {} int Value { get { System.Action callback = () => Nested(); return Target(); } } }",
    )]);
    let getter = symbol(&mut s, "A.cs", "get");
    let calls = relations(&mut s, &getter, "calls");
    assert_eq!(calls["total_results"], 1, "{calls}");
    assert_eq!(calls["relations"][0]["name"], "Target");
    assert_eq!(
        calls["relations"][0]["enclosing_symbol"]["symbol_id"],
        getter["symbol_id"]
    );
}

#[test]
fn csharp_operators_exclude_nested_callbacks_and_keep_named_owners() {
    let (_dir, mut s) = setup(&[(
        "A.cs",
        "class A { static void Target() {} static void Nested() {} public static A operator +(A x, A y) { System.Action callback = () => Nested(); Target(); return x; } }",
    )]);
    let operator = symbol(&mut s, "A.cs", "operator +");
    let calls = relations(&mut s, &operator, "calls");
    assert_eq!(calls["total_results"], 1, "{calls}");
    assert_eq!(
        calls["relations"][0]["enclosing_symbol"]["symbol_id"],
        operator["symbol_id"]
    );
}

#[test]
fn csharp_verbatim_container_names_link_members() {
    let (_dir, mut s) = setup(&[(
        "A.cs",
        "class @Store { public static void @Save() {} } class Main { void Run() { @Store.@Save(); } }",
    )]);
    let main = symbol(&mut s, "A.cs", "Run");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(candidates(&calls).len(), 1, "{calls}");
    assert_eq!(candidates(&calls)[0]["qualified_name"], "@Store::@Save");
}

#[test]
fn quoted_javascript_export_aliases_link_the_original_declaration() {
    let (_dir, mut s) = setup(&[
        (
            "a.js",
            "function target() {} export { target as 'some-name' };",
        ),
        (
            "main.js",
            "import { 'some-name' as invoke } from './a.js'; function main() { invoke(); }",
        ),
    ]);
    let main = symbol(&mut s, "main.js", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(candidates(&calls).len(), 1, "{calls}");
    assert_eq!(candidates(&calls)[0]["name"], "target");
}

#[test]
fn rust_self_module_paths_are_distinct_from_method_receivers() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "fn target() {} struct A; impl A { fn target(&self) {} fn main(&self) { self::target(); self.target(); Self::target(self); } }",
    )]);
    let main = symbol(&mut s, "lib.rs", "main");
    let calls = relations(&mut s, &main, "calls");
    let names: Vec<_> = candidates(&calls)
        .iter()
        .map(|c| c["qualified_name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["target", "A::target", "A::target"], "{calls}");
}

#[test]
fn csharp_verbatim_this_is_an_ordinary_parameter() {
    let (_dir, mut s) = setup(&[(
        "A.cs",
        "class A { void Target() {} void Main(A @this) { @this.Target(); this.Target(); } }",
    )]);
    let main = symbol(&mut s, "A.cs", "Main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(calls["relations"][0]["candidate_count"], 0, "{calls}");
    assert_eq!(calls["relations"][1]["candidate_count"], 1);
}

#[test]
fn same_named_receivers_stay_inside_their_actual_class() {
    for (path, code) in [
        (
            "a.js",
            "function outer() {\n { class A { target() {} main() { this.target(); } } }\n { class A { target() {} main() { this.target(); } } }\n}",
        ),
        (
            "a.py",
            "def outer():\n class A:\n  def target(self): pass\n  def main(self): self.target()\n class A:\n  def target(self): pass\n  def main(self): self.target()\n",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, code)]);
        let targets = find(&mut s, path, "target");
        let mains = find(&mut s, path, "main");
        assert_eq!(mains.len(), 2);
        for (target, main) in targets.iter().zip(mains) {
            let calls = relations(&mut s, &main, "calls");
            assert_eq!(candidates(&calls).len(), 1, "{calls}");
            assert_eq!(candidates(&calls)[0]["symbol_id"], target["symbol_id"]);
        }
    }
}

#[test]
fn rust_separate_impls_of_the_same_type_keep_their_links() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "struct A; impl A { fn target(&self) {} } impl A { fn main(&self) { self.target(); Self::target(self); A::target(self); } }",
    )]);
    let main = symbol(&mut s, "lib.rs", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(candidates(&calls).len(), 3, "{calls}");
}

#[test]
fn rust_const_parameters_are_scoped_to_their_own_type() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "const SIZE: usize = 1;\nstruct Buffer<const SIZE: usize> { data: [u8; SIZE] }\nfn main() { let _ = SIZE; }\n",
    )]);
    let size = symbol(&mut s, "lib.rs", "SIZE");
    let refs = relations(&mut s, &size, "references");
    assert_eq!(refs["total_results"], 1, "{refs}");
    assert_eq!(refs["relations"][0]["line"], 3);
}

#[test]
fn rust_import_paths_ignore_comments_without_losing_real_references() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "mod store { pub fn target() {} } use crate /* x */ :: store::{target as invoke}; fn main() { invoke(); }",
    )]);
    let main = symbol(&mut s, "lib.rs", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(candidates(&calls).len(), 1, "{calls}");
}

#[test]
fn csharp_expression_properties_conversions_and_destructors_own_calls() {
    for (name, declaration) in [
        ("Value", "int Value => Target();"),
        ("this", "int this[int index] => Target();"),
        (
            "implicit operator int",
            "public static implicit operator int(A value) { System.Func<int> nested = () => Target(); return Target(); }",
        ),
        (
            "~A",
            "~A() { System.Func<int> nested = () => Target(); Target(); }",
        ),
    ] {
        let code = format!("class A {{ static int Target() => 1; {declaration} }}");
        let (_dir, mut s) = setup(&[("A.cs", &code)]);
        let owner = symbol(&mut s, "A.cs", name);
        let calls = relations(&mut s, &owner, "calls");
        assert_eq!(calls["total_results"], 1, "{name}: {calls}");
        assert_eq!(
            calls["relations"][0]["enclosing_symbol"]["symbol_id"],
            owner["symbol_id"]
        );
    }
}

#[test]
fn javascript_default_arguments_remain_owned_by_the_called_function() {
    let (_dir, mut s) = setup(&[(
        "a.js",
        "function target() {} function outer() { function inner(x = target()) {} return inner; }",
    )]);
    let inner = symbol(&mut s, "a.js", "inner");
    let outer = symbol(&mut s, "a.js", "outer");
    assert_eq!(relations(&mut s, &inner, "calls")["total_results"], 1);
    assert_eq!(relations(&mut s, &outer, "calls")["total_results"], 0);
}

#[test]
fn duplicate_display_paths_keep_distinct_search_ids_and_read_bounds() {
    let (_dir, mut s) = setup(&[(
        "a.js",
        "function outer() {\n { class A {\n  target() { return 'first'; }\n } }\n { class A {\n  target() { return 'second'; }\n } }\n}\n",
    )]);
    let targets = find(&mut s, "a.js", "target");
    assert_eq!(targets.len(), 2);
    assert_eq!(targets[0]["qualified_name"], targets[1]["qualified_name"]);
    assert_ne!(targets[0]["symbol_id"], targets[1]["symbol_id"]);
    for (index, target) in targets.iter().enumerate() {
        let body = run(
            &mut s,
            "symbol_read",
            json!({"path":"a.js","symbol_id":target["symbol_id"]}),
        );
        assert!(
            body["content"]["text"]
                .as_str()
                .unwrap()
                .contains(if index == 0 { "first" } else { "second" })
        );
        assert!(tools::execute(&mut s, "symbol_read", json!({"path":"a.js","symbol_id":target["symbol_id"],"start_line":targets[1-index]["start_line"]})).is_err());
    }
}

#[test]
fn csharp_verbatim_namespace_imports_keep_source_spelling() {
    let (_dir, mut s) = setup(&[
        (
            "Store.cs",
            "namespace @Shop.@Data { class @Store { public static void @Save() {} } }",
        ),
        (
            "Main.cs",
            "using Alias = Shop.Data.Store; class Main { void Run() { Alias.Save(); global::Shop.Data.Store.Save(); } }",
        ),
    ]);
    let main = symbol(&mut s, "Main.cs", "Run");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(candidates(&calls).len(), 2, "{calls}");
    assert_eq!(
        candidates(&calls)[0]["qualified_name"],
        "@Shop.@Data::@Store::@Save"
    );
}

#[test]
fn rust_function_local_modules_keep_self_and_super_paths_separate() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "fn target() {} mod local { pub fn target() {} } fn main() { mod local { pub fn target() {} pub fn run() { self::target(); super::target(); } } }",
    )]);
    let main = symbol(&mut s, "lib.rs", "run");
    let calls = relations(&mut s, &main, "calls");
    let names: Vec<_> = candidates(&calls)
        .iter()
        .map(|c| c["qualified_name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["main::local::target", "target"], "{calls}");
}

#[test]
fn rust_use_paths_bind_at_the_import_location() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "fn main() { mod local { pub fn target() {} } use local::target as invoke; { mod local { pub fn target() {} } invoke(); } }",
    )]);
    let main = symbol(&mut s, "lib.rs", "main");
    let targets = find(&mut s, "lib.rs", "target");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(candidates(&calls).len(), 1, "{calls}");
    assert_eq!(candidates(&calls)[0]["symbol_id"], targets[0]["symbol_id"]);
}

#[test]
fn repeated_local_modules_keep_explicit_self_paths_in_their_own_block() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "fn main() {\n { mod local { pub fn target() {} pub fn run() { self::target(); } } }\n { mod local { pub fn target() {} pub fn run() { self::target(); } } }\n}",
    )]);
    let targets = find(&mut s, "lib.rs", "target");
    let runs = find(&mut s, "lib.rs", "run");
    for (target, run) in targets.iter().zip(runs) {
        let calls = relations(&mut s, &run, "calls");
        assert_eq!(candidates(&calls).len(), 1, "{calls}");
        assert_eq!(candidates(&calls)[0]["symbol_id"], target["symbol_id"]);
    }
}

#[test]
fn rust_receiver_comments_cannot_turn_member_access_into_a_module_path() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "struct A; impl A { fn target(&self) {} fn main(&self) { self /* :: */ . target(); } }",
    )]);
    let main = symbol(&mut s, "lib.rs", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(candidates(&calls).len(), 1, "{calls}");
    assert_eq!(
        calls["relations"][0]["expression"],
        "self /* :: */ . target()"
    );
}

#[test]
fn incomplete_source_keeps_search_and_symbol_reads_bounded() {
    for (path, source) in [
        ("a.rs", "fn valid() {}\nfn broken("),
        ("a.js", "function valid() {}\nfunction broken("),
        ("a.ts", "function valid(): void {}\nfunction broken("),
        ("a.py", "def valid(): pass\ndef broken("),
        ("A.java", "class A {\n void valid() {}\n void broken("),
        ("A.cs", "class A {\n void valid() {}\n void broken("),
    ] {
        let (_dir, mut s) = setup(&[(path, source)]);
        let found = run(&mut s, "symbol_search", json!({"path":path,"limit":100}));
        assert_eq!(found["parse_error_count"], 1, "{path}: {found}");
        assert!(
            found["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["name"] == "valid")
        );
        for symbol in found["symbols"].as_array().unwrap() {
            let body = run(
                &mut s,
                "symbol_read",
                json!({"path":path,"symbol_id":symbol["symbol_id"]}),
            );
            assert_eq!(body["hash"], symbol["hash"]);
            assert_eq!(body["read_start"], symbol["start_line"]);
            assert!(
                body["source"]["end_line"].as_u64().unwrap()
                    <= symbol["end_line"].as_u64().unwrap()
            );
        }
    }
}

#[test]
fn csharp_indexer_parameters_do_not_shadow_unrelated_methods() {
    let (_dir, mut s) = setup(&[(
        "A.cs",
        "class A { int target; int this[int target] { get { return target; } } int Main() { return target; } }",
    )]);
    let target = symbol(&mut s, "A.cs", "target");
    let refs = relations(&mut s, &target, "references");
    assert_eq!(refs["total_results"], 1, "{refs}");
    assert_eq!(refs["relations"][0]["enclosing_symbol"]["name"], "Main");
}

#[test]
fn csharp_setter_value_is_an_implicit_parameter() {
    let (_dir, mut s) = setup(&[(
        "A.cs",
        "class A { void value() {} System.Action Callback { set { value(); } } void Main() { value(); } }",
    )]);
    let target = symbol(&mut s, "A.cs", "value");
    let refs = relations(&mut s, &target, "callers");
    assert_eq!(refs["total_results"], 1, "{refs}");
    assert_eq!(refs["relations"][0]["enclosing_symbol"]["name"], "Main");
}

#[test]
fn rust_inline_modules_can_contain_external_submodules() {
    let (_dir, mut s) = setup(&[
        (
            "lib.rs",
            "mod store { pub mod disk; } fn main() { store::disk::target(); }",
        ),
        ("store/disk.rs", "pub fn target() {}"),
    ]);
    let main = symbol(&mut s, "lib.rs", "main");
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(candidates(&calls).len(), 1, "{calls}");
    assert_eq!(candidates(&calls)[0]["path"], "store/disk.rs");
}

#[test]
fn quoted_export_names_preserve_literal_quote_characters() {
    let (_dir, mut s) = setup(&[
        (
            "a.js",
            "function quoted() {} function plain() {} export { quoted as \"'special'\", plain as special };",
        ),
        (
            "main.js",
            "import { \"'special'\" as quoted, special as plain } from './a.js'; function main() { quoted(); plain(); }",
        ),
    ]);
    let main = symbol(&mut s, "main.js", "main");
    let calls = relations(&mut s, &main, "calls");
    let names: Vec<_> = candidates(&calls)
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["quoted", "plain"], "{calls}");
}

#[test]
fn rust_type_paths_and_constructors_do_not_reference_impl_blocks() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "struct Store; impl Store { fn save() {} } fn main() { let _: crate::Store; let _ = Store; }",
    )]);
    let found = find(&mut s, "lib.rs", "Store");
    let implementation = found.iter().find(|s| s["symbol_kind"] == "impl").unwrap();
    let refs = relations(&mut s, implementation, "references");
    assert_eq!(refs["total_results"], 0, "{refs}");
    let declaration = found.iter().find(|s| s["symbol_kind"] == "struct").unwrap();
    let refs = relations(&mut s, declaration, "references");
    assert_eq!(refs["total_results"], 2, "{refs}");
    for row in refs["relations"].as_array().unwrap() {
        assert_eq!(row["candidate_count"], 1);
    }
}

#[test]
fn qualified_csharp_type_references_ignore_value_parameters() {
    let (_dir, mut s) = setup(&[
        ("Store.cs", "namespace Shop { class Store {} }"),
        (
            "Main.cs",
            "using Types = Shop; class Main { void Run(int Types) { Types.Store item; } }",
        ),
    ]);
    let store = symbol(&mut s, "Store.cs", "Store");
    let refs = relations(&mut s, &store, "references");
    assert_eq!(refs["total_results"], 1, "{refs}");
}
