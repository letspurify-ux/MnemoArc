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
        | "function_definition"
        | "class_definition" => node.child_by_field_name("name"),
        "impl_item" => node.child_by_field_name("type"),
        "variable_declarator" => node
            .child_by_field_name("name")
            .filter(|n| n.kind() == "identifier"),
        _ => None,
    }
}

fn value(node: Node<'_>, source: &str) -> String {
    source[node.byte_range()].chars().take(500).collect()
}

fn describe(node: Node<'_>, name: Node<'_>, container: &str, source: &str, digest: &str) -> Value {
    let outer = node
        .parent()
        .filter(|p| matches!(p.kind(), "decorated_definition" | "export_statement"))
        .unwrap_or(node);
    let end = outer.end_position();
    let end_line = end.row + usize::from(end.column != 0);
    let signature_end = node
        .child_by_field_name("body")
        .map(|n| n.start_byte())
        .unwrap_or_else(|| {
            source[node.byte_range()]
                .find('\n')
                .map_or(node.end_byte(), |n| node.start_byte() + n)
        });
    let signature: String = source[node.start_byte()..signature_end]
        .trim()
        .chars()
        .take(500)
        .collect();
    let name_pos = name.start_position();
    let line_start = name.start_byte() - name_pos.column;
    let name_column = source[line_start..name.start_byte()].chars().count() + 1;
    json!({
        "symbol_id":format!("{digest}:{}:{}", outer.start_byte(), outer.end_byte()),
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
    let mut stack = vec![(tree.root_node(), String::new())];
    let mut symbols = Vec::new();
    let query = args["query"].as_str().unwrap_or("").to_lowercase();
    while let Some((node, mut container)) = stack.pop() {
        if cancel.is_cancelled() || started.elapsed() >= deadline {
            bail!("cancelled_or_timeout: structure traversal interrupted");
        }
        if let Some(name) = symbol_name(node) {
            let symbol = describe(node, name, &container, &source, &digest);
            let name = symbol["name"].as_str().unwrap();
            let next_container = if container.is_empty() {
                name.to_string()
            } else {
                format!("{container}::{name}")
            };
            if name.to_lowercase().contains(&query) {
                symbols.push(symbol);
                if symbols.len() > 20_000 {
                    bail!("outline_too_broad: narrow query or use a smaller file");
                }
            }
            container = next_container;
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push((child, container.clone()));
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
    let fingerprint = hash(serde_json::to_vec(&(&path, &digest, &query))?.as_slice());
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
        json!({"path":path,"hash":digest,"engine":"tree-sitter","language":language,
        "has_parse_errors":tree.root_node().has_error(),"total_symbols":symbols.len(),"symbols":page,
        "next_cursor":(end<symbols.len()).then(||format!("{fingerprint}:{end}"))}),
    )
}
