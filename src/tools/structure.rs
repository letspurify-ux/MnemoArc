use super::*;
use tree_sitter::{Node, Parser};

pub(super) fn language(path: &Path) -> Result<&'static str> {
    match path.extension().and_then(|s| s.to_str()) {
        Some("rs") => Ok("rust"),
        Some("js" | "mjs" | "cjs" | "jsx") => Ok("javascript"),
        Some("ts" | "mts" | "cts") => Ok("typescript"),
        Some("tsx") => Ok("typescriptreact"),
        Some("py" | "pyi") => Ok("python"),
        Some("java") => Ok("java"),
        _ => bail!(
            "unsupported_language: supported extensions are rs, js/jsx/mjs/cjs, ts/tsx/mts/cts, py/pyi, java; use source_search/file_read otherwise"
        ),
    }
}

fn symbol_name<'a>(node: Node<'a>) -> Option<Node<'a>> {
    match node.kind() {
        "function_item"
        | "function_signature_item"
        | "struct_item"
        | "enum_item"
        | "trait_item"
        | "type_item"
        | "mod_item"
        | "const_item"
        | "static_item"
        | "enum_variant"
        | "macro_definition"
        | "function_declaration"
        | "generator_function_declaration"
        | "class_declaration"
        | "class"
        | "method_definition"
        | "method_signature"
        | "abstract_method_signature"
        | "interface_declaration"
        | "type_alias_declaration"
        | "enum_declaration"
        | "function_signature"
        | "abstract_class_declaration"
        | "method_declaration"
        | "constructor_declaration"
        | "compact_constructor_declaration"
        | "record_declaration"
        | "annotation_type_declaration"
        | "annotation_type_element_declaration"
        | "enum_constant"
        | "field_declaration"
        | "public_field_definition"
        | "property_signature"
        | "function_definition"
        | "class_definition" => node.child_by_field_name("name"),
        "impl_item" => node.child_by_field_name("type"),
        "field_definition" => node.child_by_field_name("property"),
        "variable_declarator" => node
            .child_by_field_name("name")
            .filter(|n| n.kind() == "identifier"),
        _ => None,
    }
}

fn value(node: Node<'_>, source: &str) -> String {
    source[node.byte_range()].chars().take(500).collect()
}

fn options(args: &Value) -> Value {
    let case_sensitive = args["case_sensitive"].as_bool().unwrap_or(false);
    let query = args["query"].as_str().unwrap_or("");
    json!({
        "query":if case_sensitive { query.to_owned() } else { query.to_lowercase() },
        "case_sensitive":case_sensitive,
        "match":args["match"].as_str().unwrap_or("contains"),
        "kind":args["kind"], "container":args["container"], "max_depth":args["max_depth"],
        "view":args["view"].as_str().unwrap_or("detailed")
    })
}

fn fingerprint(path: &Path, digest: &str, options: &Value) -> String {
    hash(
        serde_json::to_vec(&(path, digest, options))
            .unwrap()
            .as_slice(),
    )
}

pub(super) fn continuation(args: &Value, path: &str, cursor: &str) -> Value {
    let mut next = args.clone();
    next["tool"] = json!("code_outline");
    next["path"] = json!(path);
    next["cursor"] = json!(cursor);
    next
}

/// Fit whole symbols into the delivered budget and resume at the first omitted
/// symbol. Rebounding the same result keeps the original page's starting offset.
pub(super) fn limit_outline(
    call: &crate::llm::ToolCall,
    result: &Value,
    limit: usize,
    model: &str,
) -> Option<Value> {
    if result["status"] != "ok" {
        return None;
    }
    let args: Value = serde_json::from_str(&call.arguments).ok()?;
    let data = &result["data"];
    let symbols = data["symbols"].as_array()?;
    let path = data["path"].as_str()?;
    let digest = fingerprint(Path::new(path), data["hash"].as_str()?, &options(&args));
    let start = page_cursor(&args, &digest).ok()?;
    let total = data["total_symbols"].as_u64()? as usize;
    let candidate = |count: usize| {
        let mut page = result.clone();
        page["data"]["symbols"] = json!(&symbols[..count]);
        let end = start + count;
        let cursor = (end < total).then(|| format!("{digest}:{end}"));
        page["data"]["next_cursor"] = json!(cursor);
        page["truncated"] = json!(true);
        page["next_cursor"] =
            cursor.map_or(Value::Null, |cursor| continuation(&args, path, &cursor));
        page
    };
    let (mut low, mut high) = (0, symbols.len());
    while low < high {
        let mid = (low + high).div_ceil(2);
        if result_tokens(call, &candidate(mid), model) <= limit {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    // Tiny budgets may not fit even one intact symbol; retain the existing
    // lossless archive fallback rather than emitting a non-advancing cursor.
    (low > 0).then(|| candidate(low))
}

fn function_value(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("value").filter(|value| {
        matches!(
            value.kind(),
            "arrow_function" | "function_expression" | "generator_function"
        )
    })
}

fn category(node: Node<'_>, parent_kind: &str, source: &str) -> &'static str {
    match node.kind() {
        "function_item" | "function_signature_item" | "function_definition" => {
            if matches!(parent_kind, "impl" | "trait" | "class") {
                "method"
            } else {
                "function"
            }
        }
        "function_declaration" | "generator_function_declaration" | "function_signature" => {
            "function"
        }
        "method_definition"
            if parent_kind == "class"
                && node
                    .child_by_field_name("name")
                    .is_some_and(|name| &source[name.byte_range()] == "constructor") =>
        {
            "constructor"
        }
        "method_definition"
        | "method_signature"
        | "abstract_method_signature"
        | "method_declaration" => "method",
        "constructor_declaration" | "compact_constructor_declaration" => "constructor",
        "class_declaration" | "class" | "abstract_class_declaration" | "class_definition" => {
            "class"
        }
        "struct_item" => "struct",
        "interface_declaration" => "interface",
        "trait_item" => "trait",
        "impl_item" => "impl",
        "enum_item" | "enum_declaration" => "enum",
        "enum_variant" | "enum_constant" => "enum_member",
        "record_declaration" => "record",
        "annotation_type_declaration" => "annotation",
        "annotation_type_element_declaration" => "method",
        "field_declaration"
        | "field_definition"
        | "public_field_definition"
        | "property_signature" => "field",
        "variable_declarator" if function_value(node).is_some() => "function",
        "variable_declarator" => match node.parent().map(|p| p.kind()) {
            Some("field_declaration") => "field",
            Some("constant_declaration") => "constant",
            _ => "variable",
        },
        "const_item" | "static_item" => "constant",
        "type_item" | "type_alias_declaration" => "type",
        "mod_item" => "module",
        "macro_definition" => "macro",
        _ => "variable",
    }
}

fn describe(node: Node<'_>, name: Node<'_>, container: &str, source: &str, digest: &str) -> Value {
    let mut outer = node
        .parent()
        .filter(|p| matches!(p.kind(), "decorated_definition" | "export_statement"))
        .unwrap_or(node);
    if node.kind() == "variable_declarator" {
        if let Some(parent) = node.parent().filter(|p| {
            matches!(
                p.kind(),
                "field_declaration"
                    | "constant_declaration"
                    | "lexical_declaration"
                    | "variable_declaration"
            )
        }) {
            outer = parent;
        }
        if let Some(export) = outer.parent().filter(|p| p.kind() == "export_statement") {
            outer = export;
        }
    }
    let end = outer.end_position();
    let end_line = end.row + usize::from(end.column != 0);
    let signature_end = function_value(node)
        .unwrap_or(node)
        .child_by_field_name("body")
        .map(|n| n.start_byte())
        .unwrap_or_else(|| {
            source[outer.byte_range()]
                .find('\n')
                .map_or(outer.end_byte(), |n| outer.start_byte() + n)
        });
    let signature: String = source[outer.start_byte()..signature_end]
        .trim()
        .chars()
        .take(500)
        .collect();
    let name_pos = name.start_position();
    let line_start = name.start_byte() - name_pos.column;
    let name_column = source[line_start..name.start_byte()].chars().count() + 1;
    json!({
        "symbol_id":format!("{digest}:{}:{}", node.start_byte(), node.end_byte()),
        "name":value(name,source),"kind":node.kind(),"container":container,
        "start_line":outer.start_position().row+1,"end_line":end_line,
        "name_line":name_pos.row+1,"name_column":name_column,
        "signature":signature,"has_parse_errors":node.has_error()
    })
}

pub(super) fn execute(
    s: &mut Session,
    tool: &str,
    args: &Value,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    let path = read_path(&s.project, text(args, "path")?)?;
    let source = read_text(&path)?;
    let digest = hash(source.as_bytes());
    let language = language(&path)?;
    let grammar = match language {
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "typescriptreact" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        _ => unreachable!(),
    };
    let mut parser = Parser::new();
    parser.set_language(&grammar)?;
    let started = std::time::Instant::now();
    let deadline = std::time::Duration::from_secs(s.config.tool_timeout_secs);
    let mut stop = |_: &tree_sitter::ParseState| {
        if cancel.is_cancelled() || started.elapsed() >= deadline {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let tree = parser
        .parse_with_options(
            &mut |offset, _| &source.as_bytes()[offset..],
            None,
            Some(tree_sitter::ParseOptions::new().progress_callback(&mut stop)),
        )
        .ok_or_else(|| anyhow::anyhow!("cancelled_or_timeout: structure parsing interrupted"))?;
    let mut stack = vec![(tree.root_node(), String::new(), "", 0u64)];
    let mut symbols = Vec::new();
    let filters = options(args);
    while let Some((node, mut container, mut parent_kind, mut depth)) = stack.pop() {
        if cancel.is_cancelled() || started.elapsed() >= deadline {
            bail!("cancelled_or_timeout: structure traversal interrupted");
        }
        if let Some(name) = symbol_name(node) {
            let mut symbol = describe(node, name, &container, &source, &digest);
            let kind = category(node, parent_kind, &source);
            symbol["symbol_kind"] = json!(kind);
            symbol["depth"] = json!(depth);
            let name = &source[name.byte_range()];
            let next_container = if container.is_empty() {
                name.to_string()
            } else {
                format!("{container}::{name}")
            };
            let comparable = if filters["case_sensitive"] == true {
                name.to_owned()
            } else {
                name.to_lowercase()
            };
            let query = filters["query"].as_str().unwrap();
            let matches_name = if filters["match"] == "exact" {
                comparable == query
            } else {
                comparable.contains(query)
            };
            if tool == "symbol_read"
                || (matches_name
                    && filters["kind"].as_str().is_none_or(|wanted| wanted == kind)
                    && filters["container"]
                        .as_str()
                        .is_none_or(|wanted| wanted == container)
                    && filters["max_depth"].as_u64().is_none_or(|max| depth <= max))
            {
                symbols.push(symbol);
                if symbols.len() > 20_000 {
                    bail!("outline_too_broad: narrow query or use a smaller file");
                }
            }
            container = next_container;
            parent_kind = kind;
            depth += 1;
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push((child, container.clone(), parent_kind, depth));
        }
    }
    if tool == "symbol_read" {
        let id = text(args, "symbol_id")?;
        if !id.starts_with(&format!("{digest}:")) {
            bail!("symbol_revision_conflict: source changed; call code_outline again");
        }
        let symbol = symbols
            .iter()
            .find(|item| item["symbol_id"] == id)
            .ok_or_else(|| anyhow::anyhow!("unknown_symbol: copy symbol_id from code_outline"))?;
        let start = symbol["start_line"].as_u64().unwrap();
        let end = symbol["end_line"].as_u64().unwrap();
        let mut read_args = json!({"path":path,"start_line":start,"max_lines":end-start+1,"force_read":args["force_read"].as_bool().unwrap_or(false)});
        let mut result = read_file(s, &mut read_args, cancel)?;
        if result["hash"] != digest {
            bail!("symbol_revision_conflict: source changed during read; call code_outline again");
        }
        result["symbol"] = symbol.clone();
        result["engine"] = json!("tree-sitter");
        return Ok(result);
    }
    let fingerprint = fingerprint(&path, &digest, &filters);
    let offset = page_cursor(args, &fingerprint)?;
    if offset > symbols.len() {
        bail!("invalid_cursor");
    }
    let end = offset
        .saturating_add(n(args, "limit", 50).clamp(1, 100))
        .min(symbols.len());
    let lines: Vec<_> = source.lines().collect();
    let mut page = symbols[offset..end].to_vec();
    for symbol in &mut page {
        if filters["view"] == "compact" {
            symbol
                .as_object_mut()
                .unwrap()
                .retain(|key, _| !matches!(key.as_str(), "signature" | "kind"));
            continue;
        }
        let line = symbol["name_line"].as_u64().unwrap() as usize;
        let excerpt: String = lines[line - 1].chars().take(500).collect();
        symbol["declaration"] = json!(excerpt);
        symbol["source"] = json!(observe_hashed(
            s,
            &path,
            digest.clone(),
            line,
            line,
            &excerpt
        ));
    }
    Ok(
        json!({"path":path,"hash":digest,"engine":"tree-sitter","language":language,"view":filters["view"],
        "has_parse_errors":tree.root_node().has_error(),"total_symbols":symbols.len(),"symbols":page,
        "next_cursor":(end<symbols.len()).then(||format!("{fingerprint}:{end}"))}),
    )
}
