mod documentation;
use crate::{
    config::Project,
    context::{self},
    memory::{MemoryInput, Source},
    session::{Investigation, Session, TaskState},
};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub optional: bool,
    pub read_only: bool,
    pub parameters: Value,
}
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn schema(fields: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":fields,"required":required,"additionalProperties":false})
}
fn string() -> Value {
    json!({"type":"string"})
}
fn number() -> Value {
    json!({"type":"integer","minimum":0})
}
fn strings() -> Value {
    json!({"type":"array","items":{"type":"string"}})
}
fn action(values: &[&str]) -> Value {
    json!({"type":"string","enum":values})
}
pub struct ToolRegistry;
impl ToolRegistry {
    pub fn specs() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "tool_catalog",
                description: "List/search available tools and source-docs group; shows short descriptions and active status",
                optional: false,
                read_only: true,
                parameters: schema(json!({"query":string()}), &[]),
            },
            ToolSpec {
                name: "tool_select",
                description: "Add, remove or replace optional tools/groups. Takes effect after this call batch; basic tools cannot be removed",
                optional: false,
                read_only: false,
                parameters: schema(
                    json!({"action":action(&["add","remove","replace"]),"names":strings()}),
                    &["action", "names"],
                ),
            },
            ToolSpec {
                name: "memory_write",
                description: "Save one reusable memory. Same key requires expected_revision. Source IDs must come from program observations. Keep metadata very short (aim for 80 tokens; hard limit 160 including ID/key/JSON). Put details in body. kind: fact/decision/failure/question/procedure",
                optional: false,
                read_only: false,
                parameters: schema(
                    json!({"key":string(),"title":string(),"summary":string(),"body":string(),"tags":strings(),"kind":action(&["fact","decision","failure","question","procedure"]),"inferred":{"type":"boolean"},"source_ids":strings(),"metadata":{"type":"object"},"expected_revision":number()}),
                    &["title", "summary", "body", "kind"],
                ),
            },
            ToolSpec {
                name: "memory_read",
                description: "Load memory by ID/key; offset is character offset for bounded continuation. Revalidates file sources",
                optional: false,
                read_only: true,
                parameters: schema(json!({"id":string(),"offset":number()}), &["id"]),
            },
            ToolSpec {
                name: "memory_find",
                description: "Search metadata by key/tag/keywords, or list all with empty query; use next cursor until exhausted",
                optional: false,
                read_only: true,
                parameters: schema(
                    json!({"query":string(),"tags":strings(),"cursor":string(),"limit":number()}),
                    &[],
                ),
            },
            ToolSpec {
                name: "memory_manage",
                description: "List cleanup candidates, delete unreferenced memories, or atomically replace IDs and redirect references. replacement uses memory_write fields",
                optional: false,
                read_only: false,
                parameters: schema(
                    json!({"action":action(&["candidates","delete","replace"]),"ids":strings(),"replacement":{"type":"object"}}),
                    &["action"],
                ),
            },
            ToolSpec {
                name: "task_state",
                description: "Read/update structured goals and compact progress, or read/write detailed work list. Updates preserve omitted fields. Preserve user constraints unless explicitly changed by user",
                optional: false,
                read_only: false,
                parameters: schema(
                    json!({"action":action(&["read","update","details"]),"patch":{"type":"object"},"offset":number(),"limit":number()}),
                    &["action"],
                ),
            },
            ToolSpec {
                name: "history",
                description: "Search retained raw conversation/tool bundles or read one by ID and character offset; unavailable/pruned ranges are explicit",
                optional: false,
                read_only: true,
                parameters: schema(
                    json!({"action":action(&["search","read"]),"query":string(),"id":number(),"offset":number(),"after":number(),"limit":number()}),
                    &["action"],
                ),
            },
            ToolSpec {
                name: "checkpoint_complete",
                description: "Finish a checkpoint and save progress in ONE call. Required progress is a concise current/next-work summary. Save needed memories first, or explain no new memory is needed with no_save_reason. Evaluated after other calls in this batch.",
                optional: false,
                read_only: false,
                parameters: schema(
                    json!({"id":string(),"progress":string(),"next":string(),"no_save_reason":string()}),
                    &["id", "progress"],
                ),
            },
            ToolSpec {
                name: "file_list",
                description: "List project text file paths with glob filter and pagination; respects project boundaries and exclusions",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"pattern":string(),"cursor":string(),"limit":number()}),
                    &[],
                ),
            },
            ToolSpec {
                name: "source_search",
                description: "Search source lines by literal or regex and path glob; paginated results include program-issued source IDs and hashes",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"query":string(),"regex":{"type":"boolean"},"pattern":string(),"cursor":string(),"limit":number()}),
                    &["query"],
                ),
            },
            ToolSpec {
                name: "document_inspect",
                description: "Read output metadata/hash/line count and paginated Markdown outline without loading full text. Supply section to read one unique heading, offset and expected_hash for safe continuation.",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"section":string(),"offset":number(),"limit":number(),"expected_hash":string()}),
                    &[],
                ),
            },
            ToolSpec {
                name: "symbol_search",
                description: "Heuristic declaration search for JS/TS, Rust and Python (not LSP or references). Returns source lines and hashes; narrow pattern/query. Cursor expires on source changes.",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"query":string(),"pattern":string(),"cursor":string(),"limit":number()}),
                    &[],
                ),
            },
            ToolSpec {
                name: "document_audit",
                description: "Check output citations path:line[-line], source freshness, section coverage and pending investigations in one call. Structural checks do NOT prove semantic correctness. Paginated issues.",
                optional: true,
                read_only: true,
                parameters: schema(json!({"offset":number(),"limit":number()}), &[]),
            },
            ToolSpec {
                name: "file_read",
                description: "Read project/output file by 1-based start_line and max_lines. Continuation: copy returned next_line and next_offset together (offset is relative to start_line, NOT the whole file). Returns source ID, hash, total_lines and line_start/line_offsets for exact citations; repeated unchanged active-context reads are suppressed unless force_read=true",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"path":string(),"start_line":number(),"max_lines":number(),"offset":number(),"force_read":{"type":"boolean"}}),
                    &["path"],
                ),
            },
            ToolSpec {
                name: "document_edit",
                description: "Edit ONLY configured Markdown output: create, replace entire file, append, or unique exact text patch. Existing file requires expected_hash. section replaces a unique full heading section and also requires expected_section_hash. Returns measured lines and new hash",
                optional: true,
                read_only: false,
                parameters: schema(
                    json!({"action":action(&["create","write","append","patch","section"]),"text":string(),"old_text":string(),"expected_hash":string(),"section":string(),"expected_section_hash":string()}),
                    &["action", "text"],
                ),
            },
            ToolSpec {
                name: "investigation",
                description: "Manage source documentation items. Actions list/upsert/verify/verify_batch/final_check. verify_batch items is an object keyed by item ID, each value {source_ids:[...],verification_note:string}; each is independently verified and failures reported. status uninvestigated/in_progress/written; verify compares document with source IDs and requires verification_note. section is a unique Markdown heading",
                optional: true,
                read_only: false,
                parameters: schema(
                    json!({"action":action(&["list","upsert","verify","verify_batch","final_check"]),"id":string(),"title":string(),"status":action(&["uninvestigated","in_progress","written"]),"memory_ids":strings(),"source_ids":strings(),"section":string(),"verification_note":string(),"items":{"type":"object","minProperties":1,"maxProperties":20,"additionalProperties":{"type":"object","properties":{"source_ids":strings(),"verification_note":string()},"required":["source_ids","verification_note"],"additionalProperties":false}},"offset":number(),"limit":number()}),
                    &["action"],
                ),
            },
        ]
    }
    fn checkpoint_allowed(name: &str) -> bool {
        [
            "memory_write",
            "memory_read",
            "memory_find",
            "memory_manage",
            "task_state",
            "history",
            "checkpoint_complete",
        ]
        .contains(&name)
    }
    pub fn definitions(s: &Session) -> Vec<Value> {
        Self::specs().into_iter()
            .filter(|t| !t.optional || s.active_tools.contains(t.name))
            .filter(|t| s.checkpoint.is_none() || Self::checkpoint_allowed(t.name))
            .map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.parameters}}))
            .collect()
    }
    pub fn optional_names() -> BTreeSet<String> {
        Self::specs()
            .into_iter()
            .filter(|t| t.optional)
            .map(|t| t.name.into())
            .collect()
    }
    pub fn validate(s: &Session, name: &str, args: &Value) -> Result<ToolSpec> {
        let spec = Self::specs()
            .into_iter()
            .find(|t| t.name == name)
            .ok_or_else(|| anyhow::anyhow!("unsupported_tool: {name}"))?;
        if spec.optional && !s.active_tools.contains(name) {
            bail!("tool_not_active: {name}");
        }
        if !s.config.memory_reuse && ["memory_find", "memory_read"].contains(&name) {
            bail!("unsupported: memory reuse disabled for evaluation");
        }
        let object = args
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Arguments must be an object"))?;
        let fields = spec.parameters["properties"].as_object().unwrap();
        for key in object.keys() {
            if !fields.contains_key(key) {
                bail!("unknown_argument: {key}");
            }
        }
        for required in spec.parameters["required"].as_array().unwrap() {
            if !object.contains_key(required.as_str().unwrap()) {
                bail!("missing_argument: {required}");
            }
        }
        for (k, v) in object {
            let field = &fields[k];
            let valid = match field["type"].as_str() {
                Some("string") => v.is_string(),
                Some("integer") => v.as_u64().is_some(),
                Some("boolean") => v.is_boolean(),
                Some("array") => v.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
                Some("object") => v.is_object(),
                _ => true,
            };
            if !valid {
                bail!("invalid_argument_type: {k}");
            }
            if let Some(values) = field["enum"].as_array()
                && !values.contains(v)
            {
                bail!("invalid_argument_value: {k}");
            }
        }
        Ok(spec)
    }
}
fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing_argument: {key}"))
}
fn list(args: &Value, key: &str) -> Vec<String> {
    args[key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}
fn n(args: &Value, key: &str, default: usize) -> usize {
    args[key].as_u64().map_or(default, |n| n as usize)
}
pub fn envelope(result: Result<Value>) -> Value {
    match result {
        Ok(data) => json!({"status":"ok","data":data,"truncated":false,"next_cursor":null}),
        Err(error) => {
            let message = error.to_string();
            let status = if message.contains("cancelled") {
                "cancelled"
            } else if message.starts_with("unsupported") {
                "unsupported"
            } else {
                "error"
            };
            json!({"status":status,"error":message,"truncated":false,"next_cursor":null})
        }
    }
}

fn glob_match(patterns: &[String], path: &str) -> Result<bool> {
    let mut b = globset::GlobSetBuilder::new();
    for p in patterns {
        b.add(globset::Glob::new(p)?);
    }
    Ok(b.build()?.is_match(path))
}
fn excluded(p: &Project, rel: &Path) -> Result<bool> {
    let defaults = [
        ".git",
        ".hg",
        ".svn",
        "target",
        "node_modules",
        "vendor",
        "dist",
        "build",
        ".venv",
        "__pycache__",
    ];
    if rel
        .components()
        .any(|c| defaults.contains(&c.as_os_str().to_string_lossy().as_ref()))
    {
        return Ok(true);
    }
    let s = rel.to_string_lossy();
    Ok((!p.include.is_empty() && !glob_match(&p.include, &s)?) || glob_match(&p.exclude, &s)?)
}
pub fn output_path(p: &Project) -> Result<PathBuf> {
    let root = p.root.canonicalize()?;
    let path = if p.output.is_absolute() {
        p.output.clone()
    } else {
        root.join(&p.output)
    };
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid_output"))?;
    let mut ancestor = parent;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| anyhow::anyhow!("invalid_output_parent"))?;
    }
    if !p.output.is_absolute() && !ancestor.canonicalize()?.starts_with(&root) {
        bail!("output_path_escape");
    }
    if path.exists() && !p.output.is_absolute() && !path.canonicalize()?.starts_with(&root) {
        bail!("output_symlink_escape");
    }
    if path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        bail!("parent_traversal_not_allowed");
    }
    Ok(path)
}
pub fn read_path(p: &Project, path: &str) -> Result<PathBuf> {
    let root = p.root.canonicalize()?;
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        root.join(path)
    };
    let canonical = candidate.canonicalize()?;
    let output = output_path(p)?;
    if output.exists() && canonical == output.canonicalize()? {
        return Ok(canonical);
    }
    if !canonical.starts_with(&root) {
        bail!("path_outside_project");
    }
    if excluded(p, canonical.strip_prefix(&root)?)? {
        bail!("path_excluded");
    }
    Ok(canonical)
}
fn read_text(path: &Path) -> Result<String> {
    let metadata = path.metadata()?;
    if metadata.len() > 16 * 1024 * 1024 {
        bail!("unsupported_large_file: maximum 16MiB");
    }
    let bytes = std::fs::read(path)?;
    if bytes.contains(&0) {
        bail!("unsupported_binary_file");
    }
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("unsupported_non_utf8_file"))
}
fn paths(
    p: &Project,
    pattern: Option<&str>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<PathBuf>> {
    let root = p.root.canonicalize()?;
    let mut entries = vec![];
    let filter = pattern
        .map(globset::Glob::new)
        .transpose()?
        .map(|g| g.compile_matcher());
    for entry in ignore::WalkBuilder::new(&root)
        .hidden(false)
        .follow_links(false)
        .filter_entry(|entry| {
            !entry.file_type().is_some_and(|t| t.is_dir())
                || ![
                    ".git",
                    ".hg",
                    ".svn",
                    "target",
                    "node_modules",
                    "vendor",
                    "dist",
                    "build",
                    ".venv",
                    "__pycache__",
                ]
                .contains(&entry.file_name().to_string_lossy().as_ref())
        })
        .build()
    {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        let entry = entry?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(&root)?;
        if excluded(p, rel)? || filter.as_ref().is_some_and(|f| !f.is_match(rel)) {
            continue;
        }
        match read_text(entry.path()) {
            Ok(_) => entries.push(entry.path().to_path_buf()),
            Err(e) if e.to_string().starts_with("unsupported_") => {}
            Err(e) => return Err(e),
        }
    }
    entries.sort();
    Ok(entries)
}
fn observe(
    s: &mut Session,
    path: &Path,
    contents: &str,
    start: usize,
    end: usize,
    excerpt: &str,
) -> Source {
    observe_hashed(s, path, hash(contents.as_bytes()), start, end, excerpt)
}
fn observe_hashed(
    s: &mut Session,
    path: &Path,
    content_hash: String,
    start: usize,
    end: usize,
    excerpt: &str,
) -> Source {
    if let Some(source) = s.sources.values().find(|source| {
        source.path.as_deref() == path.to_str()
            && source.hash.as_deref() == Some(&content_hash)
            && source.start_line == Some(start)
            && source.end_line == Some(end)
            && source.excerpt == excerpt.chars().take(2000).collect::<String>()
    }) {
        return source.clone();
    }
    let source = Source {
        id: crate::memory::id(),
        observed_at: chrono::Utc::now(),
        origin: "file".into(),
        path: Some(path.display().to_string()),
        start_line: Some(start),
        end_line: Some(end),
        hash: Some(content_hash),
        excerpt: excerpt.chars().take(2000).collect(),
    };
    s.memory
        .stale_path(source.path.as_deref().unwrap(), source.hash.as_deref());
    s.sources.insert(source.id.clone(), source.clone());
    source
}
pub fn revalidate(s: &mut Session) -> Result<()> {
    let mut sources: Vec<_> = s
        .memory
        .entries
        .values()
        .flat_map(|m| m.sources.clone())
        .chain(s.investigations.iter().flat_map(|i| i.sources.clone()))
        .collect();
    sources.sort_by(|a, b| a.path.cmp(&b.path));
    sources.dedup_by(|a, b| a.path == b.path);
    let mut hashes = std::collections::BTreeMap::new();
    for source in sources {
        if let Some(path) = source.path {
            let current = read_path(&s.project, &path)
                .and_then(|p| std::fs::read(p).map_err(Into::into))
                .ok()
                .map(|b| hash(&b));
            s.memory.stale_path(&path, current.as_deref());
            hashes.insert(path, current);
        }
    }
    let doc = output_path(&s.project)
        .ok()
        .and_then(|p| read_text(&p).ok());
    for item in &mut s.investigations {
        let changed = item.sources.iter().any(|r| {
            r.path.as_ref().is_some_and(|p| {
                hashes
                    .get(p)
                    .is_none_or(|h| h.as_deref() != r.hash.as_deref())
            })
        }) || item
            .memory_refs
            .iter()
            .any(|(id, rev)| s.memory.get(id).map_or(true, |m| m.revision != *rev));
        let doc_changed = doc
            .as_ref()
            .and_then(|d| section_text(d, &item.section).ok())
            .map(|t| hash(t.as_bytes()))
            != item.document_hash;
        if item.status == "verified" && (changed || doc_changed) {
            item.status = "written".into();
            item.note = "Source, memory or document changed; verification required".into();
        }
    }
    Ok(())
}
fn section_text<'a>(doc: &'a str, heading: &str) -> Result<&'a str> {
    let headings = documentation::headings(doc);
    let matching: Vec<_> = headings
        .iter()
        .filter(|h| h.heading == heading.trim())
        .collect();
    if matching.len() != 1 {
        bail!("section must be a unique Markdown heading");
    }
    let h = matching[0];
    Ok(&doc[h.start..h.end])
}
fn bounded_text(s: &Session, content: &str, offset: usize) -> Value {
    let remaining: String = content.chars().skip(offset).collect();
    let (body, truncated) = context::truncate(
        &remaining,
        s.config
            .result_tokens
            .saturating_sub(500)
            .max(s.config.result_tokens / 2)
            .max(1),
        &s.config.model,
    );
    let next = offset + body.chars().count();
    json!({"text":body,"truncated":truncated,"next_offset":truncated.then_some(next)})
}
fn page_cursor(args: &Value, fingerprint: &str) -> Result<usize> {
    if let Some(cursor) = args["cursor"].as_str() {
        let (h, index) = cursor
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("invalid_cursor"))?;
        if h != fingerprint {
            bail!("cursor_expired: source listing changed");
        }
        Ok(index.parse()?)
    } else {
        Ok(0)
    }
}

pub fn execute(s: &mut Session, name: &str, args: Value) -> Result<Value> {
    execute_cancellable(s, name, args, &tokio_util::sync::CancellationToken::new())
}
pub fn execute_cancellable(
    s: &mut Session,
    name: &str,
    mut args: Value,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    // Some compatible tool parsers quote integers. Convert only exact ASCII
    // unsigned decimals in fields whose schema explicitly requires an integer.
    if let Some(spec) = ToolRegistry::specs().into_iter().find(|t| t.name == name)
        && let Some(fields) = args.as_object_mut()
    {
        for (key, value) in fields {
            if spec.parameters["properties"][key]["type"] == "integer"
                && let Some(raw) = value.as_str()
                && !raw.is_empty()
                && raw.bytes().all(|b| b.is_ascii_digit())
                && let Ok(number) = raw.parse::<u64>()
            {
                *value = json!(number);
            }
        }
    }
    ToolRegistry::validate(s, name, &args)?;
    if s.checkpoint.is_some() && !ToolRegistry::checkpoint_allowed(name) {
        bail!("checkpoint_pending: only memory/state/history maintenance allowed");
    }
    if s.run_guidance["phase"] == "verify"
        && !s.investigations.is_empty()
        && (name == "file_list"
            || (name == "symbol_search" && args["query"].as_str().unwrap_or("").is_empty())
            || (name == "investigation" && args["action"] == "upsert" && args["id"].is_null()))
    {
        bail!(
            "verification_reserve: focus on existing investigation items; broad discovery and new items are paused"
        );
    }
    match name {
        "document_inspect" | "document_audit" | "symbol_search" => {
            documentation::execute(s, name, &args, cancel)
        }
        "tool_catalog" => {
            let q = args["query"].as_str().unwrap_or("").to_lowercase();
            Ok(
                json!({"groups":["source-docs"],"tools":ToolRegistry::specs().into_iter().filter(|t|t.name.contains(&q)||t.description.to_lowercase().contains(&q)).map(|t|json!({"name":t.name,"description":t.description,"basic":!t.optional,"active":!t.optional||s.active_tools.contains(t.name)})).collect::<Vec<_>>()}),
            )
        }
        "tool_select" => {
            let available = ToolRegistry::optional_names();
            let mut chosen = BTreeSet::new();
            for name in list(&args, "names") {
                if name == "source-docs" {
                    chosen.extend(available.clone());
                } else if available.contains(&name) {
                    chosen.insert(name);
                } else {
                    bail!("unknown_optional_tool_or_basic_tool: {name}");
                }
            }
            let mut pending = s.pending_tools.clone().unwrap_or(s.active_tools.clone());
            match text(&args, "action")? {
                "add" => pending.extend(chosen),
                "remove" => pending.retain(|n| !chosen.contains(n)),
                "replace" => pending = chosen,
                _ => unreachable!(),
            };
            s.pending_tools = Some(pending.clone());
            Ok(json!({"pending":pending,"applies":"next_request"}))
        }
        "memory_write" => {
            let input: MemoryInput = serde_json::from_value(args)?;
            let sources = s.source_refs(&input.source_ids)?;
            let result = s.memory.save(input, sources, &s.config)?;
            Ok(json!(result))
        }
        "memory_read" => {
            if !s.config.memory_reuse {
                bail!("unsupported: memory reuse disabled for evaluation");
            }
            s.memory_loads += 1;
            revalidate(s)?;
            let m = s.memory.get(text(&args, "id")?)?;
            Ok(
                json!({"metadata":m.meta(),"body":bounded_text(s,&m.body,n(&args,"offset",0)),"sources":m.sources,"inferred":m.inferred,"kind":m.kind}),
            )
        }
        "memory_find" => s.memory.page(
            args["query"].as_str().unwrap_or(""),
            &list(&args, "tags"),
            args["cursor"].as_str(),
            n(&args, "limit", 20),
        ),
        "memory_manage" => {
            let ids = list(&args, "ids");
            match text(&args, "action")? {
                "candidates" => Ok(
                    json!({"bytes":s.memory.bytes(),"candidates":s.memory.candidates(&s.protected())}),
                ),
                "delete" => {
                    let mut copy = s.memory.clone();
                    for id in ids {
                        copy.delete(&id, &s.protected())?;
                    }
                    s.memory = copy;
                    Ok(json!({"deleted":true}))
                }
                "replace" => {
                    if ids.is_empty() {
                        bail!("replace requires ids");
                    }
                    let actual = ids
                        .iter()
                        .map(|id| s.memory.get(id).map(|m| m.id.clone()))
                        .collect::<Result<Vec<_>>>()?;
                    let input: MemoryInput = serde_json::from_value(args["replacement"].clone())?;
                    let sources = s.source_refs(&input.source_ids)?;
                    let m = s.memory.replace(&actual, input, sources, &s.config)?;
                    for id in &mut s.task.memory_ids {
                        if actual.contains(id) {
                            *id = m.id.clone();
                        }
                    }
                    s.task.memory_ids.sort();
                    s.task.memory_ids.dedup();
                    s.task.revision += 1;
                    for item in &mut s.investigations {
                        let mut replaced = false;
                        for id in &actual {
                            replaced |= item.memory_refs.remove(id).is_some();
                        }
                        if replaced {
                            item.memory_refs.insert(m.id.clone(), m.revision);
                            item.status = "written".into();
                        }
                    }
                    Ok(json!(m))
                }
                _ => unreachable!(),
            }
        }
        "task_state" => match text(&args, "action")? {
            "read" => {
                let mut task = json!(s.task);
                task.as_object_mut().unwrap().remove("details");
                Ok(task)
            }
            "details" => {
                let offset = n(&args, "offset", 0);
                let limit = n(&args, "limit", 20).clamp(1, 100);
                Ok(
                    json!({"items":s.task.details.iter().skip(offset).take(limit).collect::<Vec<_>>(),"next_offset":(offset+limit<s.task.details.len()).then_some(offset+limit)}),
                )
            }
            "update" => {
                let patch = args["patch"]
                    .as_object()
                    .ok_or_else(|| anyhow::anyhow!("patch required"))?;
                let mut value = json!(s.task);
                for (k, v) in patch {
                    if k == "revision" {
                        bail!("revision is program-owned");
                    }
                    value[k] = v.clone();
                }
                let mut next: TaskState = serde_json::from_value(value)?;
                for id in &next.memory_ids {
                    s.memory.get(id)?;
                }
                next.revision = s.task.revision + 1;
                let mut compact = json!(next);
                compact.as_object_mut().unwrap().remove("details");
                if context::count(&compact, &s.config.model) > s.config.state_tokens {
                    bail!("task_state_limit");
                }
                if serde_json::to_vec(&next)?.len() > s.config.memory_bytes {
                    bail!("task_detail_limit");
                }
                s.task = next;
                Ok(json!({"revision":s.task.revision}))
            }
            _ => unreachable!(),
        },
        "history" => match text(&args, "action")? {
            "search" => Ok(s.history.search(
                args["query"].as_str().unwrap_or(""),
                n(&args, "after", 0) as u64,
                n(&args, "limit", 10),
            )),
            "read" => {
                s.history_loads += 1;
                let b = s.history.read(n(&args, "id", 0) as u64)?;
                Ok(bounded_text(
                    s,
                    &serde_json::to_string(&b.messages)?,
                    n(&args, "offset", 0),
                ))
            }
            _ => unreachable!(),
        },
        "checkpoint_complete" => {
            let cp = s
                .checkpoint
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no_checkpoint"))?;
            if cp.id != text(&args, "id")? {
                bail!("checkpoint_id_mismatch");
            }
            if cp.failed {
                bail!("checkpoint_has_failed_operations: retry on next request");
            }
            let no_save = args["no_save_reason"]
                .as_str()
                .is_some_and(|x| !x.trim().is_empty());
            if !no_save && s.memory.generation == cp.starting_memory_generation {
                bail!(
                    "checkpoint_memory_missing: save needed memories first, or provide no_save_reason when existing memories already preserve the facts"
                );
            }
            let progress = text(&args, "progress")?;
            if progress.trim().is_empty() {
                bail!("checkpoint progress must be nonempty");
            }
            let mut task = s.task.clone();
            task.current = progress.to_string();
            if let Some(next) = args["next"].as_str() {
                task.next = next.to_string();
            }
            task.revision += 1;
            let mut compact = json!(task);
            compact.as_object_mut().unwrap().remove("details");
            if context::count(&compact, &s.config.model) > s.config.state_tokens {
                bail!(
                    "task_state_limit: shorten checkpoint progress/next; original progress retained"
                );
            }
            s.task = task;
            s.checkpoint.as_mut().unwrap().acknowledged = true;
            Ok(json!({"acknowledged":true,"state_revision":s.task.revision}))
        }
        "file_list" => {
            let files = paths(&s.project, args["pattern"].as_str(), cancel)?;
            let root = s.project.root.canonicalize()?;
            let names = files
                .iter()
                .map(|p| p.strip_prefix(&root).unwrap().display().to_string())
                .collect::<Vec<_>>();
            let fingerprint = hash(serde_json::to_string(&names)?.as_bytes());
            let offset = page_cursor(&args, &fingerprint)?;
            if offset > names.len() {
                bail!("invalid_cursor");
            }
            let end = (offset + n(&args, "limit", 100).clamp(1, 500)).min(names.len());
            Ok(
                json!({"hash":fingerprint,"paths":names[offset..end],"next_cursor":(end<names.len()).then(||format!("{fingerprint}:{end}"))}),
            )
        }
        "source_search" => {
            let query = text(&args, "query")?;
            if query.is_empty() {
                bail!("query required");
            }
            let expression = if args["regex"].as_bool().unwrap_or(false) {
                query.to_string()
            } else {
                regex::escape(query)
            };
            let regex = regex::RegexBuilder::new(&expression)
                .size_limit(1024 * 1024)
                .build()?;
            let files = paths(&s.project, args["pattern"].as_str(), cancel)?;
            let mut rows = vec![];
            let mut fingerprint = Sha256::new();
            fingerprint.update(expression.as_bytes());
            for path in files {
                if cancel.is_cancelled() {
                    bail!("cancelled");
                }
                let contents = read_text(&path)?;
                let content_hash = hash(contents.as_bytes());
                fingerprint.update(path.to_string_lossy().as_bytes());
                fingerprint.update(content_hash.as_bytes());
                for (i, line) in contents.lines().enumerate() {
                    if i % 256 == 0 && cancel.is_cancelled() {
                        bail!("cancelled");
                    }
                    if regex.is_match(line) {
                        rows.push((
                            path.clone(),
                            content_hash.clone(),
                            i + 1,
                            line.chars().take(500).collect::<String>(),
                        ));
                        if rows.len() > 100000 {
                            bail!("search_too_broad: narrow path pattern");
                        }
                    }
                }
            }
            let fingerprint = format!("{:x}", fingerprint.finalize());
            let offset = page_cursor(&args, &fingerprint)?;
            if offset > rows.len() {
                bail!("invalid_cursor");
            }
            let end = (offset + n(&args, "limit", 20).clamp(1, 100)).min(rows.len());
            let mut result = vec![];
            for (path, content_hash, line, excerpt) in &rows[offset..end] {
                let source = observe_hashed(s, path, content_hash.clone(), *line, *line, excerpt);
                result.push(json!({"path":path,"line":line,"text":excerpt,"source":source}));
            }
            Ok(
                json!({"hash":fingerprint,"matches":result,"next_cursor":(end<rows.len()).then(||format!("{fingerprint}:{end}"))}),
            )
        }
        "file_read" => {
            let path = read_path(&s.project, text(&args, "path")?)?;
            let contents = read_text(&path)?;
            let start = n(&args, "start_line", 1).max(1);
            let lines = n(&args, "max_lines", 120).clamp(1, 2000);
            let offset = n(&args, "offset", 0);
            let selected = contents
                .lines()
                .skip(start - 1)
                .take(lines)
                .collect::<Vec<_>>()
                .join("\n");
            let prior = s
                .history
                .bundles
                .iter()
                .filter(|b| b.active)
                .flat_map(|b| &b.messages)
                .filter(|m| {
                    m["role"] == "tool"
                        && m["content"]
                            .as_str()
                            .and_then(|v| serde_json::from_str::<Value>(v).ok())
                            .is_some_and(|v| {
                                v["data"]["path"] == json!(path)
                                    && v["data"]["source"]["hash"] == hash(contents.as_bytes())
                                    && v["data"]["read_start"] == start
                                    && v["data"]["read_offset"] == offset
                                    && v["data"]["read_max_lines"] == lines
                            })
                })
                .count();
            if prior >= s.config.repeated_read_limit
                && !args["force_read"].as_bool().unwrap_or(false)
            {
                return Ok(
                    json!({"path":path,"hash":hash(contents.as_bytes()),"total_lines":contents.lines().count(),"repeated_read":true,"suppressed":true,"guidance":"Unchanged range already present repeatedly in active context. Reuse it, read another range, use document_inspect for output metadata, or force_read=true for deliberate verification."}),
                );
            }
            let mut content = bounded_text(s, &selected, offset);
            let shown = content["text"].as_str().unwrap();
            let observed_start =
                start + selected.chars().take(offset).filter(|c| *c == '\n').count();
            let source = observe(
                s,
                &path,
                &contents,
                observed_start,
                observed_start + shown.lines().count().saturating_sub(1),
                shown,
            );
            let line_offsets = std::iter::once(0)
                .chain(
                    shown
                        .chars()
                        .enumerate()
                        .filter(|(_, c)| *c == '\n')
                        .map(|(i, _)| i + 1),
                )
                .collect::<Vec<_>>();
            content["line_start"] = json!(observed_start);
            content["line_offsets"] = json!(line_offsets);
            let truncated = content["truncated"].as_bool().unwrap();
            let next_line = if truncated {
                Some(start)
            } else {
                (start - 1 + lines < contents.lines().count()).then_some(start + lines)
            };
            Ok(
                json!({"path":path,"total_lines":contents.lines().count(),"hash":hash(contents.as_bytes()),"read_start":start,"read_offset":offset,"read_max_lines":lines,"content":content,"source":source,"next_line":next_line,"next_offset":if truncated{content["next_offset"].clone()}else{json!(0)}}),
            )
        }
        "document_edit" => {
            let path = output_path(&s.project)?;
            let exists = path.exists();
            let old = if exists {
                read_text(&path)?
            } else {
                String::new()
            };
            let action = text(&args, "action")?;
            let new = text(&args, "text")?;
            if action == "create" && exists {
                bail!("document_exists");
            }
            if exists && args["expected_hash"].as_str() != Some(hash(old.as_bytes()).as_str()) {
                bail!("document_revision_conflict: read output and retry");
            }
            if !exists && action != "create" && action != "write" {
                bail!("document_missing");
            }
            let result = match action {
                "create" | "write" => new.to_string(),
                "append" => format!("{old}{new}"),
                "section" => {
                    let heading = text(&args, "section")?;
                    let target = section_text(&old, heading)?;
                    if args["expected_section_hash"].as_str()
                        != Some(hash(target.as_bytes()).as_str())
                    {
                        bail!("section_revision_conflict");
                    }
                    if new.lines().next() != Some(heading) {
                        bail!("section replacement must retain its heading");
                    }
                    let mut candidate = old.replacen(target, &format!("{}\n", new.trim_end()), 1);
                    section_text(&candidate, heading)?;
                    if !old.ends_with('\n') && target == old {
                        candidate = new.to_string();
                    }
                    candidate
                }
                "patch" => {
                    let target = text(&args, "old_text")?;
                    if target.is_empty() || old.matches(target).count() != 1 {
                        bail!("patch_target_must_match_once");
                    }
                    old.replacen(target, new, 1)
                }
                _ => unreachable!(),
            };
            let parent = path.parent().unwrap();
            std::fs::create_dir_all(parent)?;
            let checked = output_path(&s.project)?;
            if checked != path {
                bail!("output_changed");
            }
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            temp.write_all(result.as_bytes())?;
            temp.as_file().sync_all()?;
            if cancel.is_cancelled() {
                bail!("cancelled");
            }
            if path.exists() != exists
                || (exists && hash(read_text(&path)?.as_bytes()) != hash(old.as_bytes()))
            {
                bail!("document_revision_conflict: changed during write");
            }
            if exists {
                temp.persist(&path)?;
            } else {
                temp.persist_noclobber(&path)?;
            }
            s.document_written = true;
            revalidate(s)?;
            Ok(
                json!({"path":path,"hash":hash(result.as_bytes()),"bytes":result.len(),"total_lines":result.lines().count()}),
            )
        }
        "investigation" => match text(&args, "action")? {
            "list" => {
                revalidate(s)?;
                let offset = n(&args, "offset", 0);
                let limit = n(&args, "limit", 20).clamp(1, 100);
                Ok(
                    json!({"items":s.investigations.iter().skip(offset).take(limit).collect::<Vec<_>>(),"next_offset":(offset+limit<s.investigations.len()).then_some(offset+limit)}),
                )
            }
            "upsert" => {
                let id = args["id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(crate::memory::id);
                let previous = s.investigations.iter().find(|item| item.id == id).cloned();
                if previous.is_none()
                    && s.investigations
                        .iter()
                        .any(|item| item.title == args["title"].as_str().unwrap_or(""))
                {
                    bail!("duplicate_investigation_title: list and update the existing item by ID");
                }
                let refs = list(&args, "memory_ids")
                    .into_iter()
                    .map(|id| s.memory.get(&id).map(|m| (m.id.clone(), m.revision)))
                    .collect::<Result<_>>()?;
                let sources = s.source_refs(&list(&args, "source_ids"))?;
                let item = Investigation {
                    id: id.clone(),
                    title: text(&args, "title")?.into(),
                    status: args["status"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            previous
                                .as_ref()
                                .map(|i| {
                                    if i.status == "verified" {
                                        "written".into()
                                    } else {
                                        i.status.clone()
                                    }
                                })
                                .unwrap_or("uninvestigated".into())
                        }),
                    memory_refs: if args.get("memory_ids").is_none() {
                        previous
                            .as_ref()
                            .map(|i| i.memory_refs.clone())
                            .unwrap_or(refs)
                    } else {
                        refs
                    },
                    sources: if args.get("source_ids").is_none() {
                        previous
                            .as_ref()
                            .map(|i| i.sources.clone())
                            .unwrap_or(sources)
                    } else {
                        sources
                    },
                    section: args["section"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            previous
                                .as_ref()
                                .map(|i| i.section.clone())
                                .unwrap_or_default()
                        }),
                    document_hash: None,
                    note: String::new(),
                };
                if let Some(existing) = s.investigations.iter_mut().find(|i| i.id == id) {
                    *existing = item
                } else {
                    s.investigations.push(item);
                }
                Ok(json!({"id":id}))
            }
            "verify_batch" => {
                let items = args["items"].as_object().ok_or_else(|| {
                    anyhow::anyhow!("items must be an object keyed by investigation ID")
                })?;
                if items.is_empty() || items.len() > 20 {
                    bail!("batch requires 1..20 items");
                }
                let mut results = vec![];
                for (id, entry) in items {
                    if cancel.is_cancelled() {
                        bail!("cancelled");
                    }
                    let mut params = entry.clone();
                    if !params.is_object() {
                        results.push(
                            json!({"id":id,"status":"error","error":"item must be an object"}),
                        );
                        continue;
                    }
                    params["action"] = json!("verify");
                    params["id"] = json!(id);
                    let result = execute_cancellable(s, "investigation", params, cancel);
                    results.push(json!({"id":id,"result":envelope(result)}));
                }
                Ok(
                    json!({"results":results,"semantic_verification":"agent attestation; not program proof"}),
                )
            }
            "verify" => {
                revalidate(s)?;
                let id = text(&args, "id")?;
                let note = text(&args, "verification_note")?;
                if note.trim().is_empty() {
                    bail!("verification_note required");
                }
                let sources = s.source_refs(&list(&args, "source_ids"))?;
                if sources.is_empty()
                    || sources.iter().any(|source| {
                        source.origin != "file" || source.path.is_none() || source.hash.is_none()
                    })
                {
                    bail!(
                        "verification requires observed file sources; pass source_ids returned by file_read/source_search/symbol_search"
                    );
                }
                for source in &sources {
                    if let Some(path) = &source.path {
                        let path = read_path(&s.project, path)?;
                        if Some(hash(&std::fs::read(path)?)) != source.hash {
                            bail!("source_changed: read again");
                        }
                    }
                }
                let doc = read_text(&output_path(&s.project)?)?;
                let item = s
                    .investigations
                    .iter_mut()
                    .find(|i| i.id == id)
                    .ok_or_else(|| anyhow::anyhow!("item_not_found"))?;
                if item.status != "written" && item.status != "verified" {
                    bail!("item_must_be_written_before_verification");
                }
                for (id, revision) in &item.memory_refs {
                    let memory = s.memory.get(id)?;
                    if memory.revision != *revision
                        || memory.status != crate::memory::MemoryStatus::Active
                    {
                        bail!("memory_changed: refresh the memory and investigation references");
                    }
                }
                let section = section_text(&doc, &item.section)?;
                item.document_hash = Some(hash(section.as_bytes()));
                item.sources = sources;
                item.note = note.into();
                item.status = "verified".into();
                Ok(json!({"verified":id}))
            }
            "final_check" => {
                revalidate(s)?;
                if s.investigations.is_empty()
                    || s.investigations.iter().any(|i| i.status != "verified")
                {
                    return Ok(
                        json!({"complete":false,"incomplete":s.investigations.iter().filter(|i|i.status != "verified").map(|i|json!({"id":i.id,"title":i.title,"status":i.status,"section":i.section})).collect::<Vec<_>>(),"review":s.reviews,"guidance":"Verify pending items first; incomplete preflight does not consume a document review."}),
                    );
                }
                let audit = documentation::execute(s, "document_audit", &json!({}), cancel)?;
                if audit["structural_ok"] != true {
                    return Ok(
                        json!({"complete":false,"audit":audit,"review":s.reviews,"guidance":"Fix structural evidence issues before final review."}),
                    );
                }
                if s.reviews >= s.config.review_limit {
                    bail!("review_budget_exhausted");
                }
                s.reviews += 1;
                revalidate(s)?;
                let incomplete: Vec<_> = s
                    .investigations
                    .iter()
                    .filter(|i| i.status != "verified")
                    .map(|i| json!({"id":i.id,"title":i.title,"status":i.status}))
                    .collect();
                Ok(
                    json!({"complete":!s.investigations.is_empty()&&incomplete.is_empty(),"incomplete":incomplete,"review":s.reviews,"output":s.project.output}),
                )
            }
            _ => unreachable!(),
        },
        _ => bail!("unsupported_tool"),
    }
}

/// Deterministic output checks; this does not attest semantic accuracy.
pub fn audit_document(s: &mut Session) -> Result<Value> {
    documentation::execute(
        s,
        "document_audit",
        &json!({}),
        &tokio_util::sync::CancellationToken::new(),
    )
}

pub fn run_call(s: &mut Session, call: &crate::llm::ToolCall) -> Value {
    run_call_cancellable(s, call, &tokio_util::sync::CancellationToken::new())
}
/// Budget the actual chat message, including JSON escaping and call ID.
pub fn result_tokens(call: &crate::llm::ToolCall, result: &Value, model: &str) -> usize {
    context::count(
        &json!({"role":"tool","tool_call_id":call.id,"content":result.to_string()}),
        model,
    )
}

/// Keep results structured. Repeated bounding reuses the original archive rather
/// than serializing a preview of a preview (which expands escapes and hides IDs).
pub fn limit_result(
    s: &mut Session,
    call: &crate::llm::ToolCall,
    result: Value,
    limit: usize,
) -> Value {
    if result_tokens(call, &result, &s.config.model) <= limit {
        return result;
    }
    let mut output = result.clone();
    fn strip_duplicate_excerpts(value: &mut Value) {
        match value {
            Value::Object(fields) => {
                if fields.contains_key("observed_at") && fields.contains_key("origin") {
                    fields.remove("excerpt");
                }
                for v in fields.values_mut() {
                    strip_duplicate_excerpts(v);
                }
            }
            Value::Array(values) => {
                for v in values {
                    strip_duplicate_excerpts(v);
                }
            }
            _ => {}
        }
    }
    strip_duplicate_excerpts(&mut output);
    if result_tokens(call, &output, &s.config.model) <= limit {
        return output;
    }
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let old_archive = result["archive_id"]
        .as_u64()
        .or_else(|| {
            (result["next_cursor"]["tool"] == "history")
                .then(|| result["next_cursor"]["id"].as_u64())
                .flatten()
        })
        .filter(|id| s.history.read(*id).is_ok());
    let archive = if call.name == "history" && args["action"] == "read" {
        args["id"].as_u64().unwrap_or_default()
    } else if let Some(id) = old_archive {
        id
    } else {
        let id = s.history.push(
            vec![json!({"role":"tool_archive","call_id":call.id,"result":result})],
            true,
        );
        s.history.bundles.back_mut().unwrap().active = false;
        id
    };
    output["truncated"] = json!(true);
    output["archive_id"] = json!(archive);
    output["next_cursor"] = json!({"tool":"history","action":"read","id":archive,"offset":0});
    for pointer in ["/data/content/text", "/data/text", "/data/body/text"] {
        if let Some(text) = output
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
            let chars: Vec<_> = text.chars().collect();
            let template = output.clone();
            let (mut low, mut high) = (0, chars.len());
            let candidate = |length: usize| {
                let mut v = template.clone();
                *v.pointer_mut(pointer).unwrap() =
                    json!(chars[..length].iter().collect::<String>());
                let offset = args["offset"].as_u64().unwrap_or(0) + length as u64;
                if pointer == "/data/content/text" && call.name == "file_read" {
                    let line = args["start_line"].as_u64().unwrap_or(1).max(1);
                    v["data"]["content"]["line_offsets"] = json!(
                        std::iter::once(0)
                            .chain(
                                chars[..length]
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, c)| **c == '\n')
                                    .map(|(i, _)| i + 1)
                            )
                            .collect::<Vec<_>>()
                    );
                    v["data"]["content"]["truncated"] = json!(true);
                    v["data"]["content"]["next_offset"] = json!(offset);
                    v["data"]["next_line"] = json!(line);
                    v["data"]["next_offset"] = json!(offset);
                    v["next_cursor"] = json!({"tool":"file_read","path":args["path"],"start_line":line,"max_lines":args["max_lines"].as_u64().unwrap_or(120),"offset":offset});
                } else if pointer == "/data/content/text" && call.name == "document_inspect" {
                    v["data"]["content"]["truncated"] = json!(true);
                    v["data"]["content"]["next_offset"] = json!(offset);
                    v["next_cursor"] = json!({"tool":"document_inspect","section":args["section"],"offset":offset,"expected_hash":v["data"]["hash"]});
                } else if pointer == "/data/body/text" && call.name == "memory_read" {
                    v["data"]["body"]["truncated"] = json!(true);
                    v["data"]["body"]["next_offset"] = json!(offset);
                    v["next_cursor"] =
                        json!({"tool":"memory_read","id":args["id"],"offset":offset});
                } else if pointer == "/data/text" && call.name == "history" {
                    v["data"]["truncated"] = json!(true);
                    v["data"]["next_offset"] = json!(offset);
                    v["next_cursor"]["offset"] = json!(offset);
                }
                v
            };
            while low < high {
                let mid = (low + high).div_ceil(2);
                if result_tokens(call, &candidate(mid), &s.config.model) <= limit {
                    low = mid;
                } else {
                    high = mid - 1;
                }
            }
            output = candidate(low);
            if low > 0 && result_tokens(call, &output, &s.config.model) <= limit {
                return output;
            }
        }
    }
    // Collections stay structured; the top-level cursor retrieves omitted rows
    // from the original archive, never from another abbreviated result.
    for field in ["paths", "matches", "items"] {
        while output["data"][field]
            .as_array()
            .is_some_and(|a| !a.is_empty())
        {
            if result_tokens(call, &output, &s.config.model) <= limit {
                return output;
            }
            output["data"][field].as_array_mut().unwrap().pop();
            output["data"]["next_cursor"] = Value::Null;
        }
    }
    let mut compact = json!({"status":result["status"],"truncated":true,"data":{"message":"Result retained in history; follow next_cursor."},"next_cursor":{"tool":"history","action":"read","id":archive,"offset":0}});
    if let Some(id) = result.pointer("/data/source/id") {
        compact["data"]["source_id"] = id.clone();
    }
    if result_tokens(call, &compact, &s.config.model) > limit {
        compact["data"] = Value::Null;
    }
    compact
}

pub fn run_call_cancellable(
    s: &mut Session,
    call: &crate::llm::ToolCall,
    cancel: &tokio_util::sync::CancellationToken,
) -> Value {
    let signature = format!("{}:{}", call.name, call.arguments);
    if let Some((stored, result)) = s.ledger.get(&call.id) {
        return if stored == &signature {
            result.clone()
        } else {
            envelope(Err(anyhow::anyhow!("call_id_collision")))
        };
    }
    let result = serde_json::from_str(&call.arguments)
        .map_err(Into::into)
        .and_then(|args| execute_cancellable(s, &call.name, args, cancel));
    if result.is_err()
        && let Some(cp) = &mut s.checkpoint
    {
        cp.failed = true;
        cp.acknowledged = false;
    }
    let output = limit_result(s, call, envelope(result), s.config.result_tokens);
    // Failed mutations are not cached, allowing deliberate recovery with corrected arguments.
    if output["status"] == "ok" {
        s.ledger
            .insert(call.id.clone(), (signature, output.clone()));
    }
    output
}

/// Stable source fingerprint for repeatable evaluation; output is excluded by caller settings.
pub fn project_fingerprint(project: &Project) -> Result<String> {
    let root = project.root.canonicalize()?;
    let mut hasher = Sha256::new();
    for path in paths(project, None, &tokio_util::sync::CancellationToken::new())? {
        hasher.update(path.strip_prefix(&root)?.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(std::fs::read(path)?);
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
