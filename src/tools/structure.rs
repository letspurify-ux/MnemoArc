use super::*;
use tree_sitter::{Node, Parser, Tree};

// Cache only syntax, never sources, permissions, filtered results or evidence.
// Every call still reads and hashes the current allowed file before lookup.
// Entry count and aggregate input size bound retention; large files bypass it.
#[derive(Default)]
struct SyntaxCache {
    entries: std::collections::VecDeque<(&'static str, String, usize, Tree)>,
}
impl SyntaxCache {
    const MAX_ENTRIES: usize = 8;
    const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;

    fn get(&mut self, language: &str, digest: &str) -> Option<Tree> {
        let index = self
            .entries
            .iter()
            .position(|(lang, hash, _, _)| *lang == language && hash == digest)?;
        let entry = self.entries.remove(index)?;
        let tree = entry.3.clone();
        self.entries.push_back(entry);
        Some(tree)
    }

    fn insert(&mut self, language: &'static str, digest: String, bytes: usize, tree: Tree) {
        if bytes > Self::MAX_SOURCE_BYTES {
            return;
        }
        self.entries
            .retain(|(lang, hash, _, _)| *lang != language || *hash != digest);
        while self.entries.len() >= Self::MAX_ENTRIES
            || self.entries.iter().map(|entry| entry.2).sum::<usize>() + bytes
                > Self::MAX_SOURCE_BYTES
        {
            self.entries.pop_front();
        }
        self.entries.push_back((language, digest, bytes, tree));
    }
}

fn syntax_cache() -> &'static std::sync::Mutex<SyntaxCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<SyntaxCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

pub(super) fn language(path: &Path) -> Result<&'static str> {
    match path.extension().and_then(|s| s.to_str()) {
        Some("rs") => Ok("rust"),
        Some("js" | "mjs" | "cjs" | "jsx") => Ok("javascript"),
        Some("ts" | "mts" | "cts") => Ok("typescript"),
        Some("tsx") => Ok("typescriptreact"),
        Some("py" | "pyi") => Ok("python"),
        Some("java") => Ok("java"),
        Some("cs") => Ok("csharp"),
        _ => bail!(
            "unsupported_language: supported extensions are rs, js/jsx/mjs/cjs, ts/tsx/mts/cts, py/pyi, java, cs; use source_search/file_read otherwise"
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
        | "struct_declaration"
        | "namespace_declaration"
        | "file_scoped_namespace_declaration"
        | "property_declaration"
        | "accessor_declaration"
        | "event_declaration"
        | "delegate_declaration"
        | "destructor_declaration"
        | "enum_member_declaration"
        | "local_function_statement"
        | "annotation_type_declaration"
        | "annotation_type_element_declaration"
        | "enum_constant"
        | "field_declaration"
        | "public_field_definition"
        | "property_signature"
        | "function_definition"
        | "class_definition" => node.child_by_field_name("name"),
        "impl_item" => node.child_by_field_name("type"),
        "operator_declaration" => node.child_by_field_name("operator"),
        "conversion_operator_declaration" => node.child_by_field_name("type"),
        "indexer_declaration" => {
            let mut cursor = node.walk();
            node.children(&mut cursor).find(|n| n.kind() == "this")
        }
        "field_definition" => node.child_by_field_name("property"),
        "variable_declarator" => node
            .child_by_field_name("name")
            .filter(|n| n.kind() == "identifier"),
        _ => None,
    }
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
    let mut args: Value = serde_json::from_str(&call.arguments).ok()?;
    super::normalize_integer_arguments(call.name.as_str(), &mut args);
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
        "struct_item" | "struct_declaration" => "struct",
        "interface_declaration" => "interface",
        "trait_item" => "trait",
        "impl_item" => "impl",
        "enum_item" | "enum_declaration" => "enum",
        "enum_variant" | "enum_constant" | "enum_member_declaration" => "enum_member",
        "record_declaration" => "record",
        "annotation_type_declaration" => "annotation",
        "annotation_type_element_declaration" => "method",
        "field_declaration"
        | "field_definition"
        | "public_field_definition"
        | "property_signature" => "field",
        "variable_declarator" if function_value(node).is_some() => "function",
        "variable_declarator" => match declaration_owner(node).map(|p| p.kind()) {
            Some("event_field_declaration") => "event",
            Some("field_declaration")
                if declaration_owner(node).is_some_and(|owner| {
                    let mut cursor = owner.walk();
                    owner
                        .named_children(&mut cursor)
                        .any(|n| n.kind() == "modifier" && &source[n.byte_range()] == "const")
                }) =>
            {
                "constant"
            }
            Some("field_declaration") => "field",
            Some("constant_declaration") => "constant",
            _ => "variable",
        },
        "const_item" | "static_item" => "constant",
        "type_item" | "type_alias_declaration" => "type",
        "mod_item" => "module",
        "namespace_declaration" | "file_scoped_namespace_declaration" => "module",
        "property_declaration" | "indexer_declaration" => "property",
        "accessor_declaration" => "accessor",
        "event_declaration" => "event",
        "delegate_declaration" => "delegate",
        "destructor_declaration" => "destructor",
        "operator_declaration" | "conversion_operator_declaration" => "operator",
        "local_function_statement" => "function",
        "macro_definition" => "macro",
        _ => "variable",
    }
}

// C# wraps field/event declarators in a variable_declaration; Java and JS
// attach declarators directly to their declaration owner.
fn declaration_owner(node: Node<'_>) -> Option<Node<'_>> {
    let parent = node.parent()?;
    if parent.kind() == "variable_declaration"
        && let Some(owner) = parent
            .parent()
            .filter(|n| matches!(n.kind(), "field_declaration" | "event_field_declaration"))
    {
        return Some(owner);
    }
    Some(parent)
}

fn symbol_label(node: Node<'_>, name: Node<'_>, source: &str) -> String {
    let name = &source[name.byte_range()];
    match node.kind() {
        "destructor_declaration" => format!("~{name}"),
        "operator_declaration" => format!("operator {name}"),
        "conversion_operator_declaration" => {
            let mut cursor = node.walk();
            let keyword = node
                .children(&mut cursor)
                .find(|n| matches!(n.kind(), "implicit" | "explicit"));
            format!(
                "{} operator {}",
                keyword.map_or("conversion", |n| &source[n.byte_range()]),
                name
            )
        }
        _ => name.to_owned(),
    }
}

fn describe(
    node: Node<'_>,
    name: Node<'_>,
    container: &str,
    source: &str,
    digest: &str,
    path_identity: &str,
) -> Value {
    let mut outer = node
        .parent()
        .filter(|p| matches!(p.kind(), "decorated_definition" | "export_statement"))
        .unwrap_or(node);
    if node.kind() == "variable_declarator" {
        if let Some(parent) = declaration_owner(node).filter(|p| {
            matches!(
                p.kind(),
                "field_declaration"
                    | "event_field_declaration"
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
        .or_else(|| node.child_by_field_name("accessors"))
        .or_else(|| {
            node.child_by_field_name("value")
                .filter(|v| v.kind() == "arrow_expression_clause")
        })
        .map(|n| n.start_byte())
        .unwrap_or(outer.end_byte());
    let full_signature = source[outer.start_byte()..signature_end].trim();
    let signature: String = full_signature.chars().take(500).collect();
    let signature_start_line = outer.start_position().row + 1;
    let signature_end_line = signature_start_line + signature.lines().count().saturating_sub(1);
    let name_pos = name.start_position();
    let line_start = name.start_byte() - name_pos.column;
    let name_column = source[line_start..name.start_byte()].chars().count() + 1;
    json!({
        // The content digest alone is insufficient: two files can contain
        // identical source and byte ranges. Bind IDs to the canonical path so
        // a copied ID cannot silently read a symbol from the wrong file.
        "symbol_id":format!("{digest}:{path_identity}:{}:{}", node.start_byte(), node.end_byte()),
        "name":symbol_label(node,name,source).chars().take(500).collect::<String>(),"kind":node.kind(),"container":container,
        "start_line":outer.start_position().row+1,"end_line":end_line,
        "name_line":name_pos.row+1,"name_column":name_column,
        "signature":signature,"signature_truncated":full_signature.chars().count()>500,
        "signature_start_line":signature_start_line,"signature_end_line":signature_end_line,
        "has_parse_errors":node.has_error()
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
    let path_identity = hash(path.to_string_lossy().as_bytes());
    let language = language(&path)?;
    let grammar = match language {
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "typescriptreact" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "csharp" => tree_sitter_c_sharp::LANGUAGE.into(),
        _ => unreachable!(),
    };
    let started = std::time::Instant::now();
    let deadline = std::time::Duration::from_secs(s.config.tool_timeout_secs);
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    let cached = syntax_cache()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(language, &digest);
    let tree = match cached {
        Some(tree) => tree,
        None => {
            let mut parser = Parser::new();
            parser.set_language(&grammar)?;
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
                .ok_or_else(|| {
                    anyhow::anyhow!("cancelled_or_timeout: structure parsing interrupted")
                })?;
            syntax_cache()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(language, digest.clone(), source.len(), tree.clone());
            tree
        }
    };
    let mut stack = vec![(tree.root_node(), String::new(), "", 0u64)];
    let mut symbols = Vec::new();
    let mut containers = std::collections::BTreeSet::new();
    let filters = options(args);
    let requested_symbol_id = (tool == "symbol_read").then(|| args["symbol_id"].as_str().unwrap());
    let requested_span = requested_symbol_id.and_then(|id| {
        let mut parts = id.rsplitn(3, ':');
        let end = parts.next()?.parse::<usize>().ok()?;
        let start = parts.next()?.parse::<usize>().ok()?;
        Some((start, end))
    });
    while let Some((node, mut container, mut parent_kind, mut depth)) = stack.pop() {
        if cancel.is_cancelled() || started.elapsed() >= deadline {
            bail!("cancelled_or_timeout: structure traversal interrupted");
        }
        if let Some((start, end)) = requested_span
            && (node.end_byte() <= start || node.start_byte() >= end)
        {
            continue;
        }
        if let Some(name) = symbol_name(node) {
            let mut symbol = describe(node, name, &container, &source, &digest, &path_identity);
            let kind = category(node, parent_kind, &source);
            symbol["symbol_kind"] = json!(kind);
            symbol["depth"] = json!(depth);
            let name = symbol_label(node, name, &source);
            let next_container = if container.is_empty() {
                name.to_string()
            } else {
                format!("{container}::{name}")
            };
            symbol["qualified_name"] = json!(next_container);
            if containers.len() < 20_000 {
                containers.insert(container.clone());
            }
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
            let selected = requested_symbol_id.is_some_and(|id| symbol["symbol_id"] == id)
                || (requested_symbol_id.is_none()
                    && matches_name
                    && filters["kind"].as_str().is_none_or(|wanted| wanted == kind)
                    && filters["container"]
                        .as_str()
                        .is_none_or(|wanted| wanted == container)
                    && filters["max_depth"].as_u64().is_none_or(|max| depth <= max));
            if selected {
                symbols.push(symbol);
                if requested_symbol_id.is_some() {
                    // symbol_read has an exact ID and needs only that symbol.
                    // Stop traversal immediately so a valid ID in a very large
                    // file is not rejected as an overly broad outline.
                    break;
                }
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
        let mut scheduled = Vec::with_capacity(children.len());
        for child in children {
            scheduled.push((child, container.clone(), parent_kind, depth));
            // A file-scoped C# namespace owns following siblings, not AST
            // children. Carry its scope into subsequent declarations.
            if language == "csharp"
                && child.kind() == "file_scoped_namespace_declaration"
                && let Some(name) = child.child_by_field_name("name")
            {
                let name = symbol_label(child, name, &source);
                container = if container.is_empty() {
                    name
                } else {
                    format!("{container}::{name}")
                };
                parent_kind = "module";
                depth += 1;
            }
        }
        stack.extend(scheduled.into_iter().rev());
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
        let start = args["start_line"].as_u64().unwrap_or(start);
        if start < symbol["start_line"].as_u64().unwrap() || start > end {
            bail!(
                "invalid_symbol_range: start_line must be an absolute line within symbol.start_line..symbol.end_line"
            );
        }
        let lines = match args["max_lines"].as_u64() {
            Some(lines) if !(1..=2000).contains(&lines) => {
                bail!("invalid_symbol_range: max_lines must be between 1 and 2000")
            }
            Some(lines) => lines.min(end - start + 1),
            None => end - start + 1,
        };
        let mut read_args = json!({"path":path,"start_line":start,"max_lines":lines,"force_read":args["force_read"].as_bool().unwrap_or(false)});
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
        bail!(INVALID_CURSOR);
    }
    let end = offset
        .saturating_add(n(args, "limit", 50).clamp(1, 100))
        .min(symbols.len());
    let lines: Vec<_> = source.lines().collect();
    let mut page = symbols[offset..end].to_vec();
    let root = s.project.root.canonicalize()?;
    for symbol in &mut page {
        let relative = path.strip_prefix(&root).unwrap_or(&path).display();
        symbol["location"] = json!(format!(
            "{}:{}-{}",
            relative, symbol["start_line"], symbol["end_line"]
        ));
        if filters["view"] == "compact" {
            symbol
                .as_object_mut()
                .unwrap()
                .retain(|key, _| key != "kind" && !key.starts_with("signature"));
            continue;
        }
        let line = symbol["name_line"].as_u64().unwrap() as usize;
        let excerpt: String = lines[line - 1].chars().take(500).collect();
        symbol["declaration"] = json!(excerpt);
        symbol["source"] = json!(super::observe_hashed_quality(
            s,
            &path,
            digest.clone(),
            line,
            line,
            &excerpt,
            super::EvidenceQuality {
                line_start_complete: true,
                line_end_complete: true,
                evidence_truncated: true,
            },
        ));
        symbol["signature_source"] = json!(super::observe_hashed_quality(
            s,
            &path,
            digest.clone(),
            symbol["signature_start_line"].as_u64().unwrap() as usize,
            symbol["signature_end_line"].as_u64().unwrap() as usize,
            symbol["signature"].as_str().unwrap(),
            super::EvidenceQuality {
                line_start_complete: true,
                line_end_complete: true,
                evidence_truncated: true,
            },
        ));
    }
    let mut result = json!({"path":path,"hash":digest,"engine":"tree-sitter","language":language,"view":filters["view"],
        "has_parse_errors":tree.root_node().has_error(),"total_symbols":symbols.len(),"symbols":page,
        "next_cursor":(end<symbols.len()).then(||format!("{fingerprint}:{end}"))});
    if symbols.is_empty() {
        result["empty_reason"] = json!("no_matching_symbols");
        result["guidance"] = json!(
            "No declarations matched these filters. Copy the parent's qualified_name into container (nested names use ::, not Java dots). max_depth=0 excludes class members. Relax a filter before repeating; a parse error may also hide declarations."
        );
        result["available_containers"] = json!(containers.iter().take(12).collect::<Vec<_>>());
        result["container_suggestions_truncated"] = json!(containers.len() > 12);
    }
    Ok(result)
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn syntax_cache_separates_revisions_languages_and_bounds_retention() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn old() {}", None).unwrap();
        let mut cache = SyntaxCache::default();
        cache.insert("rust", "old".into(), 12, tree.clone());
        assert!(cache.get("rust", "old").is_some());
        assert!(cache.get("rust", "changed").is_none());
        assert!(cache.get("javascript", "old").is_none());
        for i in 0..SyntaxCache::MAX_ENTRIES {
            cache.insert("rust", i.to_string(), 12, tree.clone());
        }
        assert!(cache.get("rust", "old").is_none());
        cache.insert(
            "rust",
            "large".into(),
            SyntaxCache::MAX_SOURCE_BYTES,
            tree.clone(),
        );
        assert_eq!(cache.entries.len(), 1);
        cache.insert(
            "rust",
            "oversized".into(),
            SyntaxCache::MAX_SOURCE_BYTES + 1,
            tree,
        );
        assert!(cache.get("rust", "oversized").is_none());
        assert!(cache.get("rust", "large").is_some());
    }
}
