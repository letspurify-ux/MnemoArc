//! Conservative syntax links. No type checker, package loader, macro expansion
//! or runtime dispatch is implied by a link, including a local resolved link.
use super::*;
use tree_sitter::Node;

type Key = (usize, usize);
type Span = std::ops::Range<usize>;
const MAX_SITES: usize = 200_000;

#[derive(Clone)]
struct Declaration {
    key: Key,
    name: String,
    span: Span,
    name_span: Span,
    scope: Span,
    kind: String,
    container: String,
}

struct Binding {
    name: String,
    span: Span,
    scope: Span,
    write: bool,
}

struct UncertainBinding {
    name: String,
    span: Span,
    scope: Span,
    own_scope: Span,
}

#[derive(Clone)]
struct Import {
    alias: String,
    module: String,
    member: Option<String>,
    // `import pkg.mod` binds `pkg`, but only `pkg.mod.member` addresses mod.
    module_receiver: Option<String>,
    scope: Span,
    at: usize,
    type_only: bool,
}

#[derive(Clone)]
struct Site {
    file: usize,
    name: String,
    qualifier: Option<String>,
    span: Span,
    name_span: Span,
    line: usize,
    end_line: usize,
    name_line: usize,
    column: usize,
    call: bool,
    dynamic_callee: bool,
    name_lookup: Option<NameLookup>,
    attribute_type_name: bool,
    verbatim_name: bool,
    verbatim_receiver: bool,
    scoped_path: bool,
    owner: Option<Key>,
    callable: Option<(usize, usize)>,
}

#[derive(Clone, Copy)]
enum NameLookup {
    Method,
    Type,
}

impl NameLookup {
    fn accepts(self, kind: &str) -> bool {
        match self {
            Self::Method => kind == "method",
            Self::Type => matches!(
                kind,
                "class"
                    | "struct"
                    | "interface"
                    | "trait"
                    | "record"
                    | "enum"
                    | "annotation"
                    | "delegate"
                    | "type"
            ),
        }
    }
}

struct FileFacts {
    declarations: Vec<Declaration>,
    bindings: Vec<Binding>,
    type_parameters: Vec<Binding>,
    imports: Vec<Import>,
    wildcard_import: bool,
    package: String,
    module_scopes: Vec<Span>,
    uncertain_bindings: Vec<UncertainBinding>,
    python_scopes: BTreeMap<(usize, usize), PythonScope>,
}

struct PythonScope {
    class: bool,
    // A comprehension's first iterable is evaluated in its enclosing scope.
    outer_expression: Option<Span>,
}

fn text_of<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn unquote(name: &str) -> &str {
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'\'' | b'"') && bytes.last() == Some(&bytes[0]) {
        &name[1..name.len() - 1]
    } else {
        name
    }
}

fn csharp_identifier(name: &str) -> &str {
    name.strip_prefix('@').unwrap_or(name)
}

fn identifier<'a>(language: &str, name: &'a str) -> &'a str {
    match language {
        "csharp" => csharp_identifier(name),
        "rust" => name.strip_prefix("r#").unwrap_or(name),
        _ => name,
    }
}

fn rust_path(path: &str) -> String {
    path.split("::")
        .map(|part| identifier("rust", part))
        .collect::<Vec<_>>()
        .join("::")
}

fn csharp_path(path: &str) -> String {
    path.strip_prefix("global::")
        .unwrap_or(path)
        .split('.')
        .map(|part| csharp_identifier(part.trim()))
        .collect::<Vec<_>>()
        .join(".")
}

// Paths are token sequences. Whitespace and comments between their segments
// do not change the name, while the original expression remains source text.
fn path_text(node: Node<'_>, source: &str) -> String {
    let mut result = String::new();
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "comment" | "line_comment" | "block_comment") {
            continue;
        }
        if node.child_count() == 0 {
            result.push_str(text_of(node, source));
        } else {
            let mut cursor = node.walk();
            stack.extend(
                node.children(&mut cursor)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev(),
            );
        }
    }
    result
}

fn named(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn has_type_modifier(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| child.kind() == "type")
}

fn is_function(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_item"
            | "function_definition"
            | "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "arrow_function"
            | "method_definition"
            | "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration"
            | "accessor_declaration"
            | "operator_declaration"
            | "conversion_operator_declaration"
            | "destructor_declaration"
            | "local_function_statement"
            | "lambda"
            | "lambda_expression"
            | "closure_expression"
            | "anonymous_method_expression"
    ) || (matches!(node.kind(), "property_declaration" | "indexer_declaration")
        && node
            .child_by_field_name("value")
            .is_some_and(|value| value.kind() == "arrow_expression_clause"))
}

fn scope(node: Node<'_>, parameter: bool, language: &str) -> Span {
    let mut current = node.parent();
    let mut root = node;
    while let Some(parent) = current {
        root = parent;
        if is_function(parent)
            || (parameter && parent.kind() == "indexer_declaration")
            || (parameter && parent.kind() == "class_definition")
            || (!parameter
                && matches!(
                    parent.kind(),
                    "block"
                        | "statement_block"
                        | "class_body"
                        | "declaration_list"
                        | "class_definition"
                        | "mod_item"
                        | "for_statement"
                        | "for_in_statement"
                        | "for_expression"
                        | "foreach_statement"
                        | "enhanced_for_statement"
                        | "catch_clause"
                        | "match_arm"
                ))
        {
            return if language == "python" {
                parent
                    .child_by_field_name("body")
                    .unwrap_or(parent)
                    .byte_range()
            } else {
                parent.byte_range()
            };
        }
        current = parent.parent();
    }
    root.byte_range()
}

fn symbol_span(symbol: &Value) -> Span {
    let mut parts = symbol["symbol_id"].as_str().unwrap().rsplit(':');
    let end = parts.next().unwrap().parse().unwrap();
    let start = parts.next().unwrap().parse().unwrap();
    start..end
}

fn exact_node(root: Node<'_>, span: Span) -> Option<Node<'_>> {
    let mut node = root.descendant_for_byte_range(span.start, span.end)?;
    loop {
        if node.byte_range() == span {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn declaration_node<'a>(root: Node<'a>, symbol: &Value) -> Option<Node<'a>> {
    let span = symbol_span(symbol);
    let mut node = exact_node(root, span.clone())?;
    // An uninitialized declarator or unit enum variant can share its entire
    // span with its name. Recover the declaration, not the smaller identifier.
    while node.byte_range() == span {
        if Some(node.kind()) == symbol["kind"].as_str() {
            return Some(node);
        }
        node = node.parent()?;
    }
    None
}

fn identifiers(node: Node<'_>) -> Vec<Node<'_>> {
    let mut stack = vec![node];
    let mut result = Vec::new();
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "identifier"
                | "self"
                | "shorthand_property_identifier_pattern"
                | "shorthand_field_identifier"
                | "implicit_parameter"
        ) {
            result.push(node);
        } else if matches!(
            node.kind(),
            "assignment_pattern" | "object_assignment_pattern"
        ) {
            stack.extend(node.child_by_field_name("left"));
        } else if node.kind() == "pair_pattern" {
            stack.extend(node.child_by_field_name("value"));
        } else if node.kind() == "field_pattern" {
            stack.extend(
                node.child_by_field_name("pattern")
                    .or_else(|| node.child_by_field_name("name")),
            );
        } else if matches!(node.kind(), "struct_pattern" | "tuple_struct_pattern") {
            stack.extend(
                named(node)
                    .into_iter()
                    .filter(|child| Some(*child) != node.child_by_field_name("type")),
            );
        } else if node.kind() == "dotted_name" {
            // A one-name Python match pattern captures; a dotted pattern reads
            // an attribute and never introduces a local name.
            if node.named_child_count() == 1 {
                stack.extend(named(node));
            }
        } else if node.kind() == "class_pattern" {
            stack.extend(
                named(node)
                    .into_iter()
                    .filter(|child| child.kind() == "case_pattern"),
            );
        } else if node.kind() == "keyword_pattern" {
            stack.extend(named(node).into_iter().skip(1));
        } else if node.kind() == "dict_pattern" {
            let mut cursor = node.walk();
            stack.extend(node.children_by_field_name("value", &mut cursor));
            stack.extend(
                named(node)
                    .into_iter()
                    .filter(|child| child.kind() == "splat_pattern"),
            );
        } else if !matches!(
            node.kind(),
            "type_annotation"
                | "type_identifier"
                | "member_expression"
                | "field_expression"
                | "attribute"
                | "member_access_expression"
                | "scoped_identifier"
                | "scoped_type_identifier"
                | "subscript"
                | "subscript_expression"
                | "call"
                | "call_expression"
                | "generic_type"
        ) {
            stack.extend(named(node).into_iter().rev());
        }
    }
    result
}

fn declaration_scope(node: Node<'_>, language: &str) -> Span {
    if node.kind() == "class" && matches!(language, "javascript" | "typescript" | "typescriptreact")
    {
        return node.byte_range();
    }
    let function_scoped = language == "python"
        || (matches!(language, "javascript" | "typescript" | "typescriptreact")
            && node.kind() == "variable_declarator"
            && node
                .parent()
                .is_some_and(|n| n.kind() == "variable_declaration"));
    scope(node, function_scoped, language)
}

// Rust modules do not inherit the lexical names of their parent module.
fn visible(facts: &FileFacts, scope: &Span, at: usize) -> bool {
    if !scope.contains(&at)
        || facts.module_scopes.iter().any(|module| {
            module.contains(&at) && scope.start < module.start && scope.end >= module.end
        })
    {
        return false;
    }
    if let Some(python) = facts.python_scopes.get(&(scope.start, scope.end)) {
        if python
            .outer_expression
            .as_ref()
            .is_some_and(|span| span.contains(&at))
        {
            return false;
        }
        // Python class namespaces are used by their body and method defaults,
        // but skipped by nested functions, classes and comprehension bodies.
        if python.class
            && facts.python_scopes.iter().any(|(&(start, end), inner)| {
                start > scope.start
                    && end <= scope.end
                    && (start..end).contains(&at)
                    && !inner
                        .outer_expression
                        .as_ref()
                        .is_some_and(|span| span.contains(&at))
            })
        {
            return false;
        }
    }
    true
}

fn rust_import(
    node: Node<'_>,
    source: &str,
    prefix: &str,
    span: &Span,
    imports: &mut Vec<Import>,
    wildcard: &mut bool,
) {
    let join = |value: &str| {
        if prefix.is_empty() {
            value.to_owned()
        } else {
            format!("{prefix}::{value}")
        }
    };
    match node.kind() {
        "scoped_use_list" => {
            let path = node
                .child_by_field_name("path")
                .map(|p| path_text(p, source))
                .unwrap_or_default();
            if let Some(list) = node.child_by_field_name("list") {
                rust_import(list, source, &join(&path), span, imports, wildcard);
            }
        }
        "use_list" => {
            for child in named(node) {
                rust_import(child, source, prefix, span, imports, wildcard);
            }
        }
        "use_wildcard" => *wildcard = true,
        _ => {
            let path = node.child_by_field_name("path").unwrap_or(node);
            let full = if text_of(path, source) == "self" && !prefix.is_empty() {
                prefix.to_owned()
            } else {
                join(&path_text(path, source))
            };
            let alias = node
                .child_by_field_name("alias")
                .map(|n| text_of(n, source))
                .unwrap_or_else(|| full.rsplit("::").next().unwrap_or(""));
            imports.push(Import {
                alias: alias.into(),
                module: full.clone(),
                member: None,
                module_receiver: None,
                scope: span.clone(),
                at: node.start_byte(),
                type_only: false,
            });
        }
    }
}

fn imports(node: Node<'_>, source: &str, facts: &mut FileFacts, language: &str) -> bool {
    let span = scope(node, language == "python", language);
    match (language, node.kind()) {
        ("javascript" | "typescript" | "typescriptreact", "import_statement") => {
            let Some(module) = node.child_by_field_name("source") else {
                return true;
            };
            let module = unquote(text_of(module, source));
            let type_only_statement =
                matches!(language, "typescript" | "typescriptreact") && has_type_modifier(node);
            let mut stack = named(node);
            while let Some(child) = stack.pop() {
                match child.kind() {
                    "import_specifier" => {
                        if let Some(name) = child.child_by_field_name("name") {
                            let alias = child.child_by_field_name("alias").unwrap_or(name);
                            facts.imports.push(Import {
                                alias: text_of(alias, source).into(),
                                module: module.into(),
                                member: Some(unquote(text_of(name, source)).into()),
                                module_receiver: None,
                                scope: span.clone(),
                                at: node.start_byte(),
                                type_only: type_only_statement || has_type_modifier(child),
                            });
                        }
                    }
                    "namespace_import" => {
                        if let Some(alias) = named(child).last() {
                            facts.imports.push(Import {
                                alias: text_of(*alias, source).into(),
                                module: module.into(),
                                member: None,
                                module_receiver: None,
                                scope: span.clone(),
                                at: node.start_byte(),
                                type_only: type_only_statement,
                            });
                        }
                    }
                    "identifier" if child.parent().is_some_and(|p| p.kind() == "import_clause") => {
                        facts.imports.push(Import {
                            alias: text_of(child, source).into(),
                            module: module.into(),
                            member: Some("default".into()),
                            module_receiver: None,
                            scope: span.clone(),
                            at: node.start_byte(),
                            type_only: type_only_statement,
                        });
                    }
                    "import_clause" | "named_imports" => stack.extend(named(child)),
                    _ => {}
                }
            }
            true
        }
        ("python", "import_statement" | "import_from_statement") => {
            let from = node.child_by_field_name("module_name");
            facts.wildcard_import |= named(node).iter().any(|n| n.kind() == "wildcard_import");
            let mut cursor = node.walk();
            for item in node.children_by_field_name("name", &mut cursor) {
                let name = item.child_by_field_name("name").unwrap_or(item);
                let original = text_of(name, source);
                let alias = item
                    .child_by_field_name("alias")
                    .map(|n| text_of(n, source));
                let module = from.map(|n| text_of(n, source)).unwrap_or(original);
                facts.imports.push(Import {
                    alias: alias
                        .unwrap_or_else(|| {
                            if from.is_some() {
                                original
                            } else {
                                original.split('.').next().unwrap_or(original)
                            }
                        })
                        .into(),
                    module: module.into(),
                    member: from.map(|_| original.into()),
                    module_receiver: from.is_none().then(|| alias.unwrap_or(original).into()),
                    scope: span.clone(),
                    at: node.start_byte(),
                    type_only: false,
                });
            }
            true
        }
        ("rust", "use_declaration") => {
            if let Some(arg) = node.child_by_field_name("argument") {
                rust_import(
                    arg,
                    source,
                    "",
                    &span,
                    &mut facts.imports,
                    &mut facts.wildcard_import,
                );
            }
            true
        }
        ("java", "import_declaration") => {
            let value = text_of(node, source)
                .trim()
                .trim_start_matches("import")
                .trim()
                .trim_end_matches(';')
                .trim();
            let (static_import, value) = value
                .strip_prefix("static ")
                .map_or((false, value), |v| (true, v.trim()));
            if value.ends_with(".*") {
                facts.wildcard_import = true;
            } else {
                let (module, member) = if static_import {
                    value
                        .rsplit_once('.')
                        .map_or((value, None), |(m, n)| (m, Some(n.to_owned())))
                } else {
                    (value, None)
                };
                facts.imports.push(Import {
                    alias: value.rsplit('.').next().unwrap_or(value).into(),
                    module: module.into(),
                    member,
                    module_receiver: None,
                    scope: span,
                    at: node.start_byte(),
                    type_only: false,
                });
            }
            true
        }
        ("csharp", "using_directive") => {
            let value = text_of(node, source)
                .trim()
                .trim_start_matches("global ")
                .trim_start_matches("using")
                .trim()
                .trim_end_matches(';')
                .trim();
            if let Some((alias, module)) = value.split_once('=') {
                facts.imports.push(Import {
                    alias: alias.trim().into(),
                    module: module.trim().into(),
                    member: None,
                    module_receiver: None,
                    scope: span,
                    at: node.start_byte(),
                    type_only: false,
                });
            } else {
                let (alias, module) = value
                    .strip_prefix("static ")
                    .map_or(("*namespace", value), |v| ("*static", v.trim()));
                facts.imports.push(Import {
                    alias: alias.into(),
                    module: module.into(),
                    member: None,
                    module_receiver: None,
                    scope: span,
                    at: node.start_byte(),
                    type_only: false,
                });
            }
            true
        }
        (_, "package_declaration") => {
            facts.package = text_of(node, source)
                .trim()
                .trim_start_matches("package")
                .trim()
                .trim_end_matches(';')
                .trim()
                .into();
            true
        }
        _ => false,
    }
}

/// Return the selected name and the receiver/path whose type is not inferred.
fn expression<'a>(node: Node<'a>, source: &str) -> Option<(Node<'a>, Option<String>)> {
    let mut node = node;
    while node.kind() == "parenthesized_expression" {
        let children = named(node);
        let mut values = children
            .into_iter()
            .filter(|n| !matches!(n.kind(), "comment" | "line_comment" | "block_comment"));
        let value = values.next()?;
        if values.next().is_some() {
            return None;
        }
        node = value;
    }
    match node.kind() {
        "identifier"
        | "type_identifier"
        | "field_identifier"
        | "property_identifier"
        | "shorthand_property_identifier" => Some((node, None)),
        "generic_function" => expression(node.child_by_field_name("function")?, source),
        "generic_name" => named(node)
            .into_iter()
            .find(|n| n.kind() == "identifier")
            .map(|n| (n, None)),
        "scoped_identifier" | "scoped_type_identifier" => {
            let name = node.child_by_field_name("name")?;
            Some((
                name,
                node.child_by_field_name("path")
                    .map(|p| structure::rust_type_path(p, source)),
            ))
        }
        "dotted_name" => {
            let children = named(node);
            let name = *children.last()?;
            let qualifier = (children.len() > 1).then(|| {
                source[node.start_byte()..name.start_byte()]
                    .trim()
                    .trim_end_matches('.')
                    .trim()
                    .to_owned()
            });
            Some((name, qualifier))
        }
        "member_expression"
        | "field_expression"
        | "attribute"
        | "member_access_expression"
        | "nested_type_identifier"
        | "qualified_name" => {
            let name = node
                .child_by_field_name("property")
                .or_else(|| node.child_by_field_name("field"))
                .or_else(|| node.child_by_field_name("attribute"))
                .or_else(|| node.child_by_field_name("name"))?;
            let (name, _) = expression(name, source)?;
            let object = node
                .child_by_field_name("object")
                .or_else(|| node.child_by_field_name("value"))
                .or_else(|| node.child_by_field_name("expression"))
                .or_else(|| node.child_by_field_name("module"))
                .or_else(|| node.child_by_field_name("qualifier"));
            Some((name, object.map(|p| path_text(p, source))))
        }
        _ => None,
    }
}

fn call_expression<'a>(node: Node<'a>, source: &str) -> Option<(Node<'a>, Option<String>)> {
    match node.kind() {
        "call_expression" | "call" | "invocation_expression" => {
            expression(node.child_by_field_name("function")?, source)
        }
        "method_invocation" => Some((
            node.child_by_field_name("name")?,
            node.child_by_field_name("object")
                .map(|n| path_text(n, source)),
        )),
        "new_expression" => expression(node.child_by_field_name("constructor")?, source),
        "object_creation_expression" => expression(node.child_by_field_name("type")?, source),
        _ => None,
    }
}

fn is_call(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "call_expression"
            | "call"
            | "invocation_expression"
            | "method_invocation"
            | "new_expression"
            | "object_creation_expression"
    )
}

fn is_nonreference_identifier(node: Node<'_>, language: &str) -> bool {
    if node.kind() != "identifier" {
        return false;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    if language == "rust" && matches!(parent.kind(), "lifetime" | "label") {
        return true;
    }
    if parent.kind() == "namespace_export" {
        return true;
    }
    if parent.kind() == "export_specifier" {
        if parent.child_by_field_name("alias") == Some(node) {
            return true;
        }
        if parent.child_by_field_name("name") == Some(node) {
            let mut ancestor = parent.parent();
            while let Some(current) = ancestor {
                if current.kind() == "export_statement" {
                    return current.child_by_field_name("source").is_some();
                }
                ancestor = current.parent();
            }
        }
    }
    if matches!(
        parent.kind(),
        "keyword_argument"
            | "argument"
            | "attribute_argument"
            | "element_value_pair"
            | "tuple_element"
    ) && (parent.child_by_field_name("name") == Some(node)
        || parent.child_by_field_name("key") == Some(node))
    {
        return true;
    }
    if parent.kind() == "labeled_statement" && parent.named_child(0) == Some(node) {
        return true;
    }
    if language == "java" && matches!(parent.kind(), "break_statement" | "continue_statement") {
        return true;
    }
    if language == "csharp" && parent.kind() == "goto_statement" {
        let mut cursor = parent.walk();
        return !parent
            .children(&mut cursor)
            .any(|child| matches!(child.kind(), "case" | "default"));
    }
    false
}

fn type_only_export_name(node: Node<'_>, language: &str) -> bool {
    if !matches!(language, "typescript" | "typescriptreact") {
        return false;
    }
    let Some(specifier) = node.parent().filter(|parent| {
        parent.kind() == "export_specifier" && parent.child_by_field_name("name") == Some(node)
    }) else {
        return false;
    };
    if has_type_modifier(specifier) {
        return true;
    }
    let mut ancestor = specifier.parent();
    while let Some(current) = ancestor {
        if current.kind() == "export_statement" {
            return has_type_modifier(current);
        }
        ancestor = current.parent();
    }
    false
}

fn annotation_type_name(node: Node<'_>, language: &str) -> bool {
    matches!(language, "java" | "csharp")
        && node.parent().is_some_and(|parent| {
            matches!(
                parent.kind(),
                "marker_annotation" | "annotation" | "attribute"
            ) && parent.child_by_field_name("name") == Some(node)
        })
}

fn add_binding(node: Node<'_>, facts: &mut FileFacts, source: &str, language: &str) {
    if language == "csharp"
        && node.kind() == "accessor_declaration"
        && node
            .child_by_field_name("name")
            .is_some_and(|name| matches!(text_of(name, source), "set" | "init" | "add" | "remove"))
    {
        // C# supplies this parameter without a parameter node in the AST.
        facts.bindings.push(Binding {
            name: "value".into(),
            span: node.start_byte()..node.start_byte(),
            scope: node.byte_range(),
            write: false,
        });
    }
    let javascript = matches!(language, "javascript" | "typescript" | "typescriptreact");
    let mut cursor = node.walk();
    let loop_kind = (javascript && node.kind() == "for_in_statement")
        .then(|| {
            node.children_by_field_name("kind", &mut cursor)
                .find(|n| matches!(n.kind(), "var" | "let" | "const" | "using"))
                .map(|n| n.kind())
        })
        .flatten();
    let update = node.kind() == "update_expression"
        || (matches!(
            node.kind(),
            "prefix_unary_expression" | "postfix_unary_expression"
        ) && node
            .children(&mut node.walk())
            .any(|n| matches!(n.kind(), "++" | "--")));
    let write = update
        || matches!(
            node.kind(),
            "assignment"
                | "assignment_expression"
                | "augmented_assignment"
                | "augmented_assignment_expression"
        ) && language != "python"
        || (javascript && node.kind() == "for_in_statement" && loop_kind.is_none());
    let parameter_container = |kind| {
        matches!(
            kind,
            "parameters"
                | "formal_parameters"
                | "lambda_parameters"
                | "closure_parameters"
                | "inferred_parameters"
                | "parameter_list"
                | "implicit_parameter_list"
        )
    };
    let direct_parameter = node.parent().is_some_and(|p| {
        parameter_container(p.kind())
            || (matches!(p.kind(), "arrow_function" | "lambda_expression")
                && (p.child_by_field_name("parameter") == Some(node)
                    || p.child_by_field_name("parameters") == Some(node)))
    });
    let parameter = matches!(
        node.kind(),
        "parameter"
            | "formal_parameter"
            | "spread_parameter"
            | "required_parameter"
            | "optional_parameter"
            | "typed_parameter"
            | "default_parameter"
            | "typed_default_parameter"
    ) || direct_parameter;
    let binding = match node.kind() {
        "self_parameter" => Some(node),
        "let_declaration" | "parameter" => node
            .child_by_field_name("pattern")
            .or_else(|| node.child_by_field_name("name")),
        "variable_declarator"
        | "const_parameter"
        | "formal_parameter"
        | "spread_parameter"
        | "default_parameter"
        | "typed_default_parameter"
        | "named_expression" => node.child_by_field_name("name"),
        "delete_statement" | "case_pattern" if language == "python" => Some(node),
        "required_parameter" | "optional_parameter" => node.child_by_field_name("pattern"),
        "assignment"
        | "assignment_expression"
        | "augmented_assignment"
        | "augmented_assignment_expression" => node.child_by_field_name("left"),
        "for_statement" | "foreach_statement" | "for_in_clause" => node.child_by_field_name("left"),
        "enhanced_for_statement" | "catch_formal_parameter" | "catch_declaration" => {
            node.child_by_field_name("name")
        }
        "for_expression" | "for_in_statement" => node
            .child_by_field_name("pattern")
            .or_else(|| node.child_by_field_name("left")),
        "catch_clause" => node.child_by_field_name("parameter"),
        "let_condition" | "match_arm" => node.child_by_field_name("pattern"),
        "as_pattern" if language == "python" => node.child_by_field_name("alias"),
        "function_expression" | "generator_function" => node.child_by_field_name("name"),
        "typed_parameter" => named(node).into_iter().find(|n| n.kind() == "identifier"),
        _ if update => node.child_by_field_name("argument").or_else(|| {
            named(node)
                .into_iter()
                .find(|n| !matches!(n.kind(), "comment" | "line_comment" | "block_comment"))
        }),
        _ if direct_parameter => Some(node),
        _ => None,
    };
    if let Some(binding) = binding {
        let function_scoped = parameter
            || language == "python"
            || loop_kind == Some("var")
            || (javascript
                && node
                    .parent()
                    .is_some_and(|p| p.kind() == "variable_declaration"));
        let mut binding_scope = scope(node, function_scoped, language);
        if node.kind() == "const_parameter" {
            if let Some(owner) = node.parent().and_then(|list| list.parent()) {
                binding_scope = owner.byte_range();
            }
        } else if matches!(
            node.kind(),
            "function_expression" | "generator_function" | "match_arm"
        ) || (node.kind() == "for_in_statement" && !function_scoped && !write)
        {
            binding_scope = node.byte_range();
        } else if matches!(
            node.kind(),
            "for_expression" | "foreach_statement" | "enhanced_for_statement"
        ) {
            binding_scope = node
                .child_by_field_name("body")
                .unwrap_or(node)
                .byte_range();
        } else if language == "rust" && node.kind() == "let_declaration" {
            // A Rust let binding starts after its initializer (and else block).
            binding_scope.start = node.end_byte();
        } else if node.kind() == "let_condition" {
            let mut ancestor = node.parent();
            while let Some(parent) = ancestor {
                if matches!(parent.kind(), "if_expression" | "while_expression") {
                    // A let condition's names are unavailable in the else branch.
                    binding_scope = parent
                        .child_by_field_name("consequence")
                        .or_else(|| parent.child_by_field_name("body"))
                        .map_or_else(|| parent.byte_range(), |n| n.byte_range());
                    break;
                }
                ancestor = parent.parent();
            }
        } else if node.kind() == "for_in_clause"
            && language == "python"
            && let Some(parent) = node.parent()
        {
            binding_scope = parent.byte_range();
        }
        for name in identifiers(binding) {
            facts.bindings.push(Binding {
                name: text_of(name, source).into(),
                span: name.byte_range(),
                scope: binding_scope.clone(),
                write,
            });
        }
    }
}

fn add_type_parameter(node: Node<'_>, facts: &mut FileFacts, source: &str) {
    if node.kind() != "type_parameter" {
        return;
    }
    let name = node.child_by_field_name("name").or_else(|| {
        named(node)
            .into_iter()
            .find(|child| matches!(child.kind(), "identifier" | "type_identifier"))
    });
    if let (Some(name), Some(owner)) = (name, node.parent().and_then(|list| list.parent())) {
        facts.type_parameters.push(Binding {
            name: text_of(name, source).into(),
            span: name.byte_range(),
            scope: owner.byte_range(),
            write: false,
        });
    }
}

fn assign_write_scopes(
    facts: &mut FileFacts,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<()> {
    // An assignment mutates its binding even when written inside a conditional
    // block or closure. Without control flow analysis, invalidate that binding
    // throughout its scope rather than confirming an obsolete target.
    let mut scopes: BTreeMap<&str, Vec<&Span>> = BTreeMap::new();
    for binding in facts.bindings.iter().filter(|b| !b.write) {
        scopes
            .entry(&binding.name)
            .or_default()
            .push(&binding.scope);
    }
    for declaration in &facts.declarations {
        scopes
            .entry(&declaration.name)
            .or_default()
            .push(&declaration.scope);
    }
    for import in &facts.imports {
        scopes.entry(&import.alias).or_default().push(&import.scope);
    }
    let mut updates = Vec::new();
    for (i, binding) in facts.bindings.iter().enumerate().filter(|(_, b)| b.write) {
        check_budget(cancel, deadline)?;
        if let Some(scope) = scopes
            .get(binding.name.as_str())
            .into_iter()
            .flatten()
            .filter(|scope| visible(facts, scope, binding.span.start))
            .min_by_key(|scope| scope.len())
        {
            updates.push((i, (*scope).clone()));
        }
    }
    for (i, scope) in updates {
        facts.bindings[i].scope = scope;
    }
    Ok(())
}

fn collect(
    workspace: &Workspace,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(Vec<FileFacts>, Vec<Site>, usize)> {
    let mut all_facts = Vec::new();
    let mut sites = Vec::new();
    let mut unsupported_calls = 0;
    for (file_index, file) in workspace.files.iter().enumerate() {
        let source = &file.syntax.source;
        let root = file.syntax.tree.root_node();
        let mut facts = FileFacts {
            declarations: Vec::new(),
            bindings: Vec::new(),
            type_parameters: Vec::new(),
            imports: Vec::new(),
            wildcard_import: false,
            package: String::new(),
            module_scopes: Vec::new(),
            uncertain_bindings: Vec::new(),
            python_scopes: BTreeMap::new(),
        };
        for (index, symbol) in file.symbols.iter().enumerate() {
            let span = symbol_span(symbol);
            let Some(node) = declaration_node(root, symbol) else {
                continue;
            };
            let Some(name) = structure::symbol_name(node) else {
                continue;
            };
            facts.declarations.push(Declaration {
                key: (file_index, index),
                name: if node.kind() == "impl_item" {
                    structure::rust_type_path(name, source)
                } else {
                    text_of(name, source).into()
                },
                span,
                name_span: name.byte_range(),
                scope: declaration_scope(node, file.syntax.language),
                kind: symbol["symbol_kind"].as_str().unwrap().into(),
                container: symbol["container"].as_str().unwrap().into(),
            });
            if file.syntax.language == "rust"
                && node.kind() == "mod_item"
                && let Some(body) = node.child_by_field_name("body")
            {
                facts.module_scopes.push(body.byte_range());
            }
        }
        let declaration_names: BTreeSet<_> = facts
            .declarations
            .iter()
            .map(|d| (d.name_span.start, d.name_span.end))
            .collect();
        let callable_owners: BTreeMap<_, _> = facts
            .declarations
            .iter()
            .filter_map(|d| {
                let node = declaration_node(root, &file.symbols[d.key.1])?;
                let callable = if is_function(node) {
                    Some(node)
                } else {
                    node.child_by_field_name("value")
                        .filter(|&n| is_function(n))
                }?;
                Some(((callable.start_byte(), callable.end_byte()), d.key))
            })
            .collect();
        // A field/constant initializer has its own declaration scope. In a
        // local class its instance initialization runs separately from the
        // enclosing function, even though that function is an AST ancestor.
        let initializer_owners: BTreeMap<_, _> = facts
            .declarations
            .iter()
            .filter(|d| matches!(d.kind.as_str(), "field" | "constant" | "property" | "event"))
            .filter_map(|d| {
                let node = declaration_node(root, &file.symbols[d.key.1])?;
                let value = node.child_by_field_name("value")?;
                Some((
                    (node.start_byte(), node.end_byte()),
                    (value.byte_range(), d.key),
                ))
            })
            .collect();
        let site_start = sites.len();
        let mut stack = vec![root];
        let mut seen = BTreeSet::new();
        while let Some(node) = stack.pop() {
            check_budget(cancel, deadline)?;
            if file.syntax.language == "python" {
                let comprehension = matches!(
                    node.kind(),
                    "list_comprehension"
                        | "set_comprehension"
                        | "dictionary_comprehension"
                        | "generator_expression"
                );
                if comprehension || is_function(node) || node.kind() == "class_definition" {
                    let span = if comprehension {
                        node.byte_range()
                    } else {
                        node.child_by_field_name("body")
                            .unwrap_or(node)
                            .byte_range()
                    };
                    let outer_expression = comprehension
                        .then(|| {
                            named(node)
                                .into_iter()
                                .find(|n| n.kind() == "for_in_clause")
                                .and_then(|n| n.child_by_field_name("right"))
                                .map(|n| n.byte_range())
                        })
                        .flatten();
                    facts.python_scopes.insert(
                        (span.start, span.end),
                        PythonScope {
                            class: node.kind() == "class_definition",
                            outer_expression,
                        },
                    );
                }
            }
            if file.syntax.language == "python"
                && matches!(node.kind(), "global_statement" | "nonlocal_statement")
            {
                let binding_scope = scope(node, true, file.syntax.language);
                facts
                    .uncertain_bindings
                    .extend(identifiers(node).into_iter().map(|name| UncertainBinding {
                        name: text_of(name, source).to_owned(),
                        span: name.byte_range(),
                        scope: binding_scope.clone(),
                        own_scope: binding_scope.clone(),
                    }));
                continue;
            }
            if imports(node, source, &mut facts, file.syntax.language) {
                continue;
            }
            if matches!(
                node.kind(),
                "comment"
                    | "line_comment"
                    | "block_comment"
                    | "string_literal"
                    | "raw_string_literal"
                    | "character_literal"
                    | "macro_invocation"
                    | "macro_definition"
                    | "attribute_item"
                    | "inner_attribute_item"
                    | "token_tree"
            ) {
                continue;
            }
            if is_nonreference_identifier(node, file.syntax.language) {
                continue;
            }
            add_binding(node, &mut facts, source, file.syntax.language);
            if matches!(
                file.syntax.language,
                "rust" | "java" | "csharp" | "typescript" | "typescriptreact"
            ) {
                add_type_parameter(node, &mut facts, source);
            }
            let call = is_call(node);
            let mut value = if file.syntax.language == "csharp" && node.kind() == "attribute" {
                // C# attributes share a node name with Python member access.
                // Visit the attribute's name child so it keeps type lookup.
                None
            } else if call {
                call_expression(node, source)
            } else {
                expression(node, source)
            };
            let dynamic_callee = call && value.is_none();
            if dynamic_callee {
                unsupported_calls += 1;
                value = node
                    .child_by_field_name("function")
                    .or_else(|| node.child_by_field_name("constructor"))
                    .or_else(|| node.child_by_field_name("type"))
                    .map(|callee| (callee, None));
            }
            if let Some((name, qualifier)) = value {
                // Outer expressions are visited first, so call/member sites
                // retain their receiver and are not repeated as bare names.
                if seen.insert((name.start_byte(), name.end_byte()))
                    && !declaration_names.contains(&(name.start_byte(), name.end_byte()))
                    && !name.parent().is_some_and(|p| {
                        (matches!(p.kind(), "pair" | "pair_pattern")
                            && p.child_by_field_name("key") == Some(name))
                            || (p.kind() == "keyword_pattern" && p.named_child(0) == Some(name))
                            || (p.kind() == "field_initializer"
                                && p.child_by_field_name("field") == Some(name))
                            || (p.kind() == "field_pattern"
                                && p.child_by_field_name("name") == Some(name)
                                && p.child_by_field_name("pattern").is_some())
                    })
                {
                    let mut parent = node.parent();
                    let mut callable = None;
                    let mut initializer_owner = None;
                    while let Some(ancestor) = parent {
                        if is_function(ancestor)
                            && (file.syntax.language != "python"
                                || ancestor.child_by_field_name("body").is_some_and(|body| {
                                    body.byte_range().contains(&node.start_byte())
                                }))
                        {
                            callable = Some(ancestor);
                            break;
                        }
                        if let Some((value, key)) =
                            initializer_owners.get(&(ancestor.start_byte(), ancestor.end_byte()))
                            && value.contains(&node.start_byte())
                        {
                            initializer_owner = Some(*key);
                            break;
                        }
                        parent = ancestor.parent();
                    }
                    let owner = initializer_owner.or_else(|| {
                        callable.and_then(|n| {
                            callable_owners
                                .get(&(n.start_byte(), n.end_byte()))
                                .copied()
                        })
                    });
                    let pos = name.start_position();
                    let line_start = name.start_byte() - pos.column;
                    let spelling = text_of(name, source);
                    let verbatim_name =
                        file.syntax.language == "csharp" && spelling.starts_with('@');
                    let verbatim_receiver = file.syntax.language == "csharp"
                        && qualifier.as_deref().is_some_and(|q| q.starts_with('@'));
                    let scoped_path = file.syntax.language == "rust"
                        && name.parent().is_some_and(|parent| {
                            matches!(
                                parent.kind(),
                                "scoped_identifier" | "scoped_type_identifier"
                            )
                        });
                    sites.push(Site {
                        file: file_index,
                        name: identifier(file.syntax.language, spelling).into(),
                        qualifier: qualifier.map(|value| {
                            if file.syntax.language == "rust" {
                                rust_path(&value)
                            } else if file.syntax.language == "csharp" {
                                csharp_path(&value)
                            } else {
                                value
                            }
                        }),
                        span: node.byte_range(),
                        name_span: name.byte_range(),
                        line: node.start_position().row + 1,
                        end_line: node.end_position().row
                            + usize::from(node.end_position().column > 0),
                        column: source[line_start..name.start_byte()].chars().count() + 1,
                        name_line: pos.row + 1,
                        call,
                        dynamic_callee,
                        attribute_type_name: annotation_type_name(node, file.syntax.language),
                        verbatim_name,
                        verbatim_receiver,
                        scoped_path,
                        name_lookup: match (file.syntax.language, node.kind()) {
                            ("java" | "csharp", _)
                                if annotation_type_name(node, file.syntax.language) =>
                            {
                                Some(NameLookup::Type)
                            }
                            ("typescript" | "typescriptreact", "identifier")
                                if type_only_export_name(node, file.syntax.language) =>
                            {
                                Some(NameLookup::Type)
                            }
                            ("java", "method_invocation") => Some(NameLookup::Method),
                            ("typescript" | "typescriptreact", "nested_type_identifier") => {
                                Some(NameLookup::Type)
                            }
                            ("rust" | "java", "scoped_type_identifier")
                            | ("csharp", "qualified_name") => Some(NameLookup::Type),
                            ("java" | "csharp", "object_creation_expression") => {
                                Some(NameLookup::Type)
                            }
                            (
                                "rust" | "java" | "csharp" | "typescript" | "typescriptreact",
                                "type_identifier",
                            ) => Some(NameLookup::Type),
                            ("csharp", "identifier")
                                if name.parent().is_some_and(|parent| {
                                    parent.child_by_field_name("type") == Some(name)
                                }) =>
                            {
                                Some(NameLookup::Type)
                            }
                            _ => None,
                        },
                        owner,
                        callable: callable.map(|n| {
                            (
                                n.start_position().row + 1,
                                n.end_position().row + usize::from(n.end_position().column > 0),
                            )
                        }),
                    });
                    if sites.len() > MAX_SITES {
                        bail!("navigation_too_broad: too many references; narrow path_glob");
                    }
                }
            }
            stack.extend(named(node).into_iter().rev());
        }
        if matches!(file.syntax.language, "csharp" | "rust") {
            for declaration in &mut facts.declarations {
                declaration.name = identifier(file.syntax.language, &declaration.name).to_owned();
            }
            for binding in facts.bindings.iter_mut().chain(&mut facts.type_parameters) {
                binding.name = identifier(file.syntax.language, &binding.name).to_owned();
            }
            for import in &mut facts.imports {
                import.alias = identifier(file.syntax.language, &import.alias).to_owned();
                if file.syntax.language == "rust" {
                    import.module = rust_path(&import.module);
                } else {
                    import.module = csharp_path(&import.module);
                }
            }
        }
        assign_write_scopes(&mut facts, cancel, deadline)?;
        // A global/nonlocal directive affects only its function until that
        // function actually writes the name. After a write, calls in other
        // scopes can no longer be linked to the old declaration either.
        for binding in &mut facts.uncertain_bindings {
            check_budget(cancel, deadline)?;
            let written = facts
                .bindings
                .iter()
                .any(|other| other.name == binding.name && other.scope == binding.scope)
                || facts
                    .declarations
                    .iter()
                    .any(|other| other.name == binding.name && other.scope == binding.scope)
                || facts
                    .imports
                    .iter()
                    .any(|other| other.alias == binding.name && other.scope == binding.scope);
            if !written {
                continue;
            }
            let Some(statement) = exact_node(root, binding.span.clone()).and_then(|n| n.parent())
            else {
                continue;
            };
            if statement.kind() == "global_statement" {
                binding.scope = root.byte_range();
            } else if statement.kind() == "nonlocal_statement" {
                let mut function = statement.parent();
                let mut own_function_seen = false;
                while let Some(ancestor) = function {
                    if is_function(ancestor) {
                        if own_function_seen {
                            let outer_scope = ancestor
                                .child_by_field_name("body")
                                .unwrap_or(ancestor)
                                .byte_range();
                            if facts.bindings.iter().any(|other| {
                                other.name == binding.name && other.scope == outer_scope
                            }) || facts.declarations.iter().any(|other| {
                                other.name == binding.name && other.scope == outer_scope
                            }) || facts.imports.iter().any(|other| {
                                other.alias == binding.name && other.scope == outer_scope
                            }) {
                                binding.scope = outer_scope;
                                break;
                            }
                        } else {
                            own_function_seen = true;
                        }
                    }
                    function = ancestor.parent();
                }
            }
        }
        // Binding occurrences are definitions/writes, not references to the
        // shadowed declaration. Assignment RHS references remain included.
        let binding_names: BTreeSet<_> = facts
            .bindings
            .iter()
            .chain(&facts.type_parameters)
            .map(|b| (b.span.start, b.span.end))
            .collect();
        let file_sites = sites.split_off(site_start);
        sites.extend(
            file_sites.into_iter().filter(|site| {
                !binding_names.contains(&(site.name_span.start, site.name_span.end))
            }),
        );
        all_facts.push(facts);
    }
    Ok((all_facts, sites, unsupported_calls))
}

struct Resolution {
    status: &'static str,
    reason: &'static str,
    targets: Vec<Key>,
}

impl Resolution {
    fn unresolved(reason: &'static str) -> Self {
        Self {
            status: "unresolved",
            reason,
            targets: Vec::new(),
        }
    }
    fn candidates(mut targets: Vec<Key>, reason: &'static str) -> Self {
        targets.sort_unstable();
        targets.dedup();
        Self {
            status: if targets.is_empty() {
                "unresolved"
            } else if targets.len() == 1 {
                "candidate"
            } else {
                "ambiguous"
            },
            reason,
            targets,
        }
    }
}

fn shadowed(facts: &FileFacts, name: &str, at: usize, declaration: Option<&Declaration>) -> bool {
    facts.bindings.iter().any(|b| {
        b.name == name
            && visible(facts, &b.scope, at)
            && declaration.is_none_or(|d| b.span != d.name_span && b.scope.len() <= d.scope.len())
    })
}

fn normalized(base: &Path, relative: &str, root: &Path) -> Option<PathBuf> {
    let mut result = base.to_owned();
    for component in Path::new(relative).components() {
        match component {
            std::path::Component::Normal(p) => result.push(p),
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    result.starts_with(root).then_some(result)
}

fn module_files(workspace: &Workspace, site: &Site, module: &str) -> Vec<usize> {
    let origin = &workspace.files[site.file].syntax;
    let mut paths = Vec::new();
    match origin.language {
        "javascript" | "typescript" | "typescriptreact" if module.starts_with('.') => {
            if let Some(base) = normalized(origin.path.parent().unwrap(), module, &workspace.root) {
                paths.push(base.clone());
                if base.extension().is_none() {
                    for ext in ["js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts"] {
                        paths.push(base.with_extension(ext));
                        paths.push(base.join(format!("index.{ext}")));
                    }
                } else if base.extension().is_some_and(|ext| ext == "js") {
                    paths.push(base.with_extension("ts"));
                    paths.push(base.with_extension("tsx"));
                }
            }
        }
        "python" => {
            let dots = module.chars().take_while(|&c| c == '.').count();
            let mut base = if dots > 0 {
                origin.path.parent().unwrap().to_owned()
            } else {
                workspace.root.clone()
            };
            for _ in 1..dots {
                base.pop();
            }
            if let Some(base) =
                normalized(&base, &module[dots..].replace('.', "/"), &workspace.root)
            {
                if module.len() > dots {
                    paths.push(base.with_extension("py"));
                }
                paths.push(base.join("__init__.py"));
            }
        }
        _ => {}
    }
    workspace
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| paths.contains(&f.syntax.path))
        .map(|(i, _)| i)
        .collect()
}

fn exported(
    file: &IndexedFile,
    declaration: &Declaration,
    wanted: &str,
    type_position: bool,
) -> bool {
    if !matches!(
        file.syntax.language,
        "javascript" | "typescript" | "typescriptreact"
    ) {
        return declaration.name == wanted;
    }
    if matches!(file.syntax.language, "typescript" | "typescriptreact")
        && !type_position
        && matches!(declaration.kind.as_str(), "type" | "interface")
    {
        return false;
    }
    let root = file.syntax.tree.root_node();
    let Some(node) = declaration_node(root, &file.symbols[declaration.key.1]) else {
        return false;
    };
    let mut parent = Some(node);
    while let Some(node) = parent {
        if node.kind() == "export_statement" {
            if has_type_modifier(node)
                && (!type_position || !NameLookup::Type.accepts(&declaration.kind))
            {
                return false;
            }
            let mut cursor = node.walk();
            let default = node
                .children(&mut cursor)
                .any(|child| child.kind() == "default");
            let matches = if wanted == "default" {
                default
            } else {
                declaration.name == wanted && !default
            };
            if matches {
                return true;
            }
            // The same declaration may have additional named/default exports.
            break;
        }
        parent = node.parent();
    }
    for export in named(root)
        .into_iter()
        .filter(|n| n.kind() == "export_statement" && n.child_by_field_name("source").is_none())
    {
        let statement_type_only = has_type_modifier(export);
        if wanted == "default"
            && let Some(value) = export.child_by_field_name("value")
            && value.kind() == "identifier"
            && text_of(value, &file.syntax.source) == declaration.name
        {
            return true;
        }
        let mut stack = named(export);
        while let Some(node) = stack.pop() {
            if node.kind() == "export_specifier" {
                if let Some(name) = node.child_by_field_name("name") {
                    let alias = node.child_by_field_name("alias").unwrap_or(name);
                    if text_of(name, &file.syntax.source) == declaration.name
                        && unquote(text_of(alias, &file.syntax.source)) == wanted
                        && (!(statement_type_only || has_type_modifier(node))
                            || type_position && NameLookup::Type.accepts(&declaration.kind))
                    {
                        return true;
                    }
                }
            } else {
                stack.extend(named(node));
            }
        }
    }
    false
}

fn rust_module(workspace: &Workspace, file: usize) -> (PathBuf, Vec<String>) {
    let path = &workspace.files[file].syntax.path;
    let root = path
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(&workspace.root))
        // A path_glob may omit the crate entry file while including both
        // sides of a link. Its existence still determines their module paths.
        .find(|dir| dir.join("lib.rs").is_file() || dir.join("main.rs").is_file())
        .unwrap_or(&workspace.root)
        .to_owned();
    let relative = path.strip_prefix(&root).unwrap_or(path);
    let mut parts: Vec<String> = relative
        .parent()
        .unwrap_or(Path::new(""))
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let stem = path.file_stem().unwrap().to_string_lossy();
    if !matches!(stem.as_ref(), "lib" | "main" | "mod") {
        parts.push(stem.into_owned());
    }
    (root, parts)
}

struct LinkIndex {
    rust_modules: Vec<(PathBuf, Vec<String>)>,
    rust: BTreeMap<(PathBuf, String), Vec<Key>>,
    types: BTreeMap<(String, String), Vec<Key>>,
}
impl LinkIndex {
    fn new(
        workspace: &Workspace,
        facts: &[FileFacts],
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<Self> {
        let mut result = Self {
            rust_modules: Vec::new(),
            rust: BTreeMap::new(),
            types: BTreeMap::new(),
        };
        for (file, items) in facts.iter().enumerate() {
            check_budget(cancel, deadline)?;
            let language = workspace.files[file].syntax.language;
            let (root, prefix) = if language == "rust" {
                rust_module(workspace, file)
            } else {
                (PathBuf::new(), Vec::new())
            };
            for d in &items.declarations {
                let qualified = workspace.files[file].symbols[d.key.1]["qualified_name"]
                    .as_str()
                    .unwrap();
                if language == "rust" && !matches!(d.kind.as_str(), "macro" | "impl") {
                    // Impl blocks are containers, not type/value bindings.
                    // Macro invocations are not analyzed as expanded calls.
                    let name = rust_path(
                        &prefix
                            .iter()
                            .map(String::as_str)
                            .chain(std::iter::once(qualified))
                            .collect::<Vec<_>>()
                            .join("::"),
                    );
                    result
                        .rust
                        .entry((root.clone(), name))
                        .or_default()
                        .push(d.key);
                } else if matches!(language, "java" | "csharp") {
                    let name = if language == "csharp" {
                        csharp_path(&qualified.replace("::", "."))
                    } else {
                        qualified.replace("::", ".")
                    };
                    let name = if items.package.is_empty() {
                        name
                    } else {
                        format!("{}.{}", items.package, name)
                    };
                    result
                        .types
                        .entry((language.into(), name))
                        .or_default()
                        .push(d.key);
                }
            }
            result.rust_modules.push((root, prefix));
        }
        Ok(result)
    }
}

fn rust_targets(index: &LinkIndex, facts: &[FileFacts], site: &Site, path: &str) -> Vec<Key> {
    let (root, prefix) = &index.rust_modules[site.file];
    let mut inline_modules: Vec<_> = facts[site.file]
        .declarations
        .iter()
        .filter(|d| d.kind == "module" && d.span.contains(&site.span.start))
        .collect();
    inline_modules.sort_by_key(|d| std::cmp::Reverse(d.span.len()));
    let mut parents = vec![(prefix.clone(), None)];
    for declaration in inline_modules {
        let mut module = prefix.clone();
        if !declaration.container.is_empty() {
            module.extend(
                rust_path(&declaration.container)
                    .split("::")
                    .map(str::to_owned),
            );
        }
        module.push(declaration.name.clone());
        parents.push((module, Some(declaration)));
    }
    let (mut module, mut owner) = parents.pop().unwrap();
    let mut parts: Vec<String> = path.split("::").map(str::to_owned).collect();
    if parts.first().is_some_and(|p| p == "crate") {
        module.clear();
        parents.clear();
        owner = None;
        parts.remove(0);
    } else if parts.first().is_some_and(|p| p == "self") {
        parts.remove(0);
    }
    while parts.first().is_some_and(|p| p == "super") {
        // A local module's display path includes its enclosing function.
        // `super` crosses one actual module, not one display-path component.
        if let Some(parent) = parents.pop() {
            (module, owner) = parent;
        } else if module.pop().is_none() {
            return Vec::new();
        }
        parts.remove(0);
    }
    module.extend(parts);
    index
        .rust
        .get(&(root.clone(), rust_path(&module.join("::"))))
        .into_iter()
        .flatten()
        .filter(|&&key| {
            key.0 != site.file
                || owner.is_none_or(|module| {
                    facts[site.file]
                        .declarations
                        .iter()
                        .find(|d| d.key == key)
                        .is_some_and(|target| module.span.contains(&target.span.start))
                })
        })
        .copied()
        .collect()
}

fn container_contains(facts: &FileFacts, container: &Declaration, member: &Declaration) -> bool {
    if container.span.contains(&member.span.start) {
        return true;
    }
    // Rust impls can be separate from their type declaration. A display path
    // alone cannot distinguish same-named local types in sibling blocks.
    facts.declarations.iter().any(|implementation| {
        implementation.kind == "impl"
            && implementation.name == container.name
            && implementation.container == container.container
            && implementation.scope == container.scope
            && implementation.span.contains(&member.span.start)
    })
}

fn container_members(
    workspace: &Workspace,
    facts: &FileFacts,
    container: &Declaration,
    name: &str,
) -> Vec<Key> {
    let qualified = &workspace.files[container.key.0].symbols[container.key.1]["qualified_name"];
    facts
        .declarations
        .iter()
        .filter(|member| {
            member.name == name
                && *qualified == member.container
                && container_contains(facts, container, member)
        })
        .map(|member| member.key)
        .collect()
}

fn rust_lexical_targets(
    workspace: &Workspace,
    index: &LinkIndex,
    facts: &FileFacts,
    site: &Site,
    path: &str,
) -> Option<Vec<Key>> {
    let (first, rest) = path.split_once("::")?;
    if matches!(first, "" | "crate" | "self" | "super") {
        return None;
    }
    let declarations: Vec<_> = facts
        .declarations
        .iter()
        .filter(|d| {
            d.name == first
                && visible(facts, &d.scope, site.span.start)
                && (NameLookup::Type.accepts(&d.kind) || d.kind == "module")
        })
        .collect();
    let closest = declarations.iter().min_by_key(|d| d.scope.len())?;
    let (root, prefix) = &index.rust_modules[site.file];
    let mut targets = Vec::new();
    for declaration in declarations.iter().filter(|d| d.scope == closest.scope) {
        let qualified = workspace.files[site.file].symbols[declaration.key.1]["qualified_name"]
            .as_str()
            .unwrap();
        let name = rust_path(
            &prefix
                .iter()
                .map(String::as_str)
                .chain([qualified, rest])
                .collect::<Vec<_>>()
                .join("::"),
        );
        targets.extend(
            index
                .rust
                .get(&(root.clone(), name))
                .into_iter()
                .flatten()
                .filter(|&&key| {
                    if key.0 != site.file {
                        // Inline modules may themselves contain external
                        // submodules, whose file paths supply the same prefix.
                        return declaration.kind == "module";
                    }
                    facts
                        .declarations
                        .iter()
                        .find(|d| d.key == key)
                        .is_some_and(|member| container_contains(facts, declaration, member))
                })
                .copied(),
        );
    }
    // Even an unresolved local type shadows an outer type with the same name.
    Some(targets)
}

fn receiver_container<'a>(
    workspace: &Workspace,
    facts: &'a FileFacts,
    site: &Site,
) -> Option<&'a Declaration> {
    let qualifier = site.qualifier.as_deref()?;
    let owner = facts
        .declarations
        .iter()
        .find(|d| Some(d.key) == site.owner)?;
    let container = facts
        .declarations
        .iter()
        .filter(|d| {
            matches!(
                d.kind.as_str(),
                "class" | "struct" | "record" | "impl" | "trait" | "interface"
            ) && d.span.contains(&owner.span.start)
                && (workspace.files[site.file].symbols[d.key.1]["qualified_name"]
                    == owner.container
                    || owner.kind == "accessor")
        })
        .min_by_key(|d| d.span.len())?;
    let file = &workspace.files[site.file];
    let language = file.syntax.language;
    if site.verbatim_receiver || (language == "rust" && qualifier == "self" && site.scoped_path) {
        return None;
    }
    if (qualifier == "this"
        && matches!(
            language,
            "javascript" | "typescript" | "typescriptreact" | "java" | "csharp"
        ))
        || (qualifier == "Self" && language == "rust")
    {
        return Some(container);
    }
    let node = declaration_node(file.syntax.tree.root_node(), &file.symbols[owner.key.1])?;
    let parameter = node.child_by_field_name("parameters")?.named_child(0)?;
    let receiver = match language {
        "python" => {
            if node.parent().is_some_and(|parent| {
                parent.kind() == "decorated_definition"
                    && named(parent).iter().any(|decorator| {
                        decorator.kind() == "decorator"
                            && matches!(
                                text_of(*decorator, &file.syntax.source).trim(),
                                "@staticmethod" | "@builtins.staticmethod"
                            )
                    })
            }) {
                return None;
            }
            match parameter.kind() {
                "identifier" => parameter,
                "default_parameter" | "typed_default_parameter" => {
                    parameter.child_by_field_name("name")?
                }
                "typed_parameter" => named(parameter)
                    .into_iter()
                    .find(|n| n.kind() == "identifier")?,
                _ => return None,
            }
        }
        "rust" if qualifier == "self" => identifiers(parameter)
            .into_iter()
            .find(|n| n.kind() == "self")?,
        _ => return None,
    };
    if identifier(language, text_of(receiver, &file.syntax.source)) != qualifier {
        return None;
    }
    let binding = facts
        .bindings
        .iter()
        .find(|b| b.span == receiver.byte_range())?;
    let at = site.span.start;
    if !visible(facts, &binding.scope, at)
        || facts.bindings.iter().any(|other| {
            other.name == qualifier
                && other.span != binding.span
                && other.scope.len() <= binding.scope.len()
                && visible(facts, &other.scope, at)
        })
        || facts.declarations.iter().any(|d| {
            d.name == qualifier
                && d.scope.len() <= binding.scope.len()
                && visible(facts, &d.scope, at)
        })
        || facts.imports.iter().any(|i| {
            i.alias == qualifier
                && i.scope.len() <= binding.scope.len()
                && visible(facts, &i.scope, at)
        })
    {
        return None;
    }
    Some(container)
}

fn type_targets(
    index: &LinkIndex,
    language: &str,
    qualified: &str,
    member: Option<&str>,
) -> Vec<Key> {
    let wanted = member.map_or_else(|| qualified.to_owned(), |m| format!("{qualified}.{m}"));
    index
        .types
        .get(&(language.into(), wanted))
        .cloned()
        .unwrap_or_default()
}

fn csharp_namespace(facts: &FileFacts, at: usize) -> String {
    // The outermost containing type has the namespace as its container. An
    // inner type's container also includes enclosing type names.
    facts
        .declarations
        .iter()
        .filter(|d| {
            d.span.contains(&at)
                && matches!(d.kind.as_str(), "class" | "struct" | "record" | "interface")
        })
        .max_by_key(|d| d.span.len())
        .map(|d| csharp_path(&d.container.replace("::", ".")))
        .unwrap_or_default()
}

fn import_targets(
    workspace: &Workspace,
    index: &LinkIndex,
    facts: &[FileFacts],
    site: &Site,
    import: &Import,
    member: Option<&str>,
) -> Vec<Key> {
    let language = workspace.files[site.file].syntax.language;
    let type_position = matches!(site.name_lookup, Some(NameLookup::Type));
    if import.type_only && !type_position {
        return Vec::new();
    }
    if language == "rust" {
        let path = member.map_or_else(
            || import.module.clone(),
            |m| format!("{}::{m}", import.module),
        );
        let mut origin = site.clone();
        origin.span = import.at..import.at;
        return rust_lexical_targets(workspace, index, &facts[site.file], &origin, &path)
            .unwrap_or_else(|| rust_targets(index, facts, &origin, &path));
    }
    if matches!(language, "java" | "csharp") {
        if member.is_some() && import.member.is_some() {
            return Vec::new();
        }
        return type_targets(
            index,
            language,
            &import.module,
            member.or(import.member.as_deref()),
        );
    }
    if let (Some(member), Some(imported)) = (member, import.member.as_deref()) {
        let mut targets = Vec::new();
        for file in module_files(workspace, site, &import.module) {
            for parent in facts[file].declarations.iter().filter(|d| {
                d.container.is_empty()
                    && matches!(d.kind.as_str(), "class" | "struct" | "record" | "enum")
                    && exported(&workspace.files[file], d, imported, type_position)
            }) {
                let container = workspace.files[file].symbols[parent.key.1]["qualified_name"]
                    .as_str()
                    .unwrap();
                targets.extend(
                    facts[file]
                        .declarations
                        .iter()
                        .filter(|d| d.container == container && d.name == member)
                        .map(|d| d.key),
                );
            }
        }
        // Python `from pkg import mod` can import a submodule as well as a
        // declared class. Keep both candidates when the source is ambiguous.
        if language == "python" {
            let module = if import.module.ends_with('.') {
                format!("{}{imported}", import.module)
            } else {
                format!("{}.{imported}", import.module)
            };
            for file in module_files(workspace, site, &module) {
                targets.extend(
                    facts[file]
                        .declarations
                        .iter()
                        .filter(|d| d.container.is_empty() && d.name == member)
                        .map(|d| d.key),
                );
            }
        }
        return targets;
    }
    let wanted = member.or(import.member.as_deref());
    let Some(wanted) = wanted else {
        return Vec::new();
    };
    module_files(workspace, site, &import.module)
        .into_iter()
        .flat_map(|file| {
            facts[file]
                .declarations
                .iter()
                .filter(move |d| {
                    d.container.is_empty()
                        && exported(&workspace.files[file], d, wanted, type_position)
                })
                .map(|d| d.key)
        })
        .collect()
}

fn resolve(workspace: &Workspace, index: &LinkIndex, all: &[FileFacts], site: &Site) -> Resolution {
    let exact = resolve_exact(workspace, index, all, site);
    if workspace.files[site.file].syntax.language != "csharp"
        || !site.attribute_type_name
        || site.verbatim_name
    {
        return exact;
    }
    // C# permits omitting the Attribute suffix. Keep both interpretations
    // when both exist; their actual inheritance is outside syntax analysis.
    let mut suffixed = site.clone();
    suffixed.name.push_str("Attribute");
    let alternate = resolve_exact(workspace, index, all, &suffixed);
    if alternate.targets.is_empty() {
        return exact;
    }
    let mut targets = exact.targets;
    targets.extend(alternate.targets);
    Resolution::candidates(targets, "attribute_name_or_suffix_candidate")
}

fn resolve_exact(
    workspace: &Workspace,
    index: &LinkIndex,
    all: &[FileFacts],
    site: &Site,
) -> Resolution {
    if site.dynamic_callee {
        return Resolution::unresolved("dynamic_callee_expression");
    }
    let facts = &all[site.file];
    let language = workspace.files[site.file].syntax.language;
    if workspace.files[site.file]
        .syntax
        .tree
        .root_node()
        .has_error()
    {
        return Resolution::unresolved("parse_errors");
    }
    let at = site.span.start;
    // JLS 6.4.1/6.5 separates simple method/type names from value names.
    // A receiver such as `Store.save()` still requires value shadow checks.
    let lookup = site
        .name_lookup
        .filter(|lookup| matches!(lookup, NameLookup::Type) || site.qualifier.is_none());
    let binding_name = site
        .qualifier
        .as_deref()
        .and_then(|q| q.split(['.', ':']).next())
        .unwrap_or(&site.name);
    if (matches!(lookup, Some(NameLookup::Type))
        || (site.qualifier.is_some() && matches!(language, "rust" | "java" | "csharp")))
        && facts
            .type_parameters
            .iter()
            .any(|binding| binding.name == binding_name && visible(facts, &binding.scope, at))
    {
        return Resolution::unresolved("type_parameter_without_symbol");
    }
    if facts.uncertain_bindings.iter().any(|binding| {
        if binding.name != binding_name || !visible(facts, &binding.scope, at) {
            return false;
        }
        let shadow_scope = if binding.own_scope.contains(&at) {
            &binding.own_scope
        } else {
            &binding.scope
        };
        !facts.declarations.iter().any(|other| {
            other.name == binding_name
                && other.scope.len() < shadow_scope.len()
                && visible(facts, &other.scope, at)
        }) && !facts.bindings.iter().any(|other| {
            other.name == binding_name
                && other.scope.len() < shadow_scope.len()
                && visible(facts, &other.scope, at)
        }) && !facts.imports.iter().any(|other| {
            other.alias == binding_name
                && other.scope.len() < shadow_scope.len()
                && visible(facts, &other.scope, at)
        })
    }) {
        return Resolution::unresolved("global_or_nonlocal_binding_not_resolved");
    }
    // A validated method receiver is a closer binding than a module import.
    if let Some(container) = receiver_container(workspace, facts, site) {
        return Resolution::candidates(
            container_members(workspace, facts, container, &site.name),
            "receiver_dispatch_not_resolved",
        );
    }
    let mut imports: Vec<_> = facts
        .imports
        .iter()
        .filter(|i| {
            i.alias == binding_name
                && visible(facts, &i.scope, at)
                && (!matches!(lookup, Some(NameLookup::Method)) || i.member.is_some())
        })
        .collect();
    imports.sort_by_key(|i| i.scope.len());
    if let Some(first) = imports.first() {
        if facts.declarations.iter().any(|d| {
            d.name == binding_name
                && d.scope == first.scope
                && lookup.is_none_or(|lookup| lookup.accepts(&d.kind))
        }) {
            return Resolution::unresolved("conflicting_import_and_declaration");
        }
        // A closer lexical declaration shadows an outer import even when it
        // is a function/class (and therefore absent from the variable bindings).
        imports.retain(|import| {
            !facts.declarations.iter().any(|d| {
                d.name == binding_name
                    && lookup.is_none_or(|lookup| lookup.accepts(&d.kind))
                    && visible(facts, &d.scope, at)
                    && d.scope.len() < import.scope.len()
            })
        });
    }
    if let Some(first) = imports.first() {
        if first.type_only && !matches!(lookup, Some(NameLookup::Type)) {
            return Resolution::unresolved("type_only_import_in_value_position");
        }
        if lookup.is_none() && shadowed(facts, binding_name, at, None) {
            return Resolution::unresolved("import_binding_shadowed_or_assigned");
        }
        let mut targets = Vec::new();
        for import in imports
            .iter()
            .take_while(|i| i.scope.len() == first.scope.len())
        {
            let member = site.qualifier.as_ref().map(|_| site.name.as_str());
            // A dotted receiver longer than the imported alias needs member/type
            // resolution. Do not erase its intermediate components.
            let receiver = import.module_receiver.as_deref().unwrap_or(binding_name);
            if site.qualifier.as_deref().is_some_and(|q| q != receiver) {
                continue;
            }
            targets.extend(import_targets(workspace, index, all, site, import, member));
        }
        return Resolution::candidates(
            targets,
            "explicit_import_without_type_or_loader_resolution",
        );
    }
    if let Some(qualifier) = &site.qualifier {
        if language == "rust" && qualifier != "Self" && site.scoped_path {
            return Resolution::candidates(
                rust_lexical_targets(
                    workspace,
                    index,
                    facts,
                    site,
                    &format!("{qualifier}::{}", site.name),
                )
                .unwrap_or_else(|| {
                    rust_targets(index, all, site, &format!("{qualifier}::{}", site.name))
                }),
                "explicit_module_path_without_compiler_resolution",
            );
        }
        // A named class/module in lexical scope is useful as a candidate. Never
        // infer a receiver type merely from the spelling of a local variable.
        if lookup.is_none() && shadowed(facts, qualifier, at, None) {
            return Resolution::unresolved("receiver_type_unknown");
        }
        let types: Vec<_> = facts
            .declarations
            .iter()
            .filter(|d| {
                d.name == *qualifier
                    && visible(facts, &d.scope, at)
                    && matches!(
                        d.kind.as_str(),
                        "class" | "struct" | "record" | "module" | "enum"
                    )
            })
            .collect();
        let closest_scope = types.iter().min_by_key(|d| d.scope.len()).map(|d| &d.scope);
        let targets = types
            .iter()
            .filter(|d| Some(&d.scope) == closest_scope)
            .flat_map(|parent| container_members(workspace, facts, parent, &site.name))
            .collect::<Vec<_>>();
        if !types.is_empty() {
            return Resolution::candidates(targets, "named_container_without_type_resolution");
        }
        if language == "java" {
            let qualified = if facts.package.is_empty() {
                qualifier.clone()
            } else {
                format!("{}.{qualifier}", facts.package)
            };
            let mut targets = type_targets(index, language, &qualified, Some(&site.name));
            if targets.is_empty() {
                targets = type_targets(index, language, qualifier, Some(&site.name));
            }
            return Resolution::candidates(targets, "same_package_type_candidate");
        }
        if language == "csharp" {
            let namespace = csharp_namespace(facts, at);
            let qualified = if namespace.is_empty() {
                qualifier.clone()
            } else {
                format!("{namespace}.{qualifier}")
            };
            let mut targets = type_targets(index, language, &qualified, Some(&site.name));
            if targets.is_empty() {
                targets = type_targets(index, language, qualifier, Some(&site.name));
            }
            for import in facts
                .imports
                .iter()
                .filter(|i| i.alias == "*namespace" && i.scope.contains(&at))
            {
                targets.extend(type_targets(
                    index,
                    language,
                    &format!("{}.{qualifier}", import.module),
                    Some(&site.name),
                ));
            }
            return Resolution::candidates(targets, "namespace_type_candidate");
        }
        return Resolution::unresolved("receiver_type_or_module_unknown");
    }
    let candidates: Vec<_> = facts
        .declarations
        .iter()
        .filter(|d| {
            d.name == site.name
                && visible(facts, &d.scope, at)
                && lookup.is_none_or(|lookup| lookup.accepts(&d.kind))
                && !(language == "rust" && matches!(d.kind.as_str(), "macro" | "impl"))
        })
        .filter(|d| {
            !matches!(
                d.kind.as_str(),
                "method" | "field" | "property" | "constructor"
            ) || matches!(language, "java" | "csharp")
                || language == "python"
        })
        .collect();
    let Some(closest) = candidates.iter().min_by_key(|d| d.scope.len()) else {
        if language == "java" && matches!(lookup, Some(NameLookup::Type)) {
            let qualified = if facts.package.is_empty() {
                site.name.clone()
            } else {
                format!("{}.{}", facts.package, site.name)
            };
            let targets = type_targets(index, language, &qualified, None);
            if !targets.is_empty() {
                return Resolution::candidates(targets, "same_package_type_candidate");
            }
        }
        if language == "csharp" && matches!(lookup, Some(NameLookup::Type)) {
            let namespace = csharp_namespace(facts, at);
            let qualified = if namespace.is_empty() {
                site.name.clone()
            } else {
                format!("{namespace}.{}", site.name)
            };
            let local = type_targets(index, language, &qualified, None);
            if !local.is_empty() {
                return Resolution::candidates(local, "same_namespace_type_candidate");
            }
            let targets = facts
                .imports
                .iter()
                .filter(|i| i.alias == "*namespace" && visible(facts, &i.scope, at))
                .flat_map(|i| type_targets(index, language, &i.module, Some(&site.name)))
                .collect::<Vec<_>>();
            if !targets.is_empty() {
                return Resolution::candidates(targets, "namespace_type_candidate");
            }
        }
        if language == "csharp" && !shadowed(facts, &site.name, at, None) {
            let targets = facts
                .imports
                .iter()
                .filter(|i| i.alias == "*static" && i.scope.contains(&at))
                .flat_map(|i| type_targets(index, language, &i.module, Some(&site.name)))
                .collect::<Vec<_>>();
            if !targets.is_empty() {
                return Resolution::candidates(targets, "static_import_without_type_resolution");
            }
        }
        return Resolution::unresolved(if shadowed(facts, &site.name, at, None) {
            "local_binding_without_known_target"
        } else if facts.wildcard_import {
            "wildcard_import_not_resolved"
        } else {
            "no_lexical_binding"
        });
    };
    if lookup.is_none() && shadowed(facts, &site.name, at, Some(closest)) {
        return Resolution::unresolved("local_binding_shadowed_or_assigned");
    }
    let targets: Vec<_> = candidates
        .iter()
        .filter(|d| d.scope == closest.scope)
        .map(|d| d.key)
        .collect();
    if targets.len() != 1 {
        return Resolution::candidates(targets, "multiple_declarations_or_overloads");
    }
    if matches!(
        closest.kind.as_str(),
        "method" | "constructor" | "property" | "field" | "impl"
    ) {
        return Resolution::candidates(targets, "member_dispatch_not_resolved");
    }
    if site.call && !matches!(closest.kind.as_str(), "function") {
        return Resolution::candidates(targets, "callable_value_or_constructor_not_resolved");
    }
    if facts.wildcard_import {
        return Resolution::candidates(targets, "wildcard_import_may_change_binding");
    }
    Resolution {
        status: "resolved",
        reason: "unique_lexical_declaration",
        targets,
    }
}

pub(super) fn execute(s: &Session, args: &Value, cancel: &CancellationToken) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(s.config.tool_timeout_secs);
    let path = read_path(&s.project, text(args, "path")?)?;
    let id = text(args, "symbol_id")?;
    let workspace = Workspace::load(s, args, Some(&path), cancel, deadline)?;
    let file = workspace
        .files
        .iter()
        .position(|f| f.syntax.path == path)
        .ok_or_else(|| anyhow::anyhow!("unsupported_language: use source_search/file_read"))?;
    // Reuse the outline's ID validation and revision contract, including paths.
    let (selected, _) =
        workspace.files[file]
            .syntax
            .symbols(&json!({}), Some(id), cancel, deadline)?;
    if !id.starts_with(&format!("{}:", workspace.files[file].syntax.digest)) {
        bail!("symbol_revision_conflict: source changed; repeat symbol_search or code_outline");
    }
    let symbol = selected.first().ok_or_else(|| {
        anyhow::anyhow!(
            "unknown_symbol: copy symbol_id from symbol_search or code_outline for this path"
        )
    })?;
    let key = (
        file,
        workspace.files[file]
            .symbols
            .iter()
            .position(|v| v["symbol_id"] == id)
            .unwrap(),
    );
    let mut span = symbol_span(symbol);
    if symbol["kind"] == "file_scoped_namespace_declaration" {
        // Its members are following AST siblings, all within this namespace.
        span.end = workspace.files[file].syntax.source.len();
    }
    let callable = declaration_node(workspace.files[file].syntax.tree.root_node(), symbol)
        .is_some_and(|node| {
            is_function(node) || node.child_by_field_name("value").is_some_and(is_function)
        });
    let relation = args["relation"].as_str().unwrap_or("calls");
    let (facts, sites, unsupported_calls) = collect(&workspace, cancel, deadline)?;
    let index = LinkIndex::new(&workspace, &facts, cancel, deadline)?;
    let mut rows = Vec::new();
    let mut unresolved = 0usize;
    for site in &sites {
        check_budget(cancel, deadline)?;
        if relation == "calls"
            && (site.file != file || !span.contains(&site.span.start) || !site.call)
        {
            continue;
        }
        // A nested function has its own outgoing calls; they are not executions
        // of the selected function. Class/module queries intentionally include descendants.
        if relation == "calls" && callable && site.owner != Some(key) {
            continue;
        }
        if relation == "callers" && !site.call {
            continue;
        }
        let mut resolution = resolve(&workspace, &index, &facts, site);
        if let Some(lookup) = site.name_lookup {
            let original_count = resolution.targets.len();
            resolution.targets.retain(|&(f, i)| {
                lookup.accepts(
                    workspace.files[f].symbols[i]["symbol_kind"]
                        .as_str()
                        .unwrap(),
                )
            });
            if resolution.targets.len() != original_count {
                resolution =
                    Resolution::candidates(resolution.targets, "method_or_type_name_lookup");
            }
        }
        let original_count = resolution.targets.len();
        resolution
            .targets
            .retain(|&(file, _)| !workspace.files[file].syntax.tree.root_node().has_error());
        if resolution.targets.len() != original_count {
            resolution = Resolution::candidates(resolution.targets, "candidate_file_parse_errors");
        }
        if resolution.targets.is_empty() {
            resolution.status = "unresolved";
            unresolved += 1;
        }
        if relation != "calls" && !resolution.targets.contains(&key) {
            continue;
        }
        let file = &workspace.files[site.file].syntax;
        let path = workspace.relative(&file.path);
        let candidates: Vec<_> = resolution
            .targets
            .iter()
            .take(20)
            .map(|&(f, i)| workspace.symbol(f, &workspace.files[f].symbols[i]))
            .collect();
        let expression: String = file.source[site.span.clone()].chars().take(300).collect();
        let target_matches = resolution.targets.contains(&key);
        rows.push(json!({"path":path,"hash":file.digest,"line":site.line,"end_line":site.end_line,"name_line":site.name_line,"name_column":site.column,
            "location":format!("{path}:{}-{}",site.line,site.end_line),"name":site.name.chars().take(500).collect::<String>(),"name_truncated":site.name.chars().count()>500,"qualifier":site.qualifier.as_ref().map(|q|q.chars().take(500).collect::<String>()),"qualifier_truncated":site.qualifier.as_ref().is_some_and(|q|q.chars().count()>500),
            "kind":if site.call {"call"} else {"reference"},"expression":expression,"expression_truncated":file.source[site.span.clone()].chars().count()>300,
            "enclosing_symbol":site.owner.map(|(f,i)|workspace.symbol(f,&workspace.files[f].symbols[i])),
            "enclosing_callable":site.callable.map(|(start,end)|json!({"location":format!("{path}:{start}-{end}"),"anonymous":site.owner.is_none()})),
            "resolution":resolution.status,"reason":resolution.reason,"candidates":candidates,"candidate_count":resolution.targets.len(),
            "candidates_truncated":resolution.targets.len()>20,"selected_symbol_is_candidate":target_matches}));
    }
    let mut metadata = workspace.metadata();
    metadata["relation"] = json!(relation);
    metadata["symbol"] = workspace.symbol(file, symbol);
    metadata["scope"] = json!({"path_glob":path_glob(args)?,"target_file_always_included":true});
    metadata["unresolved_sites"] = json!(unresolved);
    metadata["unsupported_call_expressions"] = json!(unsupported_calls);
    metadata["complete_call_graph"] = json!(false);
    metadata["limitations"] = json!(
        "Syntax navigation only. resolved means a unique local declaration, not runtime execution. Imports/module paths and member dispatch yield candidates. No type inference, package/config resolution, wildcard/re-export traversal, macro expansion, reflection or dependency injection. Inbound results omit unresolved targets; empty results do not prove no callers/references. Read call sites and candidate bodies, including guards, before documenting behavior."
    );
    // Module roots can depend on a lib.rs/main.rs outside path_glob. Its
    // addition or removal changes links even when scanned source is unchanged.
    let fingerprint = hash(
        serde_json::to_vec(&(
            request_fingerprint(&workspace, "symbol_relations", args),
            &index.rust_modules,
        ))?
        .as_slice(),
    );
    page(metadata, args, &fingerprint, "relations", rows)
}
