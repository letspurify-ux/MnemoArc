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
            ..Default::default()
        },
    );
    (dir, session)
}
fn run(s: &mut Session, name: &str, args: Value) -> Value {
    tools::execute(s, name, args).unwrap()
}
fn symbol(s: &mut Session, path: &str, name: &str, kind: Option<&str>) -> Value {
    let result = run(
        s,
        "symbol_search",
        json!({"path":path,"query":name,"match":"exact","kind":kind}),
    );
    assert_eq!(result["parse_error_count"], 0, "fixture must parse");
    assert_eq!(
        result["total_results"], 1,
        "fixture must select exactly one declaration"
    );
    result["symbols"][0].clone()
}
fn relations(s: &mut Session, symbol: &Value, relation: &str) -> Value {
    run(
        s,
        "symbol_relations",
        json!({"path":symbol["path"],"symbol_id":symbol["symbol_id"],"relation":relation}),
    )
}
fn candidate_names(result: &Value) -> Vec<String> {
    result["relations"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|r| r["candidates"].as_array().unwrap())
        .map(|c| c["qualified_name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn typescript_qualified_type_does_not_reference_unrelated_local_type() {
    let (_dir, mut s) = setup(&[
        ("a.ts", "export class Foo {}"),
        (
            "main.ts",
            "import type * as Types from './a';\nclass Foo {}\ntype Bar = Types.Foo;\n",
        ),
    ]);
    let local = symbol(&mut s, "main.ts", "Foo", None);
    let refs = relations(&mut s, &local, "references");
    assert_eq!(
        refs["total_results"], 0,
        "Types.Foo must not be resolved to local Foo"
    );
}
#[test]
fn typescript_namespace_import_type_is_found() {
    let (_dir, mut s) = setup(&[
        ("a.ts", "export class Foo {}"),
        (
            "main.ts",
            "import type * as Types from './a';\ntype Bar = Types.Foo;\n",
        ),
    ]);
    let exported = symbol(&mut s, "a.ts", "Foo", None);
    assert_eq!(
        relations(&mut s, &exported, "references")["total_results"],
        1
    );
}
#[test]
fn javascript_self_parameter_is_not_this() {
    let (_dir, mut s) = setup(&[(
        "a.js",
        "class A { target() {} main(self) { self.target(); } }",
    )]);
    let target = symbol(&mut s, "a.js", "target", None);
    assert_eq!(relations(&mut s, &target, "callers")["total_results"], 0);
}
#[test]
fn python_reassigned_self_is_not_the_declaring_instance() {
    let (_dir, mut s) = setup(&[(
        "a.py",
        "class A:\n def target(self): pass\n def main(self, other):\n  self = other\n  self.target()\n",
    )]);
    let main = symbol(&mut s, "a.py", "main", None);
    assert!(candidate_names(&relations(&mut s, &main, "calls")).is_empty());
}
#[test]
fn rust_qualified_call_uses_closest_type() {
    let (_dir, mut s) = setup(&[(
        "lib.rs",
        "struct A; impl A { fn target() {} }\nfn main() { struct A; impl A { fn target() {} } A::target(); }",
    )]);
    let main = symbol(&mut s, "lib.rs", "main", None);
    assert_eq!(
        candidate_names(&relations(&mut s, &main, "calls")),
        vec!["main::A::target"]
    );
}
#[test]
fn uninitialized_js_declaration_has_references() {
    let (_dir, mut s) = setup(&[("a.js", "let target; function main() { return target; }")]);
    let target = symbol(&mut s, "a.js", "target", None);
    assert_eq!(relations(&mut s, &target, "references")["total_results"], 1);
}
#[test]
fn uninitialized_java_field_has_references() {
    let (_dir, mut s) = setup(&[(
        "A.java",
        "class A { int target; int main() { return target; } }",
    )]);
    let target = symbol(&mut s, "A.java", "target", None);
    assert_eq!(relations(&mut s, &target, "references")["total_results"], 1);
}
#[test]
fn uninitialized_csharp_field_has_references() {
    let (_dir, mut s) = setup(&[(
        "A.cs",
        "class A { int target; int main() { return target; } }",
    )]);
    let target = symbol(&mut s, "A.cs", "target", None);
    assert_eq!(relations(&mut s, &target, "references")["total_results"], 1);
}
#[test]
fn rust_unit_enum_variant_has_references() {
    let (_dir, mut s) = setup(&[("lib.rs", "enum E { A, B }\nfn main() { let _ = E::A; }")]);
    let target = symbol(&mut s, "lib.rs", "A", Some("enum_member"));
    assert_eq!(relations(&mut s, &target, "references")["total_results"], 1);
}
#[test]
fn direct_export_can_also_have_a_named_alias() {
    let (_dir, mut s) = setup(&[
        (
            "a.js",
            "export function original() {}\nexport { original as renamed };",
        ),
        (
            "main.js",
            "import { renamed } from './a.js';\nfunction main() { renamed(); }",
        ),
    ]);
    let main = symbol(&mut s, "main.js", "main", None);
    assert_eq!(
        candidate_names(&relations(&mut s, &main, "calls")),
        vec!["original"]
    );
}
#[test]
fn default_export_can_also_be_exported_by_name() {
    let (_dir, mut s) = setup(&[
        (
            "a.js",
            "export default function original() {}\nexport { original };",
        ),
        (
            "main.js",
            "import { original } from './a.js';\nfunction main() { original(); }",
        ),
    ]);
    let main = symbol(&mut s, "main.js", "main", None);
    assert_eq!(
        candidate_names(&relations(&mut s, &main, "calls")),
        vec!["original"]
    );
}
const MULTI: &str = "const first = () => {\n  return 1;\n},\nsecond = () => {\n  return 2;\n};\n";
#[test]
fn search_does_not_extend_function_through_next_declarator() {
    let (_dir, mut s) = setup(&[("a.js", MULTI)]);
    let first = symbol(&mut s, "a.js", "first", None);
    assert_eq!(first["end_line"], 3);
}
#[test]
fn symbol_read_rejects_lines_in_next_declarator() {
    let (_dir, mut s) = setup(&[("a.js", MULTI)]);
    let first = symbol(&mut s, "a.js", "first", None);
    let result = tools::execute(
        &mut s,
        "symbol_read",
        json!({"path":"a.js","symbol_id":first["symbol_id"],"start_line":5,"max_lines":1}),
    );
    assert!(
        result.is_err(),
        "line 5 belongs exclusively to second, got {result:?}"
    );
}
#[test]
fn compact_java_constructor_has_outgoing_calls() {
    let (_dir, mut s) = setup(&[(
        "R.java",
        "record R(int x) { R { helper(); } static void helper() {} }",
    )]);
    let constructor = symbol(&mut s, "R.java", "R", Some("constructor"));
    assert_eq!(
        candidate_names(&relations(&mut s, &constructor, "calls")),
        vec!["R::helper"]
    );
}
#[test]
fn file_scoped_namespace_includes_descendant_calls() {
    let (_dir, mut s) = setup(&[(
        "A.cs",
        "namespace A;\nclass B { static void target() {} static void main() { target(); } }",
    )]);
    let ns = symbol(&mut s, "A.cs", "A", Some("module"));
    assert_eq!(
        candidate_names(&relations(&mut s, &ns, "calls")),
        vec!["A::B::target"]
    );
}
#[test]
fn rust_raw_identifier_matches_normal_spelling() {
    let (_dir, mut s) = setup(&[("lib.rs", "fn r#target() {}\nfn main() { target(); }")]);
    let main = symbol(&mut s, "lib.rs", "main", None);
    assert_eq!(
        candidate_names(&relations(&mut s, &main, "calls")),
        vec!["r#target"]
    );
}

#[test]
fn control_initialized_variable_reference() {
    let (_dir, mut s) = setup(&[("a.js", "let target = 1; function main() { return target; }")]);
    let target = symbol(&mut s, "a.js", "target", None);
    assert_eq!(relations(&mut s, &target, "references")["total_results"], 1);
}
#[test]
fn control_real_this_receiver() {
    let (_dir, mut s) = setup(&[("a.js", "class A { target() {} main() { this.target(); } }")]);
    let main = symbol(&mut s, "a.js", "main", None);
    assert_eq!(
        candidate_names(&relations(&mut s, &main, "calls")),
        vec!["A::target"]
    );
}
#[test]
fn control_noninline_export_alias() {
    let (_dir, mut s) = setup(&[
        (
            "a.js",
            "function original() {}\nexport { original as renamed };",
        ),
        (
            "main.js",
            "import { renamed } from './a.js';\nfunction main() { renamed(); }",
        ),
    ]);
    let main = symbol(&mut s, "main.js", "main", None);
    assert_eq!(
        candidate_names(&relations(&mut s, &main, "calls")),
        vec!["original"]
    );
}
#[test]
fn control_explicit_java_constructor() {
    let (_dir, mut s) = setup(&[(
        "R.java",
        "record R(int x) { R(int x) { helper(); this.x = x; } static void helper() {} }",
    )]);
    let constructor = symbol(&mut s, "R.java", "R", Some("constructor"));
    assert_eq!(
        candidate_names(&relations(&mut s, &constructor, "calls")),
        vec!["R::helper"]
    );
}
#[test]
fn control_block_namespace_includes_descendant_calls() {
    let (_dir, mut s) = setup(&[(
        "A.cs",
        "namespace A { class B { static void target() {} static void main() { target(); } } }",
    )]);
    let ns = symbol(&mut s, "A.cs", "A", Some("module"));
    assert_eq!(
        candidate_names(&relations(&mut s, &ns, "calls")),
        vec!["A::B::target"]
    );
}
#[test]
fn control_named_typescript_import() {
    let (_dir, mut s) = setup(&[
        ("a.ts", "export class Foo {}"),
        (
            "main.ts",
            "import type { Foo } from './a';\ntype Bar = Foo;",
        ),
    ]);
    let target = symbol(&mut s, "a.ts", "Foo", None);
    assert_eq!(relations(&mut s, &target, "references")["total_results"], 1);
}
#[test]
fn control_unrelated_symbol_id_and_changed_file_are_rejected() {
    let (dir, mut s) = setup(&[("a.rs", "fn target() {}"), ("b.rs", "fn target() {}")]);
    let target = symbol(&mut s, "a.rs", "target", None);
    assert!(
        tools::execute(
            &mut s,
            "symbol_read",
            json!({"path":"b.rs","symbol_id":target["symbol_id"]})
        )
        .is_err()
    );
    std::fs::write(dir.path().join("a.rs"), "fn target() { let changed = 1; }").unwrap();
    assert!(
        tools::execute(
            &mut s,
            "symbol_read",
            json!({"path":"a.rs","symbol_id":target["symbol_id"]})
        )
        .unwrap_err()
        .to_string()
        .contains("symbol_revision_conflict")
    );
}
#[test]
fn control_non_declarator_symbol_range_is_enforced() {
    let (_dir, mut s) = setup(&[("a.rs", "fn first() {}\nfn second() {}\n")]);
    let first = symbol(&mut s, "a.rs", "first", None);
    assert!(
        tools::execute(
            &mut s,
            "symbol_read",
            json!({"path":"a.rs","symbol_id":first["symbol_id"],"start_line":2})
        )
        .is_err()
    );
}

#[test]
fn qualified_types_keep_type_namespace_and_runtime_calls_separate() {
    for extension in ["ts", "tsx"] {
        let path = format!("main.{extension}");
        let (_dir, mut s) = setup(&[
            ("types.ts", "export class Foo {}"),
            (
                &path,
                "import type * as Types from './types';\nfunction main(Types: number) { type Alias = Types.Foo; }\nfunction runtime() { return new Types.Foo(); }\nfunction generic<Types>() { type Alias = Types.Foo; }",
            ),
        ]);
        let target = symbol(&mut s, "types.ts", "Foo", None);
        let refs = relations(&mut s, &target, "references");
        assert_eq!(refs["total_results"], 1, "{refs}");
        assert_eq!(refs["relations"][0]["line"], 2);
        let runtime = symbol(&mut s, &path, "runtime", None);
        let calls = relations(&mut s, &runtime, "calls");
        assert_eq!(calls["relations"][0]["resolution"], "unresolved");
    }
}

#[test]
fn receivers_follow_parameters_and_do_not_treat_static_parameters_as_instances() {
    for (code, expected) in [
        (
            "class A:\n def target(self): pass\n def main(receiver):\n  receiver.target()\n",
            1,
        ),
        (
            "class A:\n def target(self): pass\n @classmethod\n def main(cls):\n  cls.target()\n",
            1,
        ),
        (
            "class A:\n def target(self): pass\n @staticmethod\n def main(self):\n  self.target()\n",
            0,
        ),
        (
            "class A:\n def target(self): pass\n def main(self, other):\n  this = other\n  this.target()\n",
            0,
        ),
        (
            "class A:\n def target(self): pass\n def main(self, other):\n  def change():\n   nonlocal self\n   self = other\n  self.target()\n",
            0,
        ),
    ] {
        let (_dir, mut s) = setup(&[("a.py", code)]);
        let main = symbol(&mut s, "a.py", "main", None);
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(candidate_names(&calls).len(), expected, "{code}: {calls}");
    }
    for (path, code) in [
        (
            "a.js",
            "class A { target() {} main(Self) { Self.target(); } }",
        ),
        (
            "A.java",
            "class A { void target() {} void main(A self) { self.target(); } }",
        ),
        (
            "A.cs",
            "class A { void target() {} void main(A self) { self.target(); } }",
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, code)]);
        let main = symbol(&mut s, path, "main", None);
        assert!(
            candidate_names(&relations(&mut s, &main, "calls")).is_empty(),
            "{code}"
        );
    }
}

#[test]
fn rust_self_parameters_are_candidates_until_reassigned() {
    for (body, expected) in [("self.target();", 1), ("self = other; self.target();", 0)] {
        let code = format!(
            "struct A; impl A {{ fn target(&self) {{}} fn main(mut self, other: A) {{ {body} }} }}"
        );
        let (_dir, mut s) = setup(&[("lib.rs", &code)]);
        let main = symbol(&mut s, "lib.rs", "main", None);
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(candidate_names(&calls).len(), expected, "{calls}");
    }
}

#[test]
fn local_containers_shadow_outer_members_even_when_the_local_member_is_missing() {
    for (path, code, expected) in [
        (
            "lib.rs",
            "mod A { pub fn target() {} } fn main() { mod A { pub fn target() {} } A::target(); }",
            vec!["main::A::target"],
        ),
        (
            "lib.rs",
            "struct A; impl A { fn target() {} } fn main() { struct A; A::target(); }",
            vec![],
        ),
        (
            "lib.rs",
            "struct A; impl A { fn target() {} } fn main() { struct A; crate::A::target(); }",
            vec!["A::target"],
        ),
        (
            "a.js",
            "class A { static target() {} } function main() { class A { static target() {} } A.target(); }",
            vec!["main::A::target"],
        ),
        (
            "a.js",
            "class A { static target() {} } function main() { class A {} A.target(); }",
            vec![],
        ),
    ] {
        let (_dir, mut s) = setup(&[(path, code)]);
        let main = symbol(&mut s, path, "main", None);
        let calls = relations(&mut s, &main, "calls");
        assert_eq!(candidate_names(&calls), expected, "{code}: {calls}");
    }
}

#[test]
fn raw_rust_names_work_in_module_paths_imports_and_shadowing() {
    let code = "mod r#store { pub fn r#target() {} } use crate::store::target as r#save; fn main() { save(); r#store::r#target(); } fn blocked(save: fn()) { r#save(); }";
    let (_dir, mut s) = setup(&[("lib.rs", code)]);
    let main = symbol(&mut s, "lib.rs", "main", None);
    let calls = relations(&mut s, &main, "calls");
    assert_eq!(
        candidate_names(&calls),
        vec!["r#store::r#target", "r#store::r#target"]
    );
    assert_eq!(calls["relations"][1]["expression"], "r#store::r#target()");
    let blocked = symbol(&mut s, "lib.rs", "blocked", None);
    assert!(candidate_names(&relations(&mut s, &blocked, "calls")).is_empty());
}

#[test]
fn later_declarators_do_not_include_previous_bodies_or_claim_complete_signatures() {
    let (_dir, mut s) = setup(&[("a.js", MULTI)]);
    let second = symbol(&mut s, "a.js", "second", None);
    assert_eq!(second["start_line"], 4);
    let body = run(
        &mut s,
        "symbol_read",
        json!({"path":"a.js","symbol_id":second["symbol_id"]}),
    );
    assert!(
        !body["content"]["text"]
            .as_str()
            .unwrap()
            .contains("return 1")
    );
    assert!(
        tools::execute(
            &mut s,
            "symbol_read",
            json!({"path":"a.js","symbol_id":second["symbol_id"],"start_line":2})
        )
        .is_err()
    );
    let outline = run(
        &mut s,
        "code_outline",
        json!({"path":"a.js","query":"second","match":"exact"}),
    );
    let signature = &outline["symbols"][0];
    assert_eq!(signature["signature"], "second = () =>");
    assert_eq!(signature["signature_truncated"], true);
    assert_eq!(signature["signature_context_start_line"], 1);
    assert_eq!(signature["signature_source"]["start_line"], 4);
}

#[test]
fn compact_constructors_exclude_lambda_calls_but_keep_their_callers_location() {
    let (_dir, mut s) = setup(&[(
        "R.java",
        "record R(int x) { R { helper(); new Thread(() -> nested()); } static void helper() {} static void nested() {} }",
    )]);
    let constructor = symbol(&mut s, "R.java", "R", Some("constructor"));
    let calls = relations(&mut s, &constructor, "calls");
    assert_eq!(candidate_names(&calls), vec!["R::helper"]);
    assert_eq!(
        calls["relations"][0]["enclosing_symbol"]["symbol_id"],
        constructor["symbol_id"]
    );
    let nested = symbol(&mut s, "R.java", "nested", None);
    let callers = relations(&mut s, &nested, "callers");
    assert_eq!(callers["total_results"], 1);
    assert_eq!(
        callers["relations"][0]["enclosing_callable"]["anonymous"],
        true
    );
}

#[test]
fn uninitialized_csharp_enum_members_and_exported_js_variables_are_references() {
    let (_dir, mut s) = setup(&[(
        "A.cs",
        "enum E { A, B } class C { E main() { return E.A; } }",
    )]);
    let target = symbol(&mut s, "A.cs", "A", Some("enum_member"));
    assert_eq!(relations(&mut s, &target, "references")["total_results"], 1);
    let (_dir, mut s) = setup(&[
        ("a.js", "export let target;"),
        (
            "main.js",
            "import { target } from './a.js'; function main() { return target; }",
        ),
    ]);
    let target = symbol(&mut s, "a.js", "target", None);
    assert_eq!(relations(&mut s, &target, "references")["total_results"], 1);
}
