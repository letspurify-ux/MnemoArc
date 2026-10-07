mod arguments;
pub mod completion_review;
mod coverage;
mod document_format;
pub mod document_review;
mod documentation;
mod file_edit;
mod memory_tools;
mod navigation;
pub mod recovery;
mod search;
mod structure;
mod suggest;
mod writes;
pub(crate) use writes::{external_write, uncertain_write_error};
pub mod task_plan;
use crate::{
    config::Project,
    context::{self},
    memory::{MemoryInput, Source},
    session::{Session, TaskState},
};
use anyhow::{Result, bail};
pub use coverage::record_delivered_read;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
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
/// Recognize additive document work while ignoring empty-line churn. Semantic
/// acceptance remains the responsibility of document and completion reviews.
pub(crate) fn document_content_shape(project: &Project) -> Result<(usize, usize)> {
    let doc = read_text(&output_path(project)?)?;
    Ok((
        documentation::headings(&doc).len(),
        doc.lines().filter(|line| !line.trim().is_empty()).count(),
    ))
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
fn memory_input_schema() -> Value {
    schema(
        json!({
            "key":string(),
            "title":string(),
            "summary":string(),
            "body":string(),
            "tags":strings(),
            "kind":action(&["fact","decision","failure","question","procedure"]),
            "inferred":{"type":"boolean"},
            "source_ids":strings(),
            "metadata":{"description":"Optional JSON value preserved exactly as supplied; may be an object, array, string, number, boolean or null"},
            "expected_revision":number()
        }),
        &["title", "summary", "body", "kind"],
    )
}
fn action(values: &[&str]) -> Value {
    json!({"type":"string","enum":values})
}
pub struct ToolRegistry;

const WORKFLOW_WRITE_SCOPE_ERROR: &str = "workflow_write_scope: the source_document workflow writes only the configured output; use document_edit or document_edit_batch, and never modify project files";

fn reject_project_write_in_source_document(s: &Session, name: &str) -> Result<()> {
    if matches!(name, "file_edit" | "file_write" | "file_patch")
        && ToolRegistry::project_writes_withheld(s)
    {
        bail!(WORKFLOW_WRITE_SCOPE_ERROR);
    }
    Ok(())
}

fn task_patch_schema() -> Value {
    let mut properties = serde_json::Map::new();
    for field in ["purpose", "scope"] {
        properties.insert(field.into(), string());
    }
    for field in [
        "deliverables",
        "constraints",
        "completion",
        "findings",
        "unresolved",
        "memory_ids",
    ] {
        properties.insert(field.into(), strings());
    }
    properties.insert("details".into(), json!({"type":"array","items":{}}));
    properties.insert(
        "phase".into(),
        action(&["investigate", "draft", "verify", "answer"]),
    );
    json!({"type":"object","properties":properties,"additionalProperties":false})
}

impl ToolRegistry {
    /// Follow-up discussion may gather and remember evidence without changing
    /// files, the task plan, review verdicts or checkpoint state.
    pub fn question_allows(name: &str) -> bool {
        matches!(
            name,
            "file_read"
                | "file_list"
                | "source_search"
                | "code_outline"
                | "symbol_search"
                | "symbol_relations"
                | "symbol_read"
                | "document_inspect"
                | "source_lookup"
                | "memory_read"
                | "memory_find"
                | "memory_write"
                | "history"
                | "db_query"
        )
    }

    pub fn specs() -> Vec<ToolSpec> {
        let mut specs = vec![
            ToolSpec {
                name: "db_query",
                description: "Read user-approved Oracle queries. First use action=list to discover enabled query IDs, purpose and bind parameters; then action=run with an ID and params. SQL and activation are controlled only by the user in Settings. Results use a read-only transaction, bound rows and cells; truncated=true includes shortened cells. RAW/BLOB cells are hexadecimal strings.",
                optional: false,
                read_only: true,
                parameters: schema(
                    json!({"action":action(&["list","run"]),"id":string(),"params":{"type":"object","additionalProperties":{"type":["string","number","null"]},"description":"Named bind values declared by the selected query; strings, numbers or null"}}),
                    &["action"],
                ),
            },
            ToolSpec {
                name: "db_execute",
                description: "Execute manually enabled ad hoc Oracle operations. mode=query runs a SELECT/WITH with read-only transaction; mode=statement runs SQL and commits; mode=procedure calls a named PL/SQL procedure and commits; mode=function calls a named PL/SQL function and commits. Use named binds. Procedure/function args are ordered positional parameters with name, direction=in|out|inout, type=string|number|boolean|cursor, and value for IN/INOUT. Function requires return_type. Result rows and cells are bounded; truncated=true includes shortened cells, and RAW/BLOB cells are hexadecimal strings. These modes are enabled only by the user in Settings; procedures/functions can have side effects.",
                optional: false,
                read_only: false,
                parameters: schema(
                    json!({
                        "mode":action(&["query","statement","procedure","function"]),
                        "sql":string(),"name":string(),
                        "params":{"type":"object","maxProperties":32,"additionalProperties":{"type":["string","number","boolean","null"]},"description":"Named SQL bind values: string, number, boolean or null"},
                        "args":{"type":"array","maxItems":32,"items":{"type":"object","properties":{"name":string(),"direction":action(&["in","out","inout"]),"type":action(&["string","number","boolean","cursor"]),"value":{}},"required":["name"],"additionalProperties":false}},
                        "return_type":action(&["string","number","boolean","cursor"])
                    }),
                    &["mode"],
                ),
            },
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
                description: "Save one self-contained finding useful beyond the current step: an observed fact, decision with rationale, reusable procedure, failure lesson with its conditions, or question with lasting relevance. Pending reads, retry instructions, temporary blockers and completion updates belong in task_state or checkpoint_complete progress, not a memory. title/summary/body/kind are required plain JSON fields. Same key requires expected_revision. Missing/stale revisions return current ID/key/status/revision and a retry patch; merge it into the full intended call. Invalid input reports received/missing/unknown fields and a JSON example. Updates replace evidence: resupply observed source_ids; sources are not inherited. Never omit source_ids after unknown_source. A project path (or path:start-end) stands in for the evidence already delivered from it. For failure lessons, separate the observed tool/input conditions and actual result from a suspected cause. One failed call does not establish a universal limitation. Set inferred=true for unconfirmed explanations or generalizations, even with source_ids. Unsourced facts, inferred memories and changed evidence are needs_review. Keep stable fact keys separate from progress; never replace an observed fact with a progress summary. Metadata has no per-entry token rejection; keep it concise for index_tokens. Oversized entries remain available through memory_find/memory_read. Put details in body. kind: fact/decision/failure/question/procedure",
                optional: false,
                read_only: false,
                parameters: memory_input_schema(),
            },
            ToolSpec {
                name: "memory_read",
                description: "Load memory by ID/key; offset is character offset for bounded continuation. Returns compact metadata plus custom_metadata exactly as written. Revalidates file sources",
                optional: false,
                read_only: true,
                parameters: schema(json!({"id":string(),"offset":number()}), &["id"]),
            },
            ToolSpec {
                name: "memory_find",
                description: "Search memories by ID/key, title, tags, summary, source path or body. Unicode/case and identifier word boundaries are normalized; full ID/key matches rank first, followed by term coverage, field relevance and memory status. Tags are exact AND filters. An empty query lists every status by recency. Needs-review/superseded results remain discoverable and are not verified facts. Use next cursor until exhausted; it is bound to memory generation and the exact query/tag filters and expires if either changes",
                optional: false,
                read_only: true,
                parameters: schema(
                    json!({"query":string(),"tags":strings(),"cursor":string(),"limit":number()}),
                    &[],
                ),
            },
            ToolSpec {
                name: "memory_manage",
                description: "List cleanup candidates, delete unreferenced memories, or atomically replace IDs and redirect references. delete requires ids; replace requires ids and replacement; duplicate IDs are ignored. replacement follows the same reusable-knowledge and inference rules as memory_write; keep transient progress in task_state/checkpoint_complete. It uses memory_write fields and must use a new key or a key among the replaced IDs; when it reuses a replaced key, expected_revision and observed-source rules are checked before removal",
                optional: false,
                read_only: false,
                parameters: schema(
                    json!({"action":action(&["candidates","delete","replace"]),"ids":strings(),"replacement":memory_input_schema()}),
                    &["action"],
                ),
            },
            ToolSpec {
                name: "task_state",
                description: "Read/update goals, constraints and completion criteria. Manage ordered work through task_plan; current/next/done and todos are not patch fields. State fields belong inside patch, e.g. {action:update,patch:{phase:verify}}. A new user task starts with a request-based completion condition; refine it into concrete checks before substantial work. The user selects task.workflow (answer or source_document) and it cannot be patched; for source_document START with completion criteria matching the user request. Do not send empty patches or completion:[]. Updates preserve omitted fields. Evidence requirements and explicit user constraints remain in force even when a plan item is removed.",
                optional: false,
                read_only: false,
                parameters: schema(
                    json!({"action":action(&["read","update","details"]),"patch":task_patch_schema(),"offset":number(),"limit":number()}),
                    &["action"],
                ),
            },
            task_plan::spec(),
            ToolSpec {
                name: "history",
                description: "Search retained raw conversation/tool bundles, or read one by ID and character offset (read requires id); unavailable/pruned ranges are explicit",
                optional: false,
                read_only: true,
                parameters: schema(
                    json!({"action":action(&["search","read"]),"query":string(),"id":number(),"offset":number(),"after":number(),"limit":number()}),
                    &["action"],
                ),
            },
            ToolSpec {
                name: "source_lookup",
                description: "Recover exact IDs from already observed sources, including stored memory evidence. Optional path is a path substring, id is an exact source ID. Returns paginated excerpts; does NOT read files or establish unseen facts. Available during checkpoints. If no matching evidence exists, preserve the claim as unresolved and investigate after checkpoint completion. Never substitute an unrelated ID.",
                optional: false,
                read_only: true,
                parameters: schema(
                    json!({"path":string(),"id":string(),"offset":number(),"limit":number()}),
                    &[],
                ),
            },
            ToolSpec {
                name: "checkpoint_complete",
                description: "Finish a checkpoint and preserve the ordered task_plan in ONE call. Required progress is a concise checkpoint summary including current blockers, pending reads and retry instructions; it does not complete or replace plan items. Saving memories is optional: write one only for a reusable finding that is not saved yet and later work needs; otherwise call this directly with progress (no_save_reason is optional). Evaluated after other calls in this batch. Continue the first unfinished plan item after cleanup.",
                optional: false,
                read_only: false,
                parameters: schema(
                    json!({"id":string(),"progress":string(),"next":string(),"no_save_reason":string()}),
                    &["id", "progress"],
                ),
            },
            ToolSpec {
                name: "file_list",
                description: "List project files. Default mode=text validates UTF-8 text; mode=paths lists regular file paths without reading contents (may include binary/large files). path_glob is a file glob such as backend/**/*.js (pattern is a legacy alias); path lists everything below one directory instead. Paginated; respects project boundaries and exclusions",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"path":string(),"path_glob":string(),"pattern":string(),"cursor":string(),"limit":number(),"mode":action(&["text","paths"])}),
                    &[],
                ),
            },
            ToolSpec {
                name: "source_search",
                description: "Search source lines: query is literal text by default. Prefer queries:[\"agent\",\"run\",\"db\"] for literal OR without regex escaping. Supply exactly one of query or queries. Use regex:true only for intentional regular expressions; punctuation such as .on( is literal unless regex:true. case_sensitive defaults true; whole_word defaults false (Unicode word boundaries). path selects one exact file, or a directory to search everything below it (no glob syntax); path_glob filters multiple files (pattern is a legacy alias). Do not combine path with path_glob or pattern. mode=matches (default) returns matching lines and source IDs; files returns matching paths; count returns matching-line counts per file. before/after add up to 20 context lines each in matches mode. Each displayed line is capped at 500 characters with truncation marked; truncated matches are navigation only and do not satisfy citation coverage, so read the full line with file_read. Reuse the same search options with cursor for pagination; limit may change. Hashes detect source changes",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"query":string(),"queries":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"string","minLength":1}},"path":string(),"regex":{"type":"boolean"},"case_sensitive":{"type":"boolean"},"whole_word":{"type":"boolean"},"mode":action(&["matches","files","count"]),"before":{"type":"integer","minimum":0,"maximum":20},"after":{"type":"integer","minimum":0,"maximum":20},"path_glob":string(),"pattern":string(),"cursor":string(),"limit":number()}),
                    &[],
                ),
            },
            ToolSpec {
                name: "document_inspect",
                description: "Inspect a document using path (relative to project.root); omit path for project.output. Uses file_read path restrictions. Returns session delivery coverage for the current hash, not proof of understanding or current context retention. coverage_offset pages missing line ranges; copy coverage.revision as expected_coverage_revision on continuation to detect intervening reads. With no section, offset is an OUTLINE HEADING INDEX, not a document line number; copy next_offset from the prior outline page. Outline entries provide level and section_path (newline-separated ancestor headings); copy section_path into section for nested or repeated headings. With section, offset is a CHARACTER INDEX inside that section; copy content.next_offset or the returned next_cursor arguments. For a document line number use file_read with start_line. section also accepts a full Markdown heading or a unique title without #; ambiguous headings are rejected. For any offset or coverage_offset > 0, copy expected_hash from the first result's hash; if unavailable, restart at offset 0 with coverage_offset 0.",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"path":string(),"section":string(),"offset":number(),"limit":number(),"coverage_offset":number(),"expected_hash":string(),"expected_coverage_revision":string()}),
                    &[],
                ),
            },
            ToolSpec {
                name: "symbol_search",
                description: "Search Tree-sitter declarations across Rust, JS/JSX, TS/TSX, Python, Java and C# files. query matches symbol names (case-insensitive substring by default); use match=exact for an exact name. path scopes one file/directory; path_glob filters files (pattern is a legacy alias); do not combine them. kind, container and max_depth follow code_outline. Returns path, hash, exact symbol_id and location; copy path/symbol_id into symbol_read or symbol_relations. Declarations/excerpts are navigation, not implementation evidence. Unsupported files and parse errors are reported. Follow next_cursor with unchanged filters; file changes expire cursors/IDs.",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"query":string(),"path_glob":string(),"pattern":string(),"cursor":string(),"limit":number()}),
                    &[],
                ),
            },
            ToolSpec {
                name: "symbol_relations",
                description: "Trace syntax calls/references without external language servers. Copy path and symbol_id from symbol_search/code_outline. relation=calls lists outgoing call sites in the symbol (nested functions have their own calls); callers finds incoming calls; references includes call and non-call uses. Optional path_glob limits scanned files; the target file is always included. resolved means a unique local syntax binding, NOT guaranteed runtime execution. Explicit imports/module paths and member access return candidate/ambiguous targets; unknown receivers or shadowed bindings remain unresolved. Never infer links from matching names alone. Results include source locations, hashes, candidate IDs, reasons and scan limitations, but no source evidence IDs. Read sites with file_read and bodies with symbol_read before describing behavior. Inbound results omit unresolved targets: an empty page is not proof of no callers. Follow next_cursor with all original arguments; source changes expire cursors/IDs.",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"path":string(),"symbol_id":string(),"relation":action(&["calls","callers","references"]),"path_glob":string(),"cursor":string(),"limit":number()}),
                    &["path", "symbol_id"],
                ),
            },
            ToolSpec {
                name: "code_outline",
                description: "Explore one Rust, JS/JSX, TS/TSX, Python, Java or C# file. For a function list use {path,view:\"compact\",max_depth:0,kind:\"function\"}; use kind:\"method\" with container and without max_depth:0 for class methods. Omit kind only for a mixed overview. Find a name with {path,query:\"handleQuestion\",match:\"exact\",kind:\"function\"}. query defaults to case-insensitive substring matching. kind filters normalized symbol_kind; container is an exact enclosing path: copy the parent class qualified_name, e.g. Store or Outer::Inner, never convert :: to Java or C# member dots. qualified_name is navigation, not a unique ID for overloaded methods. max_depth uses symbol nesting (0=top level, 1=direct members), not AST depth. Defaults: all kinds/depths, view=detailed. Compact returns names, kinds, containers, positions, a ready-to-copy location and symbol IDs, without source evidence. For parameters, defaults and declared return types use view=detailed with query and match=exact; an untruncated signature often answers without a body read. Detailed includes signature_source and signature_truncated; if truncated, read only the missing signature lines. When signature_context_start_line is present, shared declaration modifiers were omitted; use file_read there for the common prefix. A signature does not establish runtime behavior. Copy symbol_id into symbol_read for the body. Follow next_cursor preserving ALL filters and view; limit may change. Edits expire cursors/IDs. Positions are 1-based Unicode. Syntax structure does not resolve semantic references.",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"path":string(),"query":string(),"match":action(&["contains","exact"]),"case_sensitive":{"type":"boolean"},"kind":action(&["function","method","constructor","class","struct","interface","trait","impl","enum","enum_member","record","annotation","field","variable","constant","type","module","macro","property","accessor","event","delegate","operator","destructor"]),"container":{"type":"string","description":"Exact enclosing symbol path, case-sensitive. Empty string selects top-level symbols."},"max_depth":{"type":"integer","minimum":0,"description":"Maximum symbol nesting depth; 0 selects top-level declarations."},"view":action(&["compact","detailed"]),"cursor":string(),"limit":number()}),
                    &["path"],
                ),
            },
            ToolSpec {
                name: "symbol_read",
                description: "Read implementation only when the requested fact is missing from code_outline's detailed signature. Use {path,symbol_id,max_lines:30} for the start of a function; do not read a whole long function just to list arguments. start_line is an optional ABSOLUTE file line within the symbol (default symbol.start_line), max_lines is a count from 1 to 2000, capped at the symbol end. Omit both to read the whole symbol subject to file_read limits. Copy exact symbol_id from code_outline or symbol_search; stale IDs are rejected. If text is truncated, finish that requested range with file_read cursor before starting another range at next_line, at most symbol.end_line. Sources and coverage attest only delivered text. Shared declaration lines can include neighboring declarations.",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"path":string(),"symbol_id":string(),"start_line":{"type":"integer","minimum":1,"description":"Absolute file line inside the symbol, NOT a relative offset. Omit to start at the symbol's first line."},"max_lines":{"type":"integer","minimum":1,"maximum":2000,"description":"Requested line count; prefer a small range such as 30 for a function prologue."},"force_read":{"type":"boolean"}}),
                    &["path", "symbol_id"],
                ),
            },
            ToolSpec {
                name: "document_audit",
                description: "Check output citations path:line[-line], cited ranges not yet read as complete lines of the current file version (unread_citation), Markdown tables/code fences and fenced Mermaid syntax in one call. format_check reports parser coverage and warnings for unchecked UI extensions. Structural checks do NOT prove semantic correctness or browser layout. Paginated errors; the first result returns revision, and offset > 0 requires expected_revision copied from that result. Restart from offset 0 when the revision changes.",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"offset":number(),"limit":number(),"expected_revision":string()}),
                    &[],
                ),
            },
            ToolSpec {
                name: "file_read",
                description: "Read a new range with path, 1-based start_line and max_lines (line count). For a targeted source question, first locate the identifier/route with source_search or code_outline, then supply an explicit range. limit is accepted as a compatibility alias for max_lines; prefer max_lines. Do not use offset as a line number. Example: {path:\"src/agent.rs\",start_line:160,max_lines:140}. Relative paths resolve against project.root, NEVER the output directory or workspace parent. For project.output outside the project, copy the absolute path returned by document_inspect; do not shorten it to a basename. Example: root=/workspace/app and output=/workspace/app_summary.md requires path=/workspace/app_summary.md, not app_summary.md. Use document_inspect to read the configured output without supplying a path. If truncated, continue ONLY with {cursor: next_cursor.cursor}; never combine cursor with path/start_line/max_lines/offset. A cursor completes the original requested range and expires if the file changes. Once that range is complete, next_line indicates where a NEW range can start. Returned line_start/line_end describe delivered text; boundary flags mark partial lines, and partial boundary lines do not satisfy citation coverage. Use force_read=true only for deliberate repeat verification.",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"path":{"type":"string","description":"File path: relative to project.root, or the absolute configured output path returned by document_inspect. Never infer the output path from its basename."},"cursor":string(),"start_line":{"type":"integer","minimum":1},"max_lines":{"type":"integer","minimum":1,"maximum":2000},"limit":{"type":"integer","minimum":1,"maximum":2000,"description":"Compatibility alias for max_lines (number of lines). Prefer max_lines; never use a different value alongside max_lines."},"offset":number(),"force_read":{"type":"boolean"}}),
                    &[],
                ),
            },
            ToolSpec {
                name: "file_edit",
                description: "Replace exact text in an existing UTF-8 project file. Read the file first and copy its hash to expected_hash. old_text must occur exactly once unless replace_all=true. For the configured Markdown output use document_edit instead.",
                optional: true,
                read_only: false,
                parameters: schema(
                    json!({"path":string(),"old_text":string(),"new_text":string(),"expected_hash":string(),"replace_all":{"type":"boolean"}}),
                    &["path", "old_text", "new_text", "expected_hash"],
                ),
            },
            ToolSpec {
                name: "file_write",
                description: "Create or replace one UTF-8 project file. To create, omit expected_hash; to replace an existing file, read it first and provide expected_hash. For small changes use file_edit. For the configured Markdown output use document_edit instead.",
                optional: true,
                read_only: false,
                parameters: schema(
                    json!({"path":string(),"content":string(),"expected_hash":string()}),
                    &["path", "content"],
                ),
            },
            ToolSpec {
                name: "file_patch",
                description: "Apply 1..32 ordered operations across UTF-8 project files: add {path,content}, update {path,old_text,new_text,replace_all?}, replace {path,content}, move {path,to_path}, or delete {path}. The first operation on each existing file requires expected_hash from file_read. Later operations on the same file in this patch may omit it and observe prior edits; an explicitly supplied hash must match that intermediate version. Validation completes before writing. For the configured Markdown output use document_edit_batch instead.",
                optional: true,
                read_only: false,
                parameters: schema(
                    json!({"operations":{"type":"array","minItems":1,"maxItems":32,"items":{"type":"object","properties":{"action":action(&["add","update","replace","move","delete"]),"path":string(),"to_path":string(),"content":string(),"old_text":string(),"new_text":string(),"expected_hash":string(),"replace_all":{"type":"boolean"}},"required":["action","path"],"additionalProperties":false}}}),
                    &["operations"],
                ),
            },
            ToolSpec {
                name: "document_edit",
                description: "Edit ONLY configured Markdown output. Save one section at a time as its evidence is read. Inspect the outline and copy section_path when headings repeat. insert_before/insert_after add a same-level sibling beside section; insert_first_child/insert_last_child add a child under section, including a parent with no children. For a smaller change, use replace_text, delete_text, insert_before_text or insert_after_text with an exact unique old_text anchor; optional section limits matching to that subtree. Insertions keep the anchor unless text contains the exact old_text once, in which case the operation replaces it to avoid duplication. Prefer replace_text for rewrites. Do not replace the whole document merely to add or fix a small part. Existing file requires expected_hash, except replace_text, delete_text, insert_before_text and insert_after_text, whose exact old_text match is the precondition (a supplied hash is still checked). Multiple document_edit calls in one model response are applied sequentially and carry forward a successful write's hash; use document_edit_batch for related edits. section replaces an existing section INCLUDING all descendants and also requires expected_section_hash; its text must retain the original full heading. In the source_document workflow every save returns citation_check with unread cited ranges; do not patch workflow. Returns measured lines and new hash",
                optional: true,
                read_only: false,
                parameters: schema(
                    json!({"action":action(&["create","write","append","insert_before","insert_after","insert_first_child","insert_last_child","patch","replace_text","delete_text","insert_before_text","insert_after_text","section"]),"text":string(),"old_text":string(),"expected_hash":string(),"section":string(),"expected_section_hash":string()}),
                    &["action"],
                ),
            },
            ToolSpec {
                name: "document_edit_batch",
                description: "Apply 1..32 related edits to the existing configured Markdown output in order using one base expected_hash. Each edit is write, append, insert_before, insert_after, insert_first_child, insert_last_child, patch, replace_text, delete_text, insert_before_text, insert_after_text or section and observes prior edits. Copy section_path from document_inspect for repeated headings. Sibling insertion uses a same-level section anchor; child insertion uses a parent section and a heading one level deeper. Text edits use an exact unique old_text anchor, optionally within section. If insertion text contains that exact old_text once, it is treated as a replacement to avoid duplication; prefer replace_text for rewrites. All edits are prepared in memory and persisted only if every operation succeeds. Use this for related corrections from one snapshot; save new sections as their evidence is read. section replaces descendants and requires expected_section_hash.",
                optional: true,
                read_only: false,
                parameters: schema(
                    json!({"expected_hash":string(),"edits":{"type":"array","minItems":1,"maxItems":32,"items":{"type":"object","properties":{"action":action(&["write","append","insert_before","insert_after","insert_first_child","insert_last_child","patch","replace_text","delete_text","insert_before_text","insert_after_text","section"]),"text":string(),"old_text":string(),"section":string(),"expected_section_hash":string()},"required":["action"],"additionalProperties":false}}}),
                    &["expected_hash", "edits"],
                ),
            },
        ];
        let outline_fields = specs
            .iter()
            .find(|spec| spec.name == "code_outline")
            .unwrap()
            .parameters["properties"]
            .clone();
        let search_fields = specs
            .iter_mut()
            .find(|spec| spec.name == "symbol_search")
            .unwrap()
            .parameters["properties"]
            .as_object_mut()
            .unwrap();
        for field in [
            "path",
            "match",
            "case_sensitive",
            "kind",
            "container",
            "max_depth",
        ] {
            search_fields.insert(field.into(), outline_fields[field].clone());
        }
        specs
    }
    /// Closing mode finishes from gathered evidence. Broad discovery and
    /// memory maintenance are withheld; targeted reads of cited ranges remain.
    pub fn closing_blocked(name: &str) -> bool {
        [
            "file_list",
            "source_search",
            "symbol_search",
            "symbol_relations",
            "code_outline",
            "memory_write",
            "memory_read",
            "memory_find",
            "memory_manage",
            "history",
            "tool_catalog",
            "tool_select",
        ]
        .contains(&name)
    }
    /// Re-auditing the unchanged rejected document is not repair (a live run
    /// answered three times, re-checking in between); a check follows an
    /// edit, which ends repair_only.
    const REPAIR_TOOLS: &'static [&'static str] = &[
        "document_edit",
        "document_edit_batch",
        "file_read",
        "source_search",
        "symbol_read",
    ];
    fn repair_allows(name: &str) -> bool {
        Self::REPAIR_TOOLS.contains(&name)
    }
    /// A final answer was rejected by a review whose findings the unchanged
    /// document still carries. The required tool call must be a repair step,
    /// not a plan, outline or audit call that merely satisfies the requirement.
    pub fn repair_only(s: &Session) -> bool {
        s.checkpoint.is_none()
            && (s.progress_recovery.action_required || s.progress_recovery.repair_step)
            && s.is_document_work()
            && !s.document_review.issues.is_empty()
            && document_review::rejected_on_current_result(s)
    }
    /// Tools withheld in closing mode for this session. Before any document is
    /// saved, source reading is withheld as well: the only way forward is to
    /// write the document from the evidence already gathered.
    pub fn closing_withholds(s: &Session, name: &str) -> bool {
        s.checkpoint.is_none()
            && s.progress_recovery.closing.is_some()
            && (Self::closing_blocked(name)
                || (!s.document_written
                    && matches!(name, "file_read" | "symbol_read" | "source_lookup")))
    }
    /// Source documentation reads the project and writes only its configured
    /// output. A live run wrote a stray manual into the source repository with
    /// file_write after a document_edit error.
    pub fn project_writes_withheld(s: &Session) -> bool {
        s.task.workflow == "source_document"
    }

    const CHECKPOINT_TOOLS: [&str; 9] = [
        "memory_write",
        "memory_read",
        "memory_find",
        "memory_manage",
        "task_state",
        "task_plan",
        "history",
        "checkpoint_complete",
        "source_lookup",
    ];

    fn checkpoint_allowed(name: &str) -> bool {
        Self::CHECKPOINT_TOOLS.contains(&name)
    }
    fn workflow_required_tools(s: &Session) -> &'static [&'static str] {
        if s.task.workflow == "source_document" {
            &["document_edit", "document_edit_batch", "document_audit"]
        } else {
            &[]
        }
    }
    pub fn validate_tool_selection(s: &Session, names: &BTreeSet<String>) -> Result<()> {
        let missing: Vec<_> = Self::workflow_required_tools(s)
            .iter()
            .filter(|name| !names.contains(**name))
            .copied()
            .collect();
        if !missing.is_empty() {
            bail!(
                "workflow_locked: required workflow tools cannot be removed; keep {} in the tool_select set",
                missing.join(", ")
            );
        }
        let forbidden: Vec<_> = s
            .workflow_forbidden_tools()
            .iter()
            .filter(|name| names.contains(**name))
            .copied()
            .collect();
        if !forbidden.is_empty() {
            bail!(
                "workflow_forbidden: the user selected workflow={} for this session, which excludes {}; remove it from the tool_select set",
                s.workflow_mode,
                forbidden.join(", ")
            );
        }
        Ok(())
    }
    /// Add tools that became mandatory after a selection was validated.
    ///
    /// The web layer validates a running-session selection against its owner
    /// snapshot, while the agent consumes that selection later on a private
    /// session copy. A workflow can become locked in between those two
    /// points, so applying the selection must be safe at the request boundary
    /// as well as at enqueue time.
    pub fn normalize_tool_selection(s: &Session, names: &BTreeSet<String>) -> BTreeSet<String> {
        let mut normalized = names.clone();
        normalized.extend(
            Self::workflow_required_tools(s)
                .iter()
                .map(|name| (*name).to_owned()),
        );
        for name in s.workflow_forbidden_tools() {
            normalized.remove(*name);
        }
        normalized
    }
    /// Description text that refers to investigation, verification or
    /// reviews, none of which exist in the answer workflow.
    const ANSWER_DESCRIPTION_EDITS: &[(&str, &str, &str)] = &[
        (
            "document_edit",
            " Save one section at a time as its evidence is read.",
            "",
        ),
        (
            "document_edit",
            " In the source_document workflow every save returns citation_check with unread cited ranges; do not patch workflow.",
            "",
        ),
        (
            "document_edit_batch",
            "; save new sections as their evidence is read.",
            ".",
        ),
        ("task_state", "patch:{phase:verify}", "patch:{phase:answer}"),
        (
            "task_state",
            "; for source_document START with completion criteria matching the user request.",
            ".",
        ),
        (
            "task_state",
            "Evidence requirements and explicit user constraints remain",
            "Explicit user constraints remain",
        ),
    ];
    pub fn answer_description(name: &str, base: &'static str) -> &'static str {
        static DESCRIPTIONS: std::sync::OnceLock<BTreeMap<&'static str, &'static str>> =
            std::sync::OnceLock::new();
        DESCRIPTIONS
            .get_or_init(|| {
                let mut edited = BTreeMap::new();
                for spec in Self::specs() {
                    let mut text = spec.description.to_owned();
                    for (name, old, new) in Self::ANSWER_DESCRIPTION_EDITS {
                        if *name == spec.name {
                            text = text.replace(old, new);
                        }
                    }
                    if text != spec.description {
                        edited.insert(spec.name, &*text.leak());
                    }
                }
                edited
            })
            .get(name)
            .copied()
            .unwrap_or(base)
    }
    pub fn definitions(s: &Session) -> Vec<Value> {
        // Recovery focus steers an existing draft toward writing. Before any
        // document is saved, discovery is still required to find the sources.
        let recovery_focus = s.run_guidance["progress_recovery"]["active"] == true
            && s.is_document_work()
            && s.document_written;
        Self::specs().into_iter()
            .filter(|t| !s.read_only_turn || Self::question_allows(t.name))
            .filter(|t| t.name != "db_query" || s.config.database.active_queries().next().is_some())
            .filter(|t| t.name != "db_execute" || s.config.database.free_execution_enabled())
            .filter(|t| !t.optional || s.active_tools.contains(t.name))
            .filter(|t| {
                !Self::project_writes_withheld(s)
                    || !matches!(t.name, "file_edit" | "file_write" | "file_patch")
            })
            .filter(|t| s.config.memory_reuse || !["memory_read", "memory_find"].contains(&t.name))
            .filter(|t| s.checkpoint.is_none() || Self::checkpoint_allowed(t.name))
            // Only a pending checkpoint can be acknowledged. Offered on every
            // request, it was called to announce a finished task with no
            // checkpoint pending, once in each of three live runs.
            .filter(|t| t.name != "checkpoint_complete" || s.checkpoint.is_some())
            .filter(|t| {
                t.name != "source_lookup"
                    || s.checkpoint.as_ref().is_none_or(|cp| {
                        cp.source_lookup_calls < crate::context::CHECKPOINT_SOURCE_LOOKUP_LIMIT
                    })
            })
            .filter(|t| !Self::closing_withholds(s, t.name))
            .filter(|t| !Self::repair_only(s) || Self::repair_allows(t.name))
            // Second stage of the document progress ladder: after twice the
            // stall limit without a better result, stop broad discovery even
            // in the verify phase. Targeted file_read/symbol_read remain.
            .filter(|t| {
                s.checkpoint.is_some()
                    || !s.is_document_work()
                    || !s.document_written
                    || s.progress_recovery.rounds_since_best
                        < s.config.stall_round_limit.saturating_mul(2)
                    || !matches!(t.name, "file_list" | "source_search" | "symbol_search" | "symbol_relations" | "code_outline")
            })
            // Draft recovery discourages rediscovery. Verification must still
            // be able to locate a missing helper/path and finish source coverage.
            .filter(|t| !recovery_focus || s.checkpoint.is_some() || !matches!(t.name,
                "memory_write" | "memory_find" | "history")
                && (s.run_guidance["phase"] == "verify" || !matches!(t.name,
                    "file_list" | "source_search" | "symbol_search" | "symbol_relations" | "code_outline")))
            .map(|mut t| {
                // OpenAI-compatible providers reject a top-level anyOf in a
                // function parameter schema. Runtime verification-reserve
                // checks still reject broad discovery in this state.
                if s.progress_recovery.whole_write_withheld
                    && matches!(t.name, "document_edit" | "document_edit_batch")
                {
                    withhold_write_action(&mut t.parameters);
                }
                if s.task.workflow == "answer" {
                    t.description = Self::answer_description(t.name, t.description);
                }
                let fields = t.parameters["properties"].clone();
                // Keep offset for old clients, but offer the model only opaque continuation.
                if t.name == "file_read" {
                    t.parameters["properties"].as_object_mut().unwrap().remove("offset");
                    t.parameters["oneOf"] = json!([
                        {"required":["path"],"not":{"required":["cursor"]}},
                        {"required":["cursor"],"not":{"anyOf":[{"required":["path"]},{"required":["start_line"]},{"required":["max_lines"]},{"required":["limit"]},{"required":["offset"]}]}}
                    ]);
                }
                if t.name == "document_edit" {
                    t.parameters["oneOf"] = json!([
                        {"type":"object","properties":{"action":{"enum":["create","write"]},"text":fields["text"],"expected_hash":fields["expected_hash"]},"required":["text"],"additionalProperties":false},
                        {"type":"object","properties":{"action":{"const":"append"},"text":fields["text"],"expected_hash":fields["expected_hash"]},"required":["text","expected_hash"],"additionalProperties":false},
                        {"type":"object","properties":{"action":{"enum":["insert_before","insert_after","insert_first_child","insert_last_child"]},"text":fields["text"],"expected_hash":fields["expected_hash"],"section":fields["section"]},"required":["text","expected_hash","section"],"additionalProperties":false},
                        {"type":"object","properties":{"action":{"enum":["patch","replace_text","insert_before_text","insert_after_text"]},"text":fields["text"],"expected_hash":fields["expected_hash"],"old_text":fields["old_text"],"section":fields["section"]},"required":["text","old_text"],"additionalProperties":false},
                        {"type":"object","properties":{"action":{"const":"delete_text"},"expected_hash":fields["expected_hash"],"old_text":fields["old_text"],"section":fields["section"]},"required":["old_text"],"additionalProperties":false},
                        {"type":"object","properties":{"action":{"const":"section"},"text":fields["text"],"expected_hash":fields["expected_hash"],"section":fields["section"],"expected_section_hash":fields["expected_section_hash"]},"required":["text","expected_hash","section","expected_section_hash"],"additionalProperties":false}
                    ]);
                }
                if t.name == "document_edit_batch" {
                    let item_fields = t.parameters["properties"]["edits"]["items"]["properties"].clone();
                    t.parameters["properties"]["edits"]["items"] = json!({
                        "oneOf":[
                            {"type":"object","properties":{"action":{"const":"write"},"text":item_fields["text"]},"required":["action","text"],"additionalProperties":false},
                            {"type":"object","properties":{"action":{"const":"append"},"text":item_fields["text"]},"required":["action","text"],"additionalProperties":false},
                            {"type":"object","properties":{"action":{"enum":["insert_before","insert_after","insert_first_child","insert_last_child"]},"text":item_fields["text"],"section":item_fields["section"]},"required":["action","text","section"],"additionalProperties":false},
                            {"type":"object","properties":{"action":{"enum":["patch","replace_text","insert_before_text","insert_after_text"]},"text":item_fields["text"],"old_text":item_fields["old_text"],"section":item_fields["section"]},"required":["action","text","old_text"],"additionalProperties":false},
                            {"type":"object","properties":{"action":{"const":"delete_text"},"old_text":item_fields["old_text"],"section":item_fields["section"]},"required":["action","old_text"],"additionalProperties":false},
                            {"type":"object","properties":{"action":{"const":"section"},"text":item_fields["text"],"section":item_fields["section"],"expected_section_hash":item_fields["expected_section_hash"]},"required":["action","text","section","expected_section_hash"],"additionalProperties":false}
                        ]
                    });
                }
                if t.name == "source_search" {
                    t.parameters["oneOf"] = json!([
                        {"required":["query"],"not":{"required":["queries"]}},
                        {"required":["queries"],"not":{"anyOf":[{"required":["query"]},{"required":["regex"],"properties":{"regex":{"const":true}}}]}}
                    ]);
                }
                if t.name == "task_state" {
                    t.parameters["oneOf"] = json!([
                        {"properties":{"action":{"const":"read"}},"required":["action"],"not":{"anyOf":[{"required":["patch"]},{"required":["offset"]},{"required":["limit"]}]}},
                        {"properties":{"action":{"const":"details"}},"required":["action"],"not":{"required":["patch"]}},
                        {"properties":{"action":{"const":"update"}},"required":["action","patch"],"not":{"anyOf":[{"required":["offset"]},{"required":["limit"]}]}}
                    ]);
                }
                if t.name == "checkpoint_complete" {
                    // Accept old callers' next summaries, but new plans have
                    // one ordered source of truth through task_plan.
                    t.parameters["properties"].as_object_mut().unwrap().remove("next");
                }
                if t.name == "memory_manage" {
                    t.parameters["oneOf"] = json!([
                        {"properties":{"action":{"const":"candidates"}},"required":["action"],"not":{"anyOf":[{"required":["ids"]},{"required":["replacement"]}]}},
                        {"properties":{"action":{"const":"delete"}},"required":["action","ids"],"not":{"required":["replacement"]}},
                        {"properties":{"action":{"const":"replace"}},"required":["action","ids","replacement"]}
                    ]);
                }
                if t.name == "history" {
                    t.parameters["oneOf"] = json!([
                        {"properties":{"action":{"const":"search"}},"required":["action"],"not":{"anyOf":[{"required":["id"]},{"required":["offset"]}]}},
                        {"properties":{"action":{"const":"read"}},"required":["action","id"],"not":{"anyOf":[{"required":["query"]},{"required":["after"]},{"required":["limit"]}]}}
                    ]);
                }
                let description = if t.name == "db_query" {
                    let summaries = s.config.database.active_queries().map(|q| format!("{}: {} (params: {})", q.id, q.description, q.params.iter().map(|p|p.name.as_str()).collect::<Vec<_>>().join(", "))).collect::<Vec<_>>().join("; ");
                    t.parameters["properties"]["id"]["enum"] = json!(s.config.database.active_queries().map(|q|q.id.as_str()).collect::<Vec<_>>());
                    t.parameters["oneOf"] = json!([
                        {"properties":{"action":{"const":"list"}},"not":{"anyOf":[{"required":["id"]},{"required":["params"]}]}},
                        {"properties":{"action":{"const":"run"}},"required":["id"]}
                    ]);
                    format!("{} Enabled queries: {}. Use list for full parameter descriptions.", t.description, summaries.chars().take(3000).collect::<String>())
                } else if t.name == "db_execute" {
                    let db = &s.config.database;
                    let mut modes = Vec::new();
                    if db.raw_query_enabled { modes.push("query"); }
                    if db.raw_statement_enabled { modes.push("statement"); }
                    if db.procedure_enabled { modes.push("procedure"); }
                    if db.function_enabled { modes.push("function"); }
                    t.parameters["properties"]["mode"]["enum"] = json!(modes);
                    t.parameters["oneOf"] = json!([
                        {"properties":{"mode":{"const":"query"}},"required":["sql"],"not":{"anyOf":[{"required":["name"]},{"required":["args"]},{"required":["return_type"]}]}},
                        {"properties":{"mode":{"const":"statement"}},"required":["sql"],"not":{"anyOf":[{"required":["name"]},{"required":["args"]},{"required":["return_type"]}]}},
                        {"properties":{"mode":{"const":"procedure"}},"required":["name"],"not":{"anyOf":[{"required":["sql"]},{"required":["params"]},{"required":["return_type"]}]}},
                        {"properties":{"mode":{"const":"function"}},"required":["name","return_type"],"not":{"anyOf":[{"required":["sql"]},{"required":["params"]}]}}
                    ]);
                    format!("{} Enabled modes: {}.", t.description, modes.join(", "))
                } else { t.description.to_string() };
                // Per-action unions are enforced at execution. Offered to the
                // model, a top-level oneOf made at least one provider (GLM via
                // OpenRouter) drop every argument except action, e.g. a bare
                // {"action":"update"} for task_state, so it is not sent.
                if let Some(parameters) = t.parameters.as_object_mut() {
                    parameters.remove("oneOf");
                    parameters.remove("anyOf");
                }
                json!({"type":"function","function":{"name":t.name,"description":description,"parameters":t.parameters}})
            })
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
        Self::validate_inner(s, name, args).map_err(|error| arguments::annotate(error, name, args))
    }

    fn validate_inner(s: &Session, name: &str, args: &Value) -> Result<ToolSpec> {
        if s.read_only_turn && !Self::question_allows(name) {
            bail!("question_tools_not_allowed: {name} changes task state or files; task preserved");
        }
        let spec = Self::specs()
            .into_iter()
            .find(|t| t.name == name)
            .ok_or_else(|| unsupported_tool(s, name))?;
        reject_project_write_in_source_document(s, name)?;
        if spec.optional && !s.active_tools.contains(name) {
            bail!(
                "tool_not_active: {name} is an optional tool that is not active for this request; nothing was executed. Use an offered tool, or activate it with tool_select {{\"action\":\"add\",\"names\":[\"{name}\"]}} (takes effect on the next request)"
            );
        }
        if Self::closing_withholds(s, name) {
            bail!(if s.document_written {
                "closing_mode: {name} is withheld while the document is finalized; use gathered evidence, or file_read one specific cited range reported as unread"
            } else {
                "closing_mode: {name} is withheld until the document is saved; create it now with document_edit action=create from the evidence already gathered"
            }
            .replace("{name}", name));
        }
        // The offered list alone did not stop a live model from calling
        // document_audit in a forced repair step.
        if Self::repair_only(s) && !Self::repair_allows(name) {
            bail!(
                "review_repair_required: {name} is not a repair step; the reviewed document is unchanged, so edit it for document_review.issues with document_edit or document_edit_batch (read a cited source range first if needed)"
            );
        }
        if !s.config.memory_reuse && ["memory_find", "memory_read"].contains(&name) {
            bail!("unsupported: memory reuse disabled for evaluation");
        }
        let object = args.as_object().ok_or_else(|| {
            arguments::failure(
                name,
                "arguments",
                "invalid_tool_arguments",
                json!("object"),
                arguments::value_type(args),
                "must be an object",
            )
        })?;
        if name == "db_query" && s.config.database.active_queries().next().is_none() {
            bail!(
                "database_disabled: enable the database and at least one query manually in Settings"
            );
        }
        if name == "db_execute" && !s.config.database.free_execution_enabled() {
            bail!(
                "database_execution_disabled: enable an ad hoc execution mode manually in Settings"
            );
        }
        let fields = spec.parameters["properties"].as_object().unwrap();
        for key in object.keys() {
            if !fields.contains_key(key) {
                if name == "task_state" && task_patch_schema()["properties"].get(key).is_some() {
                    bail!(
                        "unknown_argument: {key} belongs inside task_state patch; use {{\"action\":\"update\",\"patch\":{{\"{key}\":...}}}}; state unchanged"
                    );
                }
                return Err(arguments::unknown_field(
                    name,
                    key,
                    &object[key],
                    &spec.parameters["properties"],
                    args,
                ));
            }
        }
        for required in spec.parameters["required"].as_array().unwrap() {
            // The batch reports a missing hash after checking its edits
            // against the current document (see missing_document_hash).
            if name == "document_edit_batch" && required == "expected_hash" {
                continue;
            }
            if !object.contains_key(required.as_str().unwrap()) {
                if name == "document_edit_batch" && !object.contains_key("expected_hash") {
                    bail!("missing_argument: {required}; expected_hash is also missing");
                }
                let key = required.as_str().unwrap();
                return Err(arguments::failure(
                    name,
                    key,
                    "missing_argument",
                    fields[key].clone(),
                    "missing",
                    "is required",
                ));
            }
        }
        for (k, v) in object {
            if name == "task_plan" && k == "operations" {
                // The plan parser accepts unambiguous JSON wrappers and
                // returns an unchanged plan for bad shapes. Rejecting here
                // would turn recoverable bookkeeping into a terminal error.
                continue;
            }
            let field = &fields[k];
            if name == "task_state" && k == "patch" && v.is_object() {
                // Retain user-owned workflow diagnostics before nested schema checks.
                continue;
            }
            arguments::validate_field(name, k, field, v, args)?;
        }
        if name == "task_state" {
            validate_task_state_arguments(args)?;
        } else if name == "document_edit" {
            validate_document_edit_arguments(args)?;
        } else if name == "document_edit_batch" {
            validate_document_edit_batch_arguments(args)?;
        } else if name == "memory_manage" {
            validate_memory_manage_arguments(args)?;
        } else if name == "history" {
            validate_history_arguments(args)?;
        }
        Ok(spec)
    }
}

/// " ; did you mean ..." for a rejected value of `field`, or "".
pub(crate) fn suggest_value(tool: &str, field: &str, received: &str, allowed: &[Value]) -> String {
    suggest::value(tool, field, &json!(received), allowed, &Value::Null)
        .map_or_else(String::new, |s| s.text)
}

/// A tool name that does not exist, with the offered tool the call most
/// likely meant: live runs called read, tool_plan and run_guidance.
fn unsupported_tool(s: &Session, name: &str) -> anyhow::Error {
    let offered: Vec<String> = ToolRegistry::definitions(s)
        .iter()
        .filter_map(|definition| definition["function"]["name"].as_str().map(str::to_owned))
        .collect();
    let offered: Vec<&str> = offered.iter().map(String::as_str).collect();
    let suggestion = suggest::tool(name, &offered);
    let mut data = json!({"execution":"not_started"});
    if let Some(target) = suggestion.as_ref().and_then(|s| s.target.as_ref()) {
        data["did_you_mean"] = json!(target);
    }
    recovery::DiagnosticError {
        message: format!(
            "unsupported_tool: {} is not a tool name; nothing was executed{}. Use an exact name from the offered tool list, or search with tool_catalog",
            name.chars().take(80).collect::<String>(),
            suggestion.map_or_else(String::new, |s| s.text)
        ),
        data,
    }
    .into()
}

const TASK_STATE_ACTIONS: &[&str] = &["read", "update", "details"];

fn task_state_fields(action: &str) -> &'static [&'static str] {
    match action {
        "details" => &["action", "offset", "limit"][..],
        "update" => &["action", "patch"][..],
        _ => &["action"][..],
    }
}

fn validate_task_state_arguments(args: &Value) -> Result<()> {
    let action = args["action"].as_str().unwrap_or("");
    validate_action_fields("task_state", args, task_state_fields, TASK_STATE_ACTIONS)?;
    if action != "update" {
        return Ok(());
    }
    let Some(patch) = args.get("patch") else {
        // A bare {"action":"update"} was repeated in a live run; show the
        // whole call shape to copy.
        bail!(
            r#"missing_argument: patch for task_state action=update. Send the fields to change inside patch in the same call, for example {{"action":"update","patch":{{"completion":["the requested result"]}}}}"#
        );
    };
    let object = patch.as_object().ok_or_else(|| {
        anyhow::anyhow!("invalid_argument_type: patch for task_state must be an object")
    })?;
    let schema = task_patch_schema();
    let properties = schema["properties"]
        .as_object()
        .expect("task patch schema properties are an object");
    for (key, value) in object {
        let Some(field) = properties.get(key) else {
            if key == "revision" {
                bail!(
                    "invalid_argument_value: patch.revision is program-owned; remove revision from patch and resend the other fields; state unchanged"
                );
            }
            if key == "workflow" {
                bail!(
                    "workflow_selected_by_user: the user selects the workflow for this session (see task.workflow); omit {key} from the patch; state unchanged"
                );
            }
            let allowed: Vec<&str> = properties.keys().map(String::as_str).collect();
            let hint = suggest::field("task_state", "patch", key, &allowed, patch)
                .map_or_else(String::new, |s| s.text);
            bail!(
                "unknown_argument: patch.{key} is not a task_state patch field{hint}; allowed patch fields: {}; state unchanged",
                allowed.join(", ")
            );
        };
        arguments::validate_field("task_state", &format!("patch.{key}"), field, value, patch)?;
    }
    Ok(())
}

/// Models sometimes emit U+FFFD for a character they failed to produce: live
/// runs wrote "제���" and "객��" into documents. Saved, the reviewer cannot quote
/// it (5 rejected review calls and a skipped page) and it survived into an
/// approved document, so reject it before anything is written.
fn reject_replacement_character(field: &str, text: &str) -> Result<()> {
    let Some(at) = text.find('\u{FFFD}') else {
        return Ok(());
    };
    let start = text[..at]
        .char_indices()
        .rev()
        .nth(19)
        .map_or(0, |(index, _)| index);
    let around: String = text[start..].chars().take(40).collect();
    bail!(
        "invalid_argument_value: {field} contains U+FFFD, a broken character, in {around:?}; rewrite that word with its intended characters and send the edit again (nothing was written)"
    );
}

const DOCUMENT_EDIT_ACTIONS: &[&str] = &[
    "create",
    "write",
    "append",
    "insert_before",
    "insert_after",
    "insert_first_child",
    "insert_last_child",
    "patch",
    "replace_text",
    "delete_text",
    "insert_before_text",
    "insert_after_text",
    "section",
];

/// A section hash guards a text edit only through the section it names.
const SECTION_HASH_WITHOUT_SECTION: &str = "checks the section that section names; add section with the heading whose section_hash it is, or drop expected_section_hash";

fn document_edit_fields(action: &str) -> &'static [&'static str] {
    match action {
        "create" | "write" | "append" => &["action", "text", "expected_hash"][..],
        // A section-scoped text edit may also guard its section: a live model
        // sent replace_text the section_hash it had read and was refused.
        "patch" | "replace_text" | "insert_before_text" | "insert_after_text" => &[
            "action",
            "text",
            "expected_hash",
            "old_text",
            "section",
            "expected_section_hash",
        ][..],
        "delete_text" => &[
            "action",
            "expected_hash",
            "old_text",
            "section",
            "expected_section_hash",
        ][..],
        "insert_before" | "insert_after" | "insert_first_child" | "insert_last_child" => {
            &["action", "text", "expected_hash", "section"][..]
        }
        "section" => &[
            "action",
            "text",
            "expected_hash",
            "section",
            "expected_section_hash",
        ][..],
        _ => &["action", "text"][..],
    }
}

fn validate_document_edit_arguments(args: &Value) -> Result<()> {
    let action = args["action"].as_str().unwrap_or("");
    // Name every missing field of this action at once; a live model fixed
    // text and then failed again on the missing old_text.
    let needed = document_edit_needs(action);
    let missing: Vec<&str> = needed
        .iter()
        .copied()
        .filter(|key| args.get(*key).is_none())
        .collect();
    validate_action_fields_with(
        "document_edit",
        args,
        document_edit_fields,
        DOCUMENT_EDIT_ACTIONS,
        document_action_hint,
    )
    // A live call refused for an extra field also lacked text, which the
    // error did not say.
    .map_err(|error| {
        if missing.is_empty() {
            error
        } else {
            anyhow::anyhow!(
                "{error}{}; action={action} needs {}",
                also_missing(&missing),
                needed.join(", ")
            )
        }
    })?;
    let require = |key: &str| {
        if args.get(key).is_none() {
            return Err(anyhow::anyhow!(
                "missing_argument: {key} for document_edit action={action}"
            ));
        }
        if args[key].as_str().is_some_and(|value| {
            if key == "old_text" {
                value.is_empty()
            } else {
                value.trim().is_empty()
            }
        }) {
            return Err(anyhow::anyhow!(
                "invalid_argument_value: {key} must not be empty for document_edit action={action}"
            ));
        }
        Ok(())
    };
    // A missing hash is reported when the edit runs, together with any
    // problem the edit has against the current document, so one retry fixes
    // both. Argument errors found here mention it for the same reason.
    let hash_deferred = requires_document_hash(action) && args.get("expected_hash").is_none();
    let also_hash = |error: anyhow::Error| {
        if hash_deferred {
            anyhow::anyhow!("{error}; expected_hash is also missing")
        } else {
            error
        }
    };
    let require = |key: &str| require(key).map_err(also_hash);
    if let [first, rest @ ..] = missing.as_slice() {
        let whole = if action == "section" && missing.contains(&"section") {
            WHOLE_DOCUMENT_HINT
        } else {
            ""
        };
        return Err(also_hash(anyhow::anyhow!(
            "missing_argument: {first} for document_edit action={action}{}; action={action} needs {}{whole}",
            also_missing(rest),
            needed.join(", ")
        )));
    }
    if let Some(text) = args["text"].as_str() {
        reject_replacement_character("text", text).map_err(also_hash)?;
    }
    if !matches!(action, "create" | "write") && args.get("expected_hash").is_some() {
        require("expected_hash")?;
    }
    match action {
        "append" => {}
        // An exact old_text that must match the current document once is
        // already the precondition, so expected_hash is optional here.
        "patch" | "replace_text" | "insert_before_text" | "insert_after_text" | "delete_text" => {
            require("old_text")?;
            if args.get("section").is_some() {
                require("section")?;
            } else if args.get("expected_section_hash").is_some() {
                return Err(also_hash(anyhow::anyhow!(
                    "invalid_argument_value: expected_section_hash of action={action} {SECTION_HASH_WITHOUT_SECTION}"
                )));
            }
            if matches!(action, "insert_before_text" | "insert_after_text")
                && args["text"].as_str().is_some_and(str::is_empty)
            {
                bail!(
                    "invalid_argument_value: text must not be empty for document_edit action={action}"
                );
            }
        }
        "section" => {
            require("section").map_err(|error| anyhow::anyhow!("{error}{WHOLE_DOCUMENT_HINT}"))?;
            require("expected_section_hash")?;
        }
        "insert_before" | "insert_after" | "insert_first_child" | "insert_last_child" => {
            require("section")?;
        }
        "create" | "write" => {}
        _ => {}
    }
    Ok(())
}

/// action=section without a heading usually means the whole document: a
/// live model sent its complete text that way, section "", and was told only
/// that section must not be empty.
const WHOLE_DOCUMENT_HINT: &str = "; action=section replaces the one section its section heading names (copy the heading from the document_inspect outline). To replace the whole document, use action=write with expected_hash and the complete text";

/// Fields a document edit action needs besides action and expected_hash,
/// text first (the order errors have always used).
fn document_edit_needs(action: &str) -> &'static [&'static str] {
    match action {
        "patch" | "replace_text" | "insert_before_text" | "insert_after_text" => {
            &["text", "old_text"]
        }
        "delete_text" => &["old_text"],
        "insert_before" | "insert_after" | "insert_first_child" | "insert_last_child" => {
            &["text", "section"]
        }
        "section" => &["text", "section", "expected_section_hash"],
        _ => &["text"],
    }
}

fn also_missing(rest: &[&str]) -> String {
    match rest {
        [] => String::new(),
        [one] => format!("; {one} is also missing"),
        more => format!("; {} are also missing", more.join(", ")),
    }
}

/// Some providers leak a model's native `<arg_key>k</arg_key><arg_value>v</arg_value>`
/// tool-call markup into one JSON key, e.g. `tags</arg_key>["a"]</arg_value><arg_key>title`
/// holding the title. Split such a key back into its arguments; a key that
/// does not parse completely is left for normal validation to reject.
/// Returns whether any key was repaired.
fn repair_leaked_argument_markup(args: &mut Value) -> Result<bool> {
    fn split(key: &str) -> Option<Vec<(String, Option<Value>)>> {
        let mut pairs = Vec::new();
        let mut rest = key;
        while let Some((name, after)) = rest.split_once("</arg_key>") {
            let after = after.trim_start();
            let after = after.strip_prefix("<arg_value>").unwrap_or(after);
            let (value, next) = after.split_once("</arg_value>")?;
            let value = value.trim();
            let value = serde_json::from_str(value).unwrap_or_else(|_| json!(value));
            pairs.push((name.trim().to_owned(), Some(value)));
            rest = next.trim_start().strip_prefix("<arg_key>")?;
        }
        pairs.push((rest.trim().to_owned(), None));
        pairs
            .iter()
            .all(|(name, _)| !name.is_empty() && !name.contains(['<', '>']))
            .then_some(pairs)
    }
    let Some(object) = args.as_object_mut() else {
        return Ok(false);
    };
    let mut repaired = false;
    let leaked: Vec<String> = object
        .keys()
        .filter(|key| key.contains("</arg_key>"))
        .cloned()
        .collect();
    for key in leaked {
        let Some(pairs) = split(&key) else {
            continue;
        };
        let last = object.remove(&key).expect("leaked key present");
        repaired = true;
        for (name, value) in pairs {
            let value = value.unwrap_or_else(|| last.clone());
            match object.get(&name) {
                Some(existing) if *existing != value => bail!(
                    "conflicting_arguments: {name} was sent twice with different values (once inside leaked <arg_key> markup); pass each argument once as a JSON field"
                ),
                Some(_) => {}
                None => {
                    object.insert(name, value);
                }
            }
        }
    }
    Ok(repaired)
}

/// Accept argument names models commonly use for an unambiguous meaning.
/// A conflicting pair is still rejected so no value is silently dropped.
/// delete_text has no replacement text. Providers that fill every field send
/// an empty text or a copy of old_text; a different text is a real conflict
/// and still reaches validation.
fn drop_filled_delete_text(object: &mut serde_json::Map<String, Value>) {
    if object.get("action").and_then(Value::as_str) == Some("delete_text")
        && object.get("text").is_some_and(|text| {
            text.is_null() || text == "" || Some(text) == object.get("old_text")
        })
    {
        object.remove("text");
    }
}

/// A next_cursor object is the continuation call itself, for example
/// {"tool":"file_read","cursor":"R2"}. A live model sent the whole object as
/// the cursor string and the read failed as an invalid cursor. When cursor
/// holds such an object for this tool (as JSON text or as an object), take
/// its cursor and the arguments the call left out or sent blank. No cursor a
/// tool issues is JSON text, so a real cursor never matches.
fn unwrap_continuation_cursor(name: &str, args: &mut Value) {
    let Some(fields) = args.as_object_mut() else {
        return;
    };
    let continuation = match fields.get("cursor") {
        Some(Value::String(text)) if text.trim_start().starts_with('{') => {
            serde_json::from_str::<Value>(text.trim()).ok()
        }
        Some(object @ Value::Object(_)) => Some(object.clone()),
        _ => None,
    };
    let Some(Value::Object(mut continuation)) = continuation else {
        return;
    };
    let names_this_tool = match continuation.remove("tool") {
        Some(tool) => tool == name,
        None => continuation.contains_key("cursor"),
    };
    let Some(spec) = ToolRegistry::specs().into_iter().find(|t| t.name == name) else {
        return;
    };
    if !names_this_tool {
        return;
    }
    fields.remove("cursor");
    for (key, value) in continuation {
        let accepted = spec.parameters["properties"]
            .as_object()
            .is_some_and(|properties| properties.contains_key(&key));
        if accepted
            && fields
                .get(&key)
                .is_none_or(|current| current.is_null() || current == "")
        {
            fields.insert(key, value);
        }
    }
}

/// A live model sent lone carriage returns as the line breaks of its document
/// edits (19 to 40 each); saved, they showed as broken text that the review
/// reported three times. A carriage return without a line feed is no
/// Markdown line ending, so it is the line break the text meant. Old text
/// gets the same treatment, so a passage copied from such an edit still
/// matches what was saved; a CRLF pair stays as it is.
fn normalize_lone_carriage_returns(name: &str, args: &mut Value) {
    fn line_breaks(value: &mut Value) {
        let Some(text) = value.as_str().filter(|text| text.contains('\r')) else {
            return;
        };
        let mut fixed = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            fixed.push(if c == '\r' && chars.peek() != Some(&'\n') {
                '\n'
            } else {
                c
            });
        }
        *value = Value::String(fixed);
    }
    let edits: Vec<&mut Value> = match name {
        "document_edit" => vec![args],
        "document_edit_batch" => args
            .get_mut("edits")
            .and_then(Value::as_array_mut)
            .map(|edits| edits.iter_mut().collect())
            .unwrap_or_default(),
        _ => return,
    };
    for edit in edits {
        for key in ["text", "old_text"] {
            if let Some(value) = edit.get_mut(key) {
                line_breaks(value);
            }
        }
    }
}

/// A live model sent task_state its whole call as the patch, as JSON text
/// ({"patch":"{\"action\":\"update\",\"patch\":{...}}"}), and was refused for
/// a missing action. No patch field is called action, so a patch naming one
/// is that call; one that contradicts the outer action is left to validation.
fn unwrap_task_state_call(name: &str, args: &mut Value) {
    if name != "task_state" {
        return;
    }
    let Some(fields) = args.as_object_mut() else {
        return;
    };
    let call = match fields.get("patch") {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text.trim()).ok(),
        Some(call @ Value::Object(_)) => Some(call.clone()),
        _ => None,
    };
    let Some(Value::Object(call)) = call else {
        return;
    };
    let Some(action) = call.get("action").and_then(Value::as_str) else {
        return;
    };
    if fields
        .get("action")
        .is_some_and(|outer| outer.as_str() != Some(action))
    {
        return;
    }
    fields.remove("patch");
    fields.extend(call);
}

/// Whether a path argument names the project root directory itself.
fn names_project_root(s: &Session, path: &str) -> bool {
    let path = path.trim();
    matches!(path, "." | "./")
        || s.project
            .root
            .canonicalize()
            .is_ok_and(|root| read_path(&s.project, path).is_ok_and(|given| given == root))
}

fn normalize_argument_aliases(s: &Session, name: &str, args: &mut Value) -> Result<()> {
    // A glob is relative to project.root already, so a path naming the root
    // narrows nothing: a live model sent the root's absolute path with
    // path_glob and was refused for conflicting filters.
    if matches!(name, "file_list" | "source_search" | "symbol_search")
        && let Some(fields) = args.as_object_mut()
        && (fields.contains_key("path_glob") || fields.contains_key("pattern"))
        && fields
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|path| names_project_root(s, path))
    {
        fields.remove("path");
    }
    fn rename(
        object: &mut serde_json::Map<String, Value>,
        alias: &str,
        key: &str,
        at: &str,
    ) -> Result<()> {
        let Some(value) = object.remove(alias) else {
            return Ok(());
        };
        match object.get(key) {
            Some(existing) if *existing != value => {
                bail!("conflicting_arguments: {at}{alias} and {at}{key} differ; pass only {key}")
            }
            Some(_) => {}
            None => {
                object.insert(key.into(), value);
            }
        }
        Ok(())
    }
    match name {
        // The schema shares details paging with every action; providers that
        // fill every field send it with read/update too. Paging changes no
        // state, so it is dropped where the action does not page.
        "task_state" => {
            if args["action"] != "details"
                && let Some(object) = args.as_object_mut()
            {
                for key in ["offset", "limit"] {
                    if object
                        .get(key)
                        .is_some_and(|value| value.as_u64().is_some())
                    {
                        object.remove(key);
                    }
                }
            }
            // Restating the user's workflow selection changes nothing; only an
            // attempt to reclassify it is rejected.
            // The same holds for the program-owned checkpoint summary echoed
            // back from the task state.
            let workflow = json!(s.task.workflow);
            if let Some(patch) = args.get_mut("patch").and_then(Value::as_object_mut) {
                for (key, current) in [
                    ("workflow", workflow),
                    ("checkpoint_summary", json!(s.task.checkpoint_summary)),
                ] {
                    if patch.get(key) == Some(&current) {
                        patch.remove(key);
                    }
                }
            }
        }
        "document_edit" => {
            if let Some(object) = args.as_object_mut() {
                rename(object, "new_text", "text", "")?;
                drop_filled_delete_text(object);
                // Appending to a document that does not exist yet is creating
                // it; a live run spent rounds on hashes for a missing output.
                // Without text the call stays an append, so its error names
                // the action the model actually sent.
                if object.get("action").and_then(Value::as_str) == Some("append")
                    && object.contains_key("text")
                    && output_path(&s.project).is_ok_and(|path| !path.exists())
                {
                    object.insert("action".into(), json!("create"));
                    object.remove("expected_hash");
                }
                // Whole-text actions take no target; filled empty placeholders
                // for targeted edits carry no meaning there.
                if matches!(
                    object.get("action").and_then(Value::as_str),
                    Some("create" | "write" | "append")
                ) {
                    for key in [
                        "expected_section_hash",
                        "old_text",
                        "section",
                        "expected_hash",
                    ] {
                        if object.get(key).is_some_and(|value| value == "") {
                            object.remove(key);
                        }
                    }
                }
                // An edit right after the model's own successful write often
                // omits expected_hash (append in three live runs in a row,
                // insert_* in several more). If the document is still exactly
                // that write, its hash is known to the model already; any
                // other change keeps the requirement.
                if object
                    .get("action")
                    .and_then(Value::as_str)
                    .is_some_and(requires_document_hash)
                {
                    fill_hash_of_own_write(s, object);
                }
            }
        }
        "document_edit_batch" => {
            // Malformed (non-object) arguments reach normal validation.
            if let Some(edits) = args.get_mut("edits").and_then(Value::as_array_mut) {
                for (index, edit) in edits.iter_mut().enumerate() {
                    if let Some(object) = edit.as_object_mut() {
                        rename(object, "new_text", "text", &format!("edits[{index}]."))?;
                        drop_filled_delete_text(object);
                    }
                }
            }
        }
        "document_audit" => {
            if let Some(object) = args.as_object_mut() {
                rename(object, "max_issues", "limit", "")?;
                // The audit always reads the configured output. A live model
                // named that output in path and was refused as an unknown
                // argument; any other path is still refused.
                let names_output = object
                    .get("path")
                    .and_then(Value::as_str)
                    .is_some_and(|path| {
                        path.is_empty()
                            || output_path(&s.project)
                                .is_ok_and(|output| documentation::names_output(s, path, &output))
                    });
                if names_output {
                    object.remove("path");
                }
            }
        }
        // Shared-schema placeholders: search does not use a history id or
        // text offset, and read does not search.
        "history" => {
            let unused: &[&str] = match args["action"].as_str() {
                Some("search") => &["id", "offset"],
                Some("read") => &["query", "after", "limit"],
                _ => &[],
            };
            if let Some(object) = args.as_object_mut() {
                for key in unused {
                    if object.get(*key).is_some_and(|value| {
                        value.is_null() || value == &json!(0) || value == &json!("")
                    }) {
                        object.remove(*key);
                    }
                }
            }
        }
        // A null optional field means the field was not supplied.
        "memory_write" => {
            if let Some(object) = args.as_object_mut() {
                let schema = memory_input_schema();
                object.retain(|key, value| {
                    !value.is_null()
                        || schema["properties"].get(key).is_none()
                        || schema["required"].as_array().unwrap().contains(&json!(key))
                        || key == "metadata"
                });
            }
        }
        // Only apply takes operations: a live model sent
        // {"expected_revision":0,"operations":"[...]"} with no action and was
        // refused for the missing action.
        "task_plan"
            if args.get("action").is_none()
                && args.get("operations").is_some_and(|operations| {
                    !(operations.is_null()
                        || operations
                            .as_str()
                            .is_some_and(|text| text.trim().is_empty())
                        || operations.as_array().is_some_and(Vec::is_empty))
                }) =>
        {
            args["action"] = json!("apply");
        }
        // One plan operation flattened into the call, e.g.
        // {action:"complete",id,result}, means an apply with that operation.
        "task_plan"
            if args["action"].as_str().is_some_and(|action| {
                [
                    "insert", "update", "split", "move", "remove", "complete", "reopen",
                ]
                .contains(&action)
            }) && args.get("operations").is_none() =>
        {
            let object = args.as_object_mut().unwrap();
            let op = object.remove("action").unwrap();
            let mut operation = serde_json::Map::from_iter([("op".to_owned(), op)]);
            for key in ["texts", "before", "id", "text", "result", "reason"] {
                if let Some(value) = object.remove(key) {
                    operation.insert(key.into(), value);
                }
            }
            object.insert("action".into(), json!("apply"));
            object.insert("operations".into(), json!([operation]));
        }
        _ => {}
    }
    Ok(())
}

/// Supply an omitted expected_hash when the output is still exactly the
/// model's own last successful write, whose result carried that hash.
fn fill_hash_of_own_write(s: &Session, object: &mut serde_json::Map<String, Value>) {
    if !object.contains_key("expected_hash")
        && let Some((written, digest)) = &s.last_document_write
        && output_path(&s.project).is_ok_and(|path| path == *written)
        && hash_file(written).is_ok_and(|current| current == *digest)
    {
        object.insert("expected_hash".into(), json!(digest));
    }
}

/// A batch has one document hash. Models often repeat it inside each edit;
/// accept that when every copy agrees, and supply a missing top-level value
/// from them. Differing copies are a real conflict. A blank copy is a filled
/// placeholder and counts as none: hoisted, it was rejected as empty, and
/// beside a real top-level hash it read as a conflict.
fn hoist_batch_expected_hash(args: &mut Value) -> Result<()> {
    let Some(edits) = args.get_mut("edits").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    let mut nested: Option<Value> = None;
    for (index, edit) in edits.iter_mut().enumerate() {
        let Some(value) = edit
            .as_object_mut()
            .and_then(|object| object.remove("expected_hash"))
            .filter(|value| value != "")
        else {
            continue;
        };
        match &nested {
            Some(previous) if *previous != value => bail!(
                "conflicting_arguments: edits[{index}].expected_hash differs from another edit; a batch applies to one document version, so pass a single top-level expected_hash"
            ),
            _ => nested = Some(value),
        }
    }
    let Some(nested) = nested else {
        return Ok(());
    };
    match args.get("expected_hash") {
        None => args["expected_hash"] = nested,
        Some(top) if *top == nested => {}
        Some(_) => bail!(
            "conflicting_arguments: an edit's expected_hash differs from the batch expected_hash; pass only the top-level expected_hash of the current document"
        ),
    }
    Ok(())
}

fn validate_document_edit_batch_arguments(args: &Value) -> Result<()> {
    // Like document_edit, a missing hash is reported when the batch runs,
    // after its edits were checked against the current document.
    let Some(expected_hash) = args.get("expected_hash") else {
        return validate_document_edit_batch_edits(args)
            .map_err(|error| anyhow::anyhow!("{error}; expected_hash is also missing"));
    };
    let expected_hash = expected_hash
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("invalid_argument_type: expected_hash must be string"))?;
    if expected_hash.trim().is_empty() {
        bail!("invalid_argument_value: expected_hash must not be empty");
    }
    validate_document_edit_batch_edits(args)
}

fn validate_document_edit_batch_edits(args: &Value) -> Result<()> {
    let edits = args
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("invalid_argument_type: edits must be an array"))?;
    if edits.is_empty() || edits.len() > 32 {
        bail!("invalid_argument_value: edits requires 1..32 entries");
    }
    for (index, edit) in edits.iter().enumerate() {
        let object = edit.as_object().ok_or_else(|| {
            anyhow::anyhow!("invalid_argument_type: edits[{index}] must be an object with action")
        })?;
        for key in object.keys() {
            if ![
                "action",
                "text",
                "old_text",
                "section",
                "expected_section_hash",
            ]
            .contains(&key.as_str())
            {
                bail!("unknown_argument: edits[{index}].{key}");
            }
        }
        let action_value = object
            .get("action")
            .ok_or_else(|| anyhow::anyhow!("missing_argument: edits[{index}].action"))?;
        let action = action_value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("invalid_argument_type: edits[{index}].action"))?;
        if ![
            "write",
            "append",
            "insert_before",
            "insert_after",
            "insert_first_child",
            "insert_last_child",
            "patch",
            "replace_text",
            "delete_text",
            "insert_before_text",
            "insert_after_text",
            "section",
        ]
        .contains(&action)
        {
            bail!("invalid_argument_value: edits[{index}].action is not a supported document edit");
        }
        let needed = document_edit_needs(action);
        let missing: Vec<&str> = needed
            .iter()
            .copied()
            .filter(|key| !object.contains_key(*key))
            .collect();
        if let [first, rest @ ..] = missing.as_slice() {
            bail!(
                "missing_argument: edits[{index}].{first} is required for action={action}{}; action={action} needs {}",
                also_missing(rest),
                needed.join(", ")
            );
        }
        if action != "delete_text" {
            let text_value = object
                .get("text")
                .ok_or_else(|| anyhow::anyhow!("missing_argument: edits[{index}].text"))?;
            let text = text_value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("invalid_argument_type: edits[{index}].text"))?;
            reject_replacement_character(&format!("edits[{index}].text"), text)?;
        }
        match action {
            "patch" | "replace_text" | "delete_text" | "insert_before_text"
            | "insert_after_text" => {
                let old_text_value = object
                    .get("old_text")
                    .ok_or_else(|| anyhow::anyhow!("missing_argument: edits[{index}].old_text"))?;
                let old_text = old_text_value.as_str().ok_or_else(|| {
                    anyhow::anyhow!("invalid_argument_type: edits[{index}].old_text")
                })?;
                if old_text.is_empty() {
                    bail!("invalid_argument_value: edits[{index}].old_text must not be empty");
                }
                if matches!(action, "insert_before_text" | "insert_after_text")
                    && object["text"].as_str().is_some_and(str::is_empty)
                {
                    bail!("invalid_argument_value: edits[{index}].text must not be empty");
                }
                if let Some(section) = object.get("section")
                    && section.as_str().is_none_or(|value| value.trim().is_empty())
                {
                    bail!("invalid_argument_value: edits[{index}].section must not be empty");
                }
                if object.contains_key("expected_section_hash") && !object.contains_key("section") {
                    bail!(
                        "invalid_argument_value: edits[{index}].expected_section_hash of action={action} {SECTION_HASH_WITHOUT_SECTION}"
                    );
                }
                if action == "delete_text" && object.contains_key("text") {
                    bail!(
                        "unknown_argument: edits[{index}].text is not valid for action=delete_text"
                    );
                }
            }
            "section" => {
                let section_value = object
                    .get("section")
                    .ok_or_else(|| anyhow::anyhow!("missing_argument: edits[{index}].section"))?;
                let section = section_value.as_str().ok_or_else(|| {
                    anyhow::anyhow!("invalid_argument_type: edits[{index}].section")
                })?;
                if section.trim().is_empty() {
                    bail!("invalid_argument_value: edits[{index}].section must not be empty");
                }
                let section_hash_value = object.get("expected_section_hash").ok_or_else(|| {
                    anyhow::anyhow!("missing_argument: edits[{index}].expected_section_hash")
                })?;
                let section_hash = section_hash_value.as_str().ok_or_else(|| {
                    anyhow::anyhow!("invalid_argument_type: edits[{index}].expected_section_hash")
                })?;
                if section_hash.trim().is_empty() {
                    bail!(
                        "invalid_argument_value: edits[{index}].expected_section_hash must not be empty"
                    );
                }
                if object.contains_key("old_text") {
                    bail!(
                        "unknown_argument: edits[{index}].old_text is not valid for action=section"
                    );
                }
            }
            "insert_before" | "insert_after" | "insert_first_child" | "insert_last_child" => {
                let section = object
                    .get("section")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing_argument: edits[{index}].section"))?;
                if section.trim().is_empty() {
                    bail!("invalid_argument_value: edits[{index}].section must not be empty");
                }
                for key in ["old_text", "expected_section_hash"] {
                    if object.contains_key(key) {
                        bail!(
                            "unknown_argument: edits[{index}].{key} is not valid for action={action}{}",
                            document_action_hint(action, key)
                        );
                    }
                }
            }
            "write" | "append" => {
                for key in ["old_text", "section", "expected_section_hash"] {
                    if object.contains_key(key) {
                        bail!(
                            "unknown_argument: edits[{index}].{key} is not valid for action={action}"
                        );
                    }
                }
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}

/// Explain a batch operation whose old_text did not match exactly once.
/// `states[k]` is the document before operation k (states[0] = original).
fn batch_target_hint(states: &[String], applied: &[usize], edit: &Value, error: &str) -> String {
    if error.starts_with("section_revision_conflict") {
        // The section may have matched the snapshot the caller read and been
        // changed by an earlier operation of this same batch.
        let (Some(section), Some(expected)) = (
            edit["section"].as_str(),
            edit["expected_section_hash"].as_str(),
        ) else {
            return String::new();
        };
        let matched = |doc: &String| {
            documentation::resolve_heading(doc, section)
                .is_ok_and(|h| hash(&doc.as_bytes()[h.start..h.end]) == expected)
        };
        if let Some(changed_by) =
            (1..states.len()).find(|&k| matched(&states[k - 1]) && !matched(&states[k]))
        {
            return format!(
                ". The section matched when this batch started but edits[{}] in this same batch already changed it; merge the two edits into one section replacement or send them as separate requests",
                applied[changed_by - 1]
            );
        }
        return String::new();
    }
    if !error.starts_with("patch_target_must_match_once") {
        return String::new();
    }
    let Some(target) = edit["old_text"].as_str().filter(|t| !t.is_empty()) else {
        return String::new();
    };
    // Present in the original but removed by an earlier operation here. The
    // operation's own error explains every other cause.
    if let Some(changed_by) =
        (1..states.len()).find(|&k| states[k - 1].contains(target) && !states[k].contains(target))
    {
        return format!(
            ". old_text existed in the original document but edits[{}] in this same batch already changed it; operations apply in order, so copy old_text from the text after that edit, merge the two corrections, or send them as separate requests",
            applied[changed_by - 1]
        );
    }
    String::new()
}

/// What checking an edit against the current document in memory found, for
/// an error that stops it before writing (`failures`; a batch lists each
/// failing operation), so one retry fixes everything.
fn checked_clause(batch: bool, failures: &[String]) -> String {
    match (batch, failures) {
        (false, []) => {
            "the edit was checked against the current document and has no other problem".to_owned()
        }
        (false, failures) => format!(
            "the edit was also checked against the current document and failed: {}; fix it too",
            failures.join("; ")
        ),
        (true, []) => {
            "its edits were checked against the current document and have no other problem"
                .to_owned()
        }
        (true, failures) => format!(
            "its edits were also checked against the current document and {} failed: {}; operations after a failed one were checked without it, so fix every listed operation too",
            failures.len(),
            failures
                .iter()
                .map(|failure| format!("[{failure}]"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// A missing hash is found only after the edit was checked against the
/// current document in memory, so the error also names the edit's own
/// problems and one retry fixes both.
fn missing_document_hash(target: &str, batch: bool, failures: &[String]) -> anyhow::Error {
    anyhow::anyhow!(
        "document_hash_required: {target} on an existing document requires expected_hash; copy hash from the latest document_inspect or document_edit result; {}; nothing was written",
        checked_clause(batch, failures)
    )
}

/// A SHA-256 hex digest, the only form tools issue.
fn is_document_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A stale hash means the document changed; a malformed one was never issued
/// by a tool, so rereading alone would not tell the model what went wrong.
/// Like a missing hash, a malformed one comes with what checking the edit
/// against the current document found (`checked`).
fn revision_conflict(args: &Value, current: &str, checked: Option<String>) -> anyhow::Error {
    let expected = args["expected_hash"].as_str().unwrap_or("");
    let checked = checked.map_or_else(String::new, |checked| {
        format!("; {checked}; nothing was written")
    });
    if !is_document_hash(expected) {
        // A copy that lost a span of the current hash keeps failing when the
        // model reuses it; name the exact loss instead of asking for a reread.
        if let Some(dropped) = dropped_span(current, expected) {
            return anyhow::anyhow!(
                "document_revision_conflict: expected_hash is the current hash with {dropped:?} missing after its first {} characters; use the current hash exactly: {current}{checked}",
                expected
                    .bytes()
                    .zip(current.bytes())
                    .take_while(|(a, b)| a == b)
                    .count()
            );
        }
        return anyhow::anyhow!(
            "document_revision_conflict: expected_hash is not a document hash (a SHA-256 hash is 64 hex characters; got {}); copy the hash field exactly from the latest document_inspect or document_edit result instead of retyping it{checked}",
            expected.chars().count()
        );
    }
    anyhow::anyhow!(
        "document_revision_conflict: the document changed since this expected_hash; read output and retry with its current hash"
    )
}

/// The contiguous span removed from `current` to produce `copy`, when the
/// copy keeps a recognizable prefix and suffix of the current hash.
pub(crate) fn dropped_span<'a>(current: &'a str, copy: &str) -> Option<&'a str> {
    if copy.len() >= current.len() || copy.len() < 32 {
        return None;
    }
    let prefix = copy
        .bytes()
        .zip(current.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    let missing = current.len() - copy.len();
    // The overlap may make several split points valid; any of them proves
    // the copy is prefix + suffix of the current hash.
    (prefix.saturating_sub(missing)..=prefix)
        .find(|&split| {
            split >= 8
                && copy.len() - split >= 8
                && current.ends_with(&copy[split..])
                && current.starts_with(&copy[..split])
        })
        .map(|split| &current[split..split + missing])
}

/// An anchor missing from the named section but present elsewhere: say
/// where, so the model drops `section` or names the heading containing it.
fn anchor_outside_section(
    old: &str,
    section: &documentation::Heading,
    target: &str,
) -> Option<anyhow::Error> {
    let line_of = |at: usize| old[..at].matches('\n').count() + 1;
    let lines: Vec<usize> = old
        .match_indices(target)
        .map(|(at, _)| line_of(at))
        .take(4)
        .collect();
    let first = *lines.first()?;
    let containing = documentation::headings(old)
        .into_iter()
        .rfind(|heading| heading.line <= first)
        .map_or_else(
            || "the text before the first heading".to_owned(),
            |heading| format!("{:?}", heading.heading),
        );
    let last_line = line_of(section.end.saturating_sub(1).max(section.start));
    let repeated = if lines.len() > 1 {
        format!(
            "; it occurs more than once (lines {}), so also include more surrounding text",
            lines
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        String::new()
    };
    Some(anyhow::anyhow!(
        "patch_target_must_match_once: old_text is not inside section {:?} (lines {}-{last_line}), but occurs in the document at line {first} under {containing}{repeated}. Omit section to anchor in the whole document, or set section to the heading that contains old_text",
        section.heading,
        section.line
    ))
}

fn unique_document_text_span(old: &str, target: &str) -> Result<(usize, usize)> {
    if target.is_empty() {
        bail!("patch_target_must_match_once: old_text is empty");
    }
    let Some(first) = old.find(target) else {
        if let Some(entities) = html_escaped_target(old, target) {
            bail!(
                "patch_target_must_match_once: old_text contains HTML entities ({entities}) but the document has the literal characters; send old_text and text unescaped (for example => instead of =&gt;)"
            );
        }
        if let Some(passage) = near_match(old, target) {
            bail!(
                "patch_target_must_match_once: old_text is not in the current document (or the given section), but this passage differs from it only in backticks, dashes, quotes or spacing: {passage:?}. Copy that passage exactly as old_text"
            );
        }
        if let Some(passage) = anchored_passage(old, target) {
            bail!(
                "patch_target_must_match_once: old_text is not in the current document (or the given section); its start and end match this passage, whose middle differs (text dropped, added or retyped). Copy it exactly as old_text: {passage:?}"
            );
        }
        if let Some((matched, document_next, target_next)) = divergence(old, target) {
            bail!(
                "patch_target_must_match_once: old_text is not in the current document (or the given section). Its beginning matches the document up to {matched:?}; after that the document continues with {document_next:?} but old_text continues with {target_next:?}. Copy old_text from the document text at that point"
            );
        }
        bail!(
            "patch_target_must_match_once: old_text is not in the current document (or the given section); copy it exactly (including spacing and line breaks) from document_inspect or file_read of the output"
        );
    };
    // Start one Unicode character after the first match so overlapping
    // occurrences such as `aa` in `aaa` are treated as ambiguous.
    let next_start = first + old[first..].chars().next().unwrap().len_utf8();
    if old[next_start..].contains(target) {
        bail!(
            "patch_target_must_match_once: old_text occurs {} times; include more surrounding text so it matches once",
            old.matches(target).count().max(2)
        );
    }
    Ok((first, first + target.len()))
}

/// Where an old_text that begins like the document stops matching it. The
/// longest prefix of `target` found exactly once in `old` (at least 12
/// characters) marks the point; the text on either side of it is returned
/// so the caller sees which sentence it misremembered. None when the
/// prefix is short or occurs more than once.
fn divergence(old: &str, target: &str) -> Option<(String, String, String)> {
    let bounds: Vec<usize> = target
        .char_indices()
        .map(|(at, _)| at)
        .chain(std::iter::once(target.len()))
        .collect();
    // Presence of a prefix is monotonic in its length, so binary search the
    // longest one that occurs.
    let (mut low, mut high) = (0, bounds.len() - 1);
    while low < high {
        let mid = (low + high).div_ceil(2);
        if old.contains(&target[..bounds[mid]]) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let prefix = &target[..bounds[low]];
    if low < 12 || old.matches(prefix).count() != 1 {
        return None;
    }
    let at = old.find(prefix)? + prefix.len();
    let tail = |text: &str| text.chars().take(40).collect::<String>();
    let matched: String = {
        let chars: Vec<char> = prefix.chars().collect();
        chars[chars.len().saturating_sub(30)..].iter().collect()
    };
    Some((matched, tail(&old[at..]), tail(&target[prefix.len()..])))
}

/// The one passage that begins with the start of `target` and ends with its
/// end. A retyped old_text that dropped or changed text in the middle, or
/// whose opening words occur in several places, has no single divergence
/// point; its intact ends still identify the text to copy.
fn anchored_passage(old: &str, target: &str) -> Option<String> {
    const ANCHOR: usize = 6;
    // Byte offset of each character boundary of target, including its end.
    let bounds: Vec<usize> = target
        .char_indices()
        .map(|(at, _)| at)
        .chain(std::iter::once(target.len()))
        .collect();
    let count = bounds.len() - 1;
    if count < ANCHOR * 2 {
        return None;
    }
    // Presence is monotonic in length, so binary search the longest
    // occurring prefix and suffix, measured in characters.
    fn longest(old: &str, count: usize, piece: impl Fn(usize) -> String) -> usize {
        let (mut low, mut high) = (0, count);
        while low < high {
            let mid = (low + high).div_ceil(2);
            if old.contains(piece(mid).as_str()) {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        low
    }
    let head_len = longest(old, count, |n| target[..bounds[n]].to_owned());
    let tail_len = longest(old, count, |n| target[bounds[count - n]..].to_owned());
    if head_len < ANCHOR || tail_len < ANCHOR {
        return None;
    }
    let head = &target[..bounds[head_len]];
    let tail = &target[bounds[count - tail_len]..];
    let window = target.len().saturating_mul(2).saturating_add(200);
    let mut passages = std::collections::BTreeSet::new();
    for (start, _) in old.match_indices(head).take(8) {
        let mut limit = (start + window).min(old.len());
        while !old.is_char_boundary(limit) {
            limit -= 1;
        }
        let rest = &old[start..limit];
        if let Some(end) = rest
            .match_indices(tail)
            .map(|(at, _)| at + tail.len())
            .find(|end| *end >= head.len())
        {
            passages.insert(&old[start..start + end]);
        }
    }
    let mut passages = passages.into_iter();
    match (passages.next(), passages.next()) {
        (Some(passage), None) if passage != target && passage.chars().count() <= 1200 => {
            Some(passage.to_owned())
        }
        _ => None,
    }
}

/// The one passage that equals `target` once backticks are ignored, dash and
/// quote variants are unified and whitespace runs are collapsed. Citations
/// are often retyped with an en dash or a moved backtick; showing the exact
/// text lets the caller copy it instead of guessing again.
fn near_match(old: &str, target: &str) -> Option<String> {
    fn normalize(text: &str) -> (Vec<char>, Vec<(usize, usize)>) {
        let mut chars = Vec::new();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for (at, c) in text.char_indices() {
            let end = at + c.len_utf8();
            let c = match c {
                '`' => continue,
                '–' | '—' | '‒' | '−' => '-',
                '‘' | '’' => '\'',
                '“' | '”' => '"',
                c if c.is_whitespace() => ' ',
                c => c,
            };
            if c == ' ' && chars.last() == Some(&' ') {
                if let Some(span) = spans.last_mut() {
                    span.1 = end;
                }
                continue;
            }
            chars.push(c);
            spans.push((at, end));
        }
        (chars, spans)
    }
    let (doc, spans) = normalize(old);
    let (wanted, _) = normalize(target.trim());
    if wanted.len() < 8 || wanted.len() > doc.len() {
        return None;
    }
    let mut found =
        (0..=doc.len() - wanted.len()).filter(|&i| doc[i..i + wanted.len()] == wanted[..]);
    let start = found.next()?;
    if found.next().is_some() {
        return None;
    }
    let (from, _) = spans[start];
    let (_, to) = spans[start + wanted.len() - 1];
    Some(old[from..to].to_owned())
}

/// Entities in a target that only matches once they are decoded.
fn html_escaped_target(old: &str, target: &str) -> Option<String> {
    const ENTITIES: [(&str, &str); 6] = [
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&#x27;", "'"),
        ("&amp;", "&"),
    ];
    let found: Vec<&str> = ENTITIES
        .iter()
        .filter(|(entity, _)| target.contains(entity))
        .map(|(entity, _)| *entity)
        .collect();
    let decoded = ENTITIES
        .iter()
        .fold(target.to_owned(), |text, (entity, raw)| {
            text.replace(entity, raw)
        });
    (!found.is_empty() && old.contains(&decoded)).then(|| found.join(", "))
}

fn line_delimiter_at(text: &str, position: usize) -> &str {
    let newline = text[..position]
        .rfind('\n')
        .or_else(|| text[position..].find('\n').map(|next| position + next));
    if newline.is_some_and(|index| index > 0 && text.as_bytes()[index - 1] == b'\r') {
        "\r\n"
    } else {
        "\n"
    }
}

fn ends_with_blank_line(text: &str) -> bool {
    text.lines()
        .last()
        .is_some_and(|line| line.trim().is_empty())
}

/// Whether the document sets its headings off with a blank line; None when
/// no heading follows other content yet.
fn heading_spacing(text: &str) -> Option<bool> {
    let mut later = documentation::headings(text)
        .into_iter()
        .filter(|heading| heading.start > 0)
        .peekable();
    later.peek()?;
    Some(later.any(|heading| ends_with_blank_line(&text[..heading.start])))
}

/// Whether insertion text forms its own line or Markdown block rather than
/// inline words.
fn inserts_block(text: &str) -> bool {
    let first = text.trim_start();
    let ordered = first.split_once(". ").is_some_and(|(number, _)| {
        !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
    });
    text.contains('\n')
        || ordered
        || ["- ", "* ", "+ ", "#", "|", "> ", "```"]
            .iter()
            .any(|marker| first.starts_with(marker))
}

fn starts_with_heading(text: &str) -> bool {
    let line = text
        .trim_start_matches(['\r', '\n'])
        .trim_start_matches(' ');
    let level = line.bytes().take_while(|b| *b == b'#').count();
    (1..=6).contains(&level) && line[level..].starts_with([' ', '\t', '\r', '\n'])
}

/// Line breaks a block inserted at a line boundary needs on both edges. The
/// anchor side is already a boundary, but the block's far edge was joined to
/// the neighboring line: a live run inserted a section before
/// "## 1. ..." without a trailing newline and got "...(README.md:27-28).## 1. ...",
/// which destroyed that heading. A heading next to the block also follows
/// the document's blank-line heading spacing.
fn separate_inserted_block(old: &str, at: usize, new: &str) -> String {
    let (before, after) = old.split_at(at);
    let delimiter = line_delimiter_at(old, at);
    let spaced = heading_spacing(old).unwrap_or(false);
    let leading_breaks = |text: &str| -> String {
        text.chars()
            .take_while(|c| matches!(c, '\r' | '\n'))
            .collect()
    };
    let mut text = new.to_string();
    if !before.is_empty() {
        if !before.ends_with('\n') && !text.starts_with(['\r', '\n']) {
            text.insert_str(0, delimiter);
        }
        if spaced
            && starts_with_heading(&text)
            && !ends_with_blank_line(&format!("{before}{}", leading_breaks(&text)))
        {
            text.insert_str(0, delimiter);
        }
    }
    if !after.is_empty() {
        if !after.starts_with(['\r', '\n']) && !text.ends_with('\n') {
            text.push_str(delimiter);
        }
        if spaced
            && starts_with_heading(after)
            && !ends_with_blank_line(&format!("{text}{}", leading_breaks(after)))
        {
            text.push_str(delimiter);
        }
    }
    text
}

/// Structural edits place text by heading or document end, so only the
/// document hash shows the model saw the version it edits.
fn requires_document_hash(action: &str) -> bool {
    matches!(
        action,
        "append"
            | "section"
            | "insert_before"
            | "insert_after"
            | "insert_first_child"
            | "insert_last_child"
    )
}

fn is_anchored_text_edit(action: &str) -> bool {
    matches!(
        action,
        "patch" | "replace_text" | "delete_text" | "insert_before_text" | "insert_after_text"
    )
}

/// A section edit, and a text edit scoped to a section, may only apply to the
/// section version its expected_section_hash names.
fn check_section_hash(section: &str, heading: &str, sent: &str) -> Result<()> {
    if sent == hash(section.as_bytes()) {
        return Ok(());
    }
    if sent.len() != 64 || !sent.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!(
            "section_revision_conflict: expected_section_hash {sent:?} is not a section hash (64 hex characters); copy section_hash from document_inspect of {heading:?}, not an item ID or document hash"
        );
    }
    bail!(
        "section_revision_conflict: expected_section_hash is not the current hash of {heading:?}; read that section again with document_inspect and use its section_hash"
    );
}

fn apply_document_edit_operation(old: &str, args: &Value) -> Result<String> {
    let action = text(args, "action")?;
    let new = if action == "delete_text" {
        ""
    } else {
        text(args, "text")?
    };
    match action {
        "create" | "write" => Ok(new.to_string()),
        // An appended heading starts a new block. Without a line break it
        // would join the previous sentence and stop being a heading, and it
        // follows the document's heading spacing like an inserted section.
        "append" if !old.is_empty() && new.trim_start_matches(' ').starts_with('#') => {
            let delimiter = line_delimiter_at(old, old.len());
            let mut prefix = old.to_string();
            if !prefix.ends_with('\n') {
                prefix.push_str(delimiter);
            }
            if heading_spacing(old).unwrap_or(false) && !ends_with_blank_line(&prefix) {
                prefix.push_str(delimiter);
            }
            Ok(format!("{prefix}{new}"))
        }
        "append" => Ok(format!("{old}{new}")),
        "insert_before" | "insert_after" | "insert_first_child" | "insert_last_child" => {
            // Blank lines before the heading carry no content; drop them so
            // the section still starts on its first line.
            let mut new = new;
            while let Some((line, rest)) = new.split_once('\n')
                && line.trim().is_empty()
            {
                new = rest;
            }
            let anchor = documentation::resolve_heading(old, text(args, "section")?)?;
            let new_headings = documentation::headings(new);
            let child = matches!(action, "insert_first_child" | "insert_last_child");
            if child && anchor.level == 6 {
                bail!(
                    "invalid_argument_value: a level-6 heading cannot have a Markdown heading child"
                );
            }
            let required_level = anchor.level + usize::from(child);
            let hashes = "#".repeat(required_level);
            // Name the heading that breaks the rule; the generic rule alone
            // reads as wrong when the text does start at the right level.
            match new_headings.first() {
                Some(first) if first.start == 0 && first.level == required_level => {}
                Some(first) if first.start == 0 => {
                    // The heading level usually shows the intended placement;
                    // name the action that places it there.
                    let fits = if child && first.level == anchor.level {
                        format!(
                            "; a level-{} heading is a sibling of {:?}: use insert_after or insert_before to add it beside that section",
                            first.level, anchor.heading
                        )
                    } else if !child && first.level == anchor.level + 1 {
                        format!(
                            "; a level-{} heading is a child of {:?}: use insert_last_child or insert_first_child to add it under that section",
                            first.level, anchor.heading
                        )
                    } else {
                        String::new()
                    };
                    bail!(
                        "invalid_argument_value: {action} text must start with a level-{required_level} heading ({hashes} ...), but it starts with level-{} {:?}{fits}",
                        first.level,
                        first.heading
                    )
                }
                _ => bail!(
                    "invalid_argument_value: {action} text must start on its first line with a level-{required_level} heading ({hashes} ...); remove any text or blank line before it"
                ),
            }
            if let Some(extra) = new_headings
                .iter()
                .skip(1)
                .find(|heading| heading.level <= required_level)
            {
                let nest = if required_level < 6 {
                    format!(
                        ", or make nested headings level {} or deeper",
                        required_level + 1
                    )
                } else {
                    String::new()
                };
                bail!(
                    "invalid_argument_value: {action} text must contain one section starting with a level-{required_level} heading, but line {} of text starts another level-{} section {:?}; insert each section with its own operation{nest}",
                    extra.line,
                    extra.level,
                    extra.heading
                );
            }
            let position = match action {
                "insert_before" => anchor.start,
                "insert_first_child" => documentation::headings(old)
                    .into_iter()
                    .find(|heading| {
                        heading.start > anchor.start
                            && heading.start < anchor.end
                            && heading.level == required_level
                    })
                    .map_or(anchor.end, |heading| heading.start),
                _ => anchor.end,
            };
            let delimiter = line_delimiter_at(old, position);
            let mut prefix = old[..position].to_string();
            if !prefix.is_empty() && !prefix.ends_with('\n') {
                prefix.push_str(delimiter);
            }
            // Follow the document's heading spacing: when its headings are
            // set off by a blank line, keep one on both sides of the inserted
            // section instead of gluing it to the previous paragraph.
            let blank_line_end = ends_with_blank_line;
            let spaced = heading_spacing(old).unwrap_or(false);
            if spaced && !prefix.is_empty() && !blank_line_end(&prefix) {
                prefix.push_str(delimiter);
            }
            let inserted_start = prefix.len();
            let mut inserted = new.to_string();
            if !inserted.ends_with('\n') {
                inserted.push_str(delimiter);
            }
            let rest = &old[position..];
            if spaced
                && !rest.is_empty()
                && !blank_line_end(&inserted)
                && !rest
                    .lines()
                    .next()
                    .is_some_and(|line| line.trim().is_empty())
            {
                inserted.push_str(delimiter);
            }
            let candidate = format!("{}{}{}", prefix, inserted, &old[position..]);
            let path = documentation::heading_path(&candidate, inserted_start)?;
            // The only way the new heading's path repeats is that the same
            // heading already exists there. Reporting it as an ambiguous
            // section argument would point at the wrong mistake.
            if documentation::resolve_heading(&candidate, &path).is_err() {
                let heading = new_headings[0].heading.as_str();
                bail!(
                    "invalid_argument_value: {action} text starts with {heading:?}, which already exists under the same parent; inserting it would duplicate that section. To extend it, use insert_first_child or insert_last_child on it with level-{} headings, replace_text/insert_after_text inside it, or action=section to rewrite it",
                    new_headings[0].level + 1
                );
            }
            Ok(candidate)
        }
        "section" => {
            let heading = text(args, "section")?;
            let resolved = documentation::resolve_heading(old, heading)?;
            let target = &old[resolved.start..resolved.end];
            check_section_hash(
                target,
                &resolved.heading,
                args["expected_section_hash"].as_str().unwrap_or(""),
            )?;
            if new.lines().next().map(str::trim) != target.lines().next().map(str::trim) {
                bail!("invalid_argument_value: section replacement must retain its heading");
            }
            if let Some(extra) = documentation::headings(new)
                .iter()
                .skip(1)
                .find(|heading| heading.level <= resolved.level)
            {
                let guidance = if resolved.level < 6 {
                    let child_level = resolved.level + 1;
                    let hashes = "#".repeat(child_level);
                    format!(
                        "Child headings are allowed: if this is a child, change its prefix to {hashes} (level {child_level}) or deeper, up to level 6. Heading depth depends on the number of # characters, not section numbering."
                    )
                } else {
                    "A level-6 section cannot have Markdown child headings; use paragraphs or lists for details within this section.".into()
                };
                bail!(
                    "invalid_argument_value: section replacement cannot add a sibling or ancestor heading: target {:?} is level {}; line {} of text contains level-{} heading {:?}. {guidance}",
                    resolved.heading,
                    resolved.level,
                    extra.line,
                    extra.level,
                    extra.heading
                );
            }
            let mut replacement = new.to_string();
            if resolved.end < old.len() {
                // A following heading must remain on its own line, and keep
                // the blank line that set it off from this section. Use the
                // original section's delimiter only when the replacement
                // has not supplied one; otherwise preserve text verbatim.
                let delimiter = if target.ends_with("\r\n") {
                    "\r\n"
                } else {
                    "\n"
                };
                if replacement.ends_with('\r') {
                    replacement.push('\n');
                } else if !replacement.ends_with('\n') {
                    replacement.push_str(delimiter);
                }
                let blank_line_end = |text: &str| {
                    text.lines()
                        .last()
                        .is_some_and(|line| line.trim().is_empty())
                };
                if blank_line_end(target) && !blank_line_end(&replacement) {
                    replacement.push_str(delimiter);
                }
            }
            let candidate = format!(
                "{}{}{}",
                &old[..resolved.start],
                replacement,
                &old[resolved.end..]
            );
            section_text(&candidate, heading)?;
            Ok(candidate)
        }
        "patch" | "replace_text" | "delete_text" | "insert_before_text" | "insert_after_text" => {
            let target = text(args, "old_text")?;
            // A live model often sends a complete revised passage through an
            // insertion action. If the payload contains the exact old passage
            // once, interpret it as a replacement instead of duplicating the
            // passage. Ambiguous or approximate repeats are still rejected.
            let anchor = target.trim();
            let insertion = matches!(action, "insert_before_text" | "insert_after_text");
            let repeated_anchor = insertion && anchor.chars().count() >= 20 && new.contains(anchor);
            let replace_instead = repeated_anchor && new.matches(target).count() == 1;
            if repeated_anchor && !replace_instead {
                bail!(
                    "invalid_argument_value: {action} keeps old_text and adds text beside it, but text repeats old_text ambiguously, so the passage would appear twice. Use replace_text with an exact old_text and the full revised passage as text"
                );
            }
            let scoped = args
                .get("section")
                .and_then(Value::as_str)
                .map(|section| documentation::resolve_heading(old, section))
                .transpose()?;
            if let Some(heading) = &scoped
                && let Some(expected) = args["expected_section_hash"].as_str()
            {
                check_section_hash(&old[heading.start..heading.end], &heading.heading, expected)?;
            }
            let (base, scope) = scoped.as_ref().map_or((0, old), |heading| {
                (heading.start, &old[heading.start..heading.end])
            });
            // Surrounding whitespace in old_text is copying noise (for example
            // a line's indentation kept when the passage starts mid-line).
            // When only it differs, match the trimmed passage and drop the
            // same whitespace from a replacement that repeats it.
            let trimmed = target.trim();
            let (target, new) =
                if !scope.contains(target) && !trimmed.is_empty() && scope.contains(trimmed) {
                    let lead = &target[..target.len() - target.trim_start().len()];
                    let trail = &target[target.trim_end().len()..];
                    let new = if insertion {
                        new
                    } else {
                        let new = new.strip_prefix(lead).unwrap_or(new);
                        new.strip_suffix(trail).unwrap_or(new)
                    };
                    (trimmed, new)
                } else {
                    (target, new)
                };
            let (relative_start, relative_end) =
                unique_document_text_span(scope, target).map_err(|error| {
                    scoped
                        .as_ref()
                        .filter(|_| !scope.contains(target))
                        .and_then(|heading| anchor_outside_section(old, heading, target))
                        .unwrap_or(error)
                })?;
            let (start, end) = (base + relative_start, base + relative_end);
            // A line or block inserted beside an anchor that stops mid-line
            // is glued into that line ("...다릅니다.- MariaDB는 ..."), which
            // leaves a broken list. Only inline text may go mid-line.
            let mut separated = None;
            if insertion && !replace_instead && inserts_block(new) {
                let at = if action == "insert_after_text" {
                    end
                } else {
                    start
                };
                let line_boundary = at == 0
                    || at == old.len()
                    || old[..at].ends_with('\n')
                    || old[at..].starts_with('\n')
                    || old[at..].starts_with("\r\n");
                if !line_boundary {
                    let (edge, extend) = if action == "insert_after_text" {
                        ("ends", "to the end of its line")
                    } else {
                        ("starts", "back to the start of its line")
                    };
                    bail!(
                        "invalid_argument_value: {action} old_text {edge} in the middle of a line, but text is a separate line or block (it contains a line break or starts with a list, heading, table or quote marker), so it would be glued into that line. Extend old_text {extend} so the text lands on its own line, or use replace_text with the full revised line"
                    );
                }
                separated = Some(separate_inserted_block(old, at, new));
            }
            let new = separated.as_deref().unwrap_or(new);
            Ok(match action {
                _ if replace_instead => format!("{}{}{}", &old[..start], new, &old[end..]),
                "insert_before_text" => format!("{}{}{}", &old[..start], new, &old[start..]),
                "insert_after_text" => format!("{}{}{}", &old[..end], new, &old[end..]),
                _ => format!("{}{}{}", &old[..start], new, &old[end..]),
            })
        }
        _ => bail!("invalid_argument_value: unsupported document edit action"),
    }
}

/// Remove the whole-document `write` action from an edit tool schema.
fn withhold_write_action(schema: &mut Value) {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(map) => {
                if let Some(Value::Array(actions)) = map.get_mut("enum")
                    && actions.iter().any(|a| a == "write")
                    && actions.iter().any(|a| a == "section")
                {
                    actions.retain(|a| a != "write");
                }
                if let Some(Value::Array(branches)) = map.get_mut("oneOf") {
                    branches.retain(|b| b["properties"]["action"]["const"] != "write");
                }
                for child in map.values_mut() {
                    strip(child);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(schema);
}

/// After an output-limit truncation, a whole-document write is withheld.
fn check_whole_write(s: &Session, name: &str, args: &Value) -> Result<()> {
    if !s.progress_recovery.whole_write_withheld {
        return Ok(());
    }
    let writes = match name {
        "document_edit" => args["action"] == "write",
        "document_edit_batch" => args["edits"]
            .as_array()
            .is_some_and(|edits| edits.iter().any(|edit| edit["action"] == "write")),
        _ => false,
    };
    if writes {
        bail!(
            "whole_write_withheld: a previous response exceeded the output limit while the document is large; rewrite one section at a time with document_edit action=section (or replace_text for a passage). Whole-document write is available again after a smaller edit succeeds"
        );
    }
    Ok(())
}

fn persist_document_edit(
    s: &mut Session,
    path: &Path,
    old: &str,
    exists: bool,
    result: String,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    // Every persisted document must remain readable by document_inspect and
    // subsequent edits, which both use read_text's size and text checks.
    if result.len() > MAX_FILE_BYTES {
        bail!("unsupported_large_file: maximum 16MiB");
    }
    if result.as_bytes().contains(&0) {
        bail!(
            "invalid_argument_value: document edit text contains NUL (U+0000); remove NUL characters from the edit text and resend the edit, keeping expected_hash when the document already exists"
        );
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid_output_parent"))?;
    std::fs::create_dir_all(parent)?;
    let checked = output_path(&s.project)?;
    if checked != path {
        bail!("output_changed");
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(result.as_bytes())?;
    if exists {
        // Atomic replacement must retain the existing document's access mode;
        // NamedTempFile otherwise replaces it with its private default mode.
        temp.as_file()
            .set_permissions(std::fs::metadata(path)?.permissions())?;
    }
    temp.as_file().sync_all()?;
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    if path.exists() != exists
        || (exists && hash(read_text(path)?.as_bytes()) != hash(old.as_bytes()))
    {
        bail!("document_revision_conflict: changed during write");
    }
    if exists {
        temp.persist(path)?;
    } else {
        temp.persist_noclobber(path)?;
    }
    completion_review::record_document_baseline(s, path, old, exists);
    s.document_written = true;
    // A smaller edit succeeded; whole-document writes are allowed again.
    s.progress_recovery.whole_write_withheld = false;
    if s.document_review.approved_hash.is_some() {
        // A later edit starts a new review cycle. An earlier approval's zero
        // issues must not make the first new finding look like a stalled review.
        document_review::close_cycle(s);
        s.document_review.stalled_attempts = 0;
        s.document_review.best_issue_count = None;
        s.document_review.last_reviewed_section_count = 0;
        s.document_review.last_reviewed_content_lines = 0;
    }
    s.document_review.approved_hash = None;
    s.last_document_write = Some((path.to_path_buf(), hash(result.as_bytes())));
    revalidate(s)?;
    let hash = hash(result.as_bytes());
    let bytes = result.len();
    let total_lines = result.lines().count();
    // The answer workflow verifies nothing: report the write only.
    if s.task.workflow == "answer" {
        return Ok(json!({"path":path,"hash":hash,"bytes":bytes,"total_lines":total_lines}));
    }
    let citation_check = documentation::citation_check(s, path, &result)?;
    let format_check = document_format::check(&result).write_result();
    let mut saved = json!({
        "path":path,
        "hash":hash,
        "bytes":bytes,
        "total_lines":total_lines,
        "citation_check":citation_check,
        "format_check":format_check
    });
    // Each save says what a non-developer reader would not understand, while
    // the passage is still fresh to the writer.
    if s.is_document_work()
        && let Some(check) = documentation::audience_check(&s.project.audience, &result)
    {
        saved["audience_check"] = check;
    }
    if s.is_document_work()
        && let Some(check) = documentation::test_code_check(s, path, &result)
    {
        saved["test_code_check"] = check;
    }
    Ok(saved)
}

const MEMORY_MANAGE_ACTIONS: &[&str] = &["candidates", "delete", "replace"];

fn memory_manage_fields(action: &str) -> &'static [&'static str] {
    match action {
        "delete" => &["action", "ids"][..],
        "replace" => &["action", "ids", "replacement"][..],
        _ => &["action"][..],
    }
}

fn validate_memory_manage_arguments(args: &Value) -> Result<()> {
    let action = args["action"].as_str().unwrap_or("");
    validate_action_fields(
        "memory_manage",
        args,
        memory_manage_fields,
        MEMORY_MANAGE_ACTIONS,
    )?;
    if matches!(action, "delete" | "replace") && args["ids"].as_array().is_none_or(Vec::is_empty) {
        bail!("missing_argument: ids for memory_manage action={action}");
    }
    if action == "replace" && args.get("replacement").is_none() {
        bail!("missing_argument: replacement for memory_manage action=replace");
    }
    Ok(())
}

const HISTORY_ACTIONS: &[&str] = &["search", "read"];

fn history_fields(action: &str) -> &'static [&'static str] {
    match action {
        "search" => &["action", "query", "after", "limit"][..],
        "read" => &["action", "id", "offset"][..],
        _ => &["action"][..],
    }
}

fn validate_history_arguments(args: &Value) -> Result<()> {
    validate_action_fields("history", args, history_fields, HISTORY_ACTIONS)?;
    if args["action"] == "read" && args.get("id").is_none() {
        bail!("missing_argument: id for history action=read");
    }
    Ok(())
}

/// An argument outside its schema enum. Naming only the field left a live
/// run resending task_plan action="insert" three times; say what arrived,
/// what is allowed and the allowed value the call most likely meant. Returns
/// the message and that value. `siblings` is the object holding the field.
fn enum_value_error(
    name: &str,
    key: &str,
    value: &Value,
    allowed: &[Value],
    siblings: &Value,
) -> (String, Option<String>) {
    let shown = match value.as_str() {
        Some(text) => format!("{:?}", text.chars().take(80).collect::<String>()),
        None => value.to_string().chars().take(80).collect(),
    };
    let listed = allowed
        .iter()
        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
        .collect::<Vec<_>>()
        .join(", ");
    let leaf = key.rsplit('.').next().unwrap_or(key);
    let leaf = leaf.split('[').next().unwrap_or(leaf);
    let plan_ops = [
        "insert", "update", "split", "move", "remove", "complete", "reopen",
    ];
    // A plan operation sent as the action, by name or by a common synonym.
    let operation = (name == "task_plan" && key == "action")
        .then(|| value.as_str())
        .flatten()
        .and_then(|sent| {
            plan_ops
                .contains(&sent)
                .then_some(sent.to_owned())
                .or_else(|| {
                    let ops: Vec<Value> = plan_ops.iter().map(|op| json!(op)).collect();
                    suggest::value(name, "op", value, &ops, siblings).and_then(|s| s.target)
                })
        });
    let (hint, target) = match operation {
        Some(op) => (
            format!(
                "; {op:?} is an operation, not an action: send action \"apply\" with expected_revision and operations [{{\"op\":{op:?}, ...}}]"
            ),
            Some("apply".to_owned()),
        ),
        None => suggest::value(name, leaf, value, allowed, siblings)
            .map_or((String::new(), None), |s| (s.text, s.target)),
    };
    (
        format!("invalid_argument_value: {key} {shown} is not one of: {listed}{hint}"),
        target,
    )
}

type ActionFields = fn(&str) -> &'static [&'static str];

/// The arguments each action of a tool accepts and the tool's actions, for
/// tools whose fields depend on the action.
fn action_fields(name: &str) -> Option<(ActionFields, &'static [&'static str])> {
    Some(match name {
        "task_state" => (task_state_fields, TASK_STATE_ACTIONS),
        "document_edit" => (document_edit_fields, DOCUMENT_EDIT_ACTIONS),
        "memory_manage" => (memory_manage_fields, MEMORY_MANAGE_ACTIONS),
        "history" => (history_fields, HISTORY_ACTIONS),
        _ => return None,
    })
}

/// A provider that fills every field sends the fields of other actions with
/// empty values: a live run's task_state action=read with patch:{} was
/// refused. An empty value ("", [], {} or null) of a field another action
/// uses gives this action nothing, so it counts as omitted. A filled one is
/// still refused, and so is any field no action uses (a misspelled name).
fn drop_empty_unaccepted_fields(name: &str, args: &mut Value) {
    let Some((fields, actions)) = action_fields(name) else {
        return;
    };
    let Some(object) = args.as_object_mut() else {
        return;
    };
    let allowed = fields(object.get("action").and_then(Value::as_str).unwrap_or(""));
    object.retain(|key, value| {
        let key = key.as_str();
        let empty = value.is_null()
            || value.as_str().is_some_and(str::is_empty)
            || value.as_array().is_some_and(Vec::is_empty)
            || value.as_object().is_some_and(serde_json::Map::is_empty);
        allowed.contains(&key)
            || !empty
            || !actions.iter().any(|action| fields(action).contains(&key))
    });
}

fn validate_action_fields(
    name: &str,
    args: &Value,
    fields: ActionFields,
    actions: &[&str],
) -> Result<()> {
    validate_action_fields_with(name, args, fields, actions, |_, _| String::new())
}

/// `fields(action)` lists the arguments an action accepts. A rejected
/// argument names the other `actions` that accept it (history action=search
/// with id: id is used by action=read). `hint(action, key)` may add the next
/// step instead.
fn validate_action_fields_with(
    name: &str,
    args: &Value,
    fields: ActionFields,
    actions: &[&str],
    hint: impl Fn(&str, &str) -> String,
) -> Result<()> {
    let object = args
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("invalid_tool_arguments: arguments must be an object"))?;
    let action = args["action"].as_str().unwrap_or("");
    let allowed = fields(action);
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        let mut next = hint(action, key);
        let owners: Vec<_> = actions
            .iter()
            .filter(|other| **other != action && fields(other).contains(&key.as_str()))
            .map(|other| format!("action={other}"))
            .collect();
        if next.is_empty() && !owners.is_empty() {
            next = format!(
                "; {key} is used by {}: drop {key}, or send that action if it is what you meant",
                owners.join(", ")
            );
        }
        bail!(
            "invalid_action_arguments: {name} action={action} does not accept {key}; allowed: {}{next}",
            allowed.join(", ")
        );
    }
    Ok(())
}

/// Structural inserts anchor on the heading named by `section`; live runs
/// sent them old_text meaning the passage-anchored inserts, and the bare
/// "allowed:" list did not name those actions.
fn document_action_hint(action: &str, key: &str) -> String {
    match (action, key) {
        ("insert_before" | "insert_after", "old_text") => format!(
            "; {action} inserts a new section beside the heading named by section, so drop old_text and name that heading in section. To insert next to an exact passage instead, use action={action}_text with old_text"
        ),
        ("insert_first_child" | "insert_last_child", "old_text") => format!(
            "; {action} inserts a child section under the heading named by section, so drop old_text. To insert next to an exact passage instead, use action=insert_before_text or insert_after_text with old_text"
        ),
        _ => String::new(),
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
fn path_glob(args: &Value) -> Result<Option<&str>> {
    if args["path_glob"].is_string()
        && args["pattern"].is_string()
        && args["path_glob"] != args["pattern"]
    {
        bail!("conflicting_path_filters: use path_glob only; pattern is a legacy alias");
    }
    let value = args["path_glob"]
        .as_str()
        .or_else(|| args["pattern"].as_str());
    if value.is_some_and(|v| {
        v.starts_with('^') || v.ends_with('$') || v.contains("(?") || v.contains("\\s")
    }) {
        bail!(
            "invalid_path_glob: expected a file glob such as backend/**/*.js, not a content regex; use source_search query with regex=true for content"
        );
    }
    if let Some(value) = value {
        globset::Glob::new(value).map_err(|error| anyhow::anyhow!("invalid_path_glob: {error}"))?;
    }
    Ok(value)
}
/// path is literal (a directory may be named `[slug]`), so it is never
/// joined with path_glob silently. Name the one glob that means both: a live
/// run resent `path` + `path_glob:"*.css"` five times without it.
fn path_glob_conflict_hint(s: &Session, args: &Value) -> String {
    let glob = args["path_glob"]
        .as_str()
        .or_else(|| args["pattern"].as_str())
        .unwrap_or("")
        .trim_start_matches("./");
    let (Some(path), Ok(root)) = (args["path"].as_str(), s.project.root.canonicalize()) else {
        return String::new();
    };
    let Ok(resolved) = read_path(&s.project, path) else {
        return String::new();
    };
    if resolved.is_file() {
        return format!("; path already names one file, so drop path_glob {glob:?}");
    }
    let Some(relative) = resolved
        .strip_prefix(&root)
        .ok()
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
    else {
        return String::new();
    };
    // A bare file pattern matches at any depth below the directory, as path
    // alone lists everything below it.
    let below = if glob.contains('/') {
        glob.to_owned()
    } else {
        format!("**/{glob}")
    };
    let combined = if relative.is_empty() {
        below
    } else {
        format!("{}/{below}", globset::escape(&relative))
    };
    format!(
        "; to match {glob:?} below {path}, send only path_glob:{combined:?} (relative to project.root; ** includes subdirectories)"
    )
}

fn n(args: &Value, key: &str, default: usize) -> usize {
    args[key].as_u64().map_or(default, |n| n as usize)
}

// Some compatible tool parsers quote unsigned integer arguments. Normalize
// them once at the execution boundary and again anywhere a raw call is used to
// rebuild a continuation, so both paths use the same typed arguments.
fn normalize_integer_arguments(name: &str, args: &mut Value) {
    fn blank_placeholder(key: &str, value: &Value) -> bool {
        value.as_str() == Some("")
            && matches!(
                key,
                "id" | "title"
                    | "path"
                    | "path_glob"
                    | "pattern"
                    | "cursor"
                    | "query"
                    | "section"
                    | "old_text"
                    | "expected_hash"
                    | "expected_section_hash"
                    | "verification_note"
                    | "reason"
            )
    }
    fn required_document_field(action: &str, key: &str) -> bool {
        match key {
            "old_text" => matches!(
                action,
                "patch"
                    | "replace_text"
                    | "delete_text"
                    | "insert_before_text"
                    | "insert_after_text"
            ),
            "section" => matches!(
                action,
                "insert_before"
                    | "insert_after"
                    | "insert_first_child"
                    | "insert_last_child"
                    | "section"
            ),
            "expected_section_hash" => action == "section",
            // A blank expected_hash is a placeholder, read as omitted:
            // anchored text edits need none, and the others report a
            // missing hash with their dry-run result or take the model's own
            // last-write hash. "must not be empty" helped no provider.
            _ => false,
        }
    }
    if let Some(spec) = ToolRegistry::specs().into_iter().find(|t| t.name == name)
        && let Some(fields) = args.as_object_mut()
    {
        // For a read-only tool, explicit null means "not given". Some
        // providers also fill every optional path/filter/cursor string with
        // "". Keep required arguments and write-tool values unchanged.
        if spec.read_only {
            let required = spec.parameters["required"].as_array();
            fields.retain(|key, value| {
                let required =
                    required.is_some_and(|required| required.iter().any(|r| r == key.as_str()));
                // An empty query is an exact empty-name filter for the
                // symbol tools, so only their navigation tokens are dropped.
                // A live run sent code_outline cursor:"" and document_inspect
                // section:"" and repeated each rejected call; another sent
                // symbol_search path_glob with pattern:"" (its legacy alias),
                // which was rejected as two conflicting filters.
                let blank_navigation_option = value.as_str() == Some("")
                    && match name {
                        "file_list" | "source_search" | "file_read" => matches!(
                            key.as_str(),
                            "path" | "path_glob" | "pattern" | "cursor" | "query"
                        ),
                        "document_inspect" => matches!(
                            key.as_str(),
                            "path" | "path_glob" | "pattern" | "cursor" | "query" | "section"
                        ),
                        "code_outline" | "symbol_search" | "symbol_relations" | "symbol_read" => {
                            matches!(key.as_str(), "path" | "path_glob" | "pattern" | "cursor")
                        }
                        _ => false,
                    };
                !spec.parameters["properties"]
                    .as_object()
                    .unwrap()
                    .contains_key(key)
                    || required
                    || !(value.is_null() || blank_navigation_option)
            });
        }
        // Function-call providers can fill optional fields with empty strings
        // even for an action that forbids those fields (notably create). None
        // of these identifiers, paths or revision tokens has an empty-string
        // meaning. Keep text/content values, where empty can mean deletion.
        let action = fields
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        fields.retain(|key, value| {
            !matches!(name, "document_edit" | "document_edit_batch")
                || !spec.parameters["properties"]
                    .as_object()
                    .unwrap()
                    .contains_key(key)
                || !blank_placeholder(key, value)
                || (name == "document_edit" && required_document_field(&action, key))
        });
        if name == "document_edit_batch"
            && let Some(edits) = fields.get_mut("edits").and_then(Value::as_array_mut)
        {
            for edit in edits {
                if let Some(fields) = edit.as_object_mut() {
                    let action = fields
                        .get("action")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    fields.retain(|key, value| {
                        !spec.parameters["properties"]["edits"]["items"]["properties"]
                            .as_object()
                            .unwrap()
                            .contains_key(key)
                            || !blank_placeholder(key, value)
                            || required_document_field(&action, key)
                    });
                }
            }
        }
        for (key, value) in &mut *fields {
            let kind = &spec.parameters["properties"][key]["type"];
            if *kind == "integer"
                && let Some(raw) = value.as_str()
                && !raw.is_empty()
                && raw.bytes().all(|b| b.is_ascii_digit())
                && let Ok(number) = raw.parse::<u64>()
            {
                *value = json!(number);
            }
            // Models sometimes send an array or object argument as its JSON
            // text, e.g. edits:"[{...}]". Decode it only when it is that type.
            // task_plan decodes its operations itself and reports doing so.
            if name != "task_plan"
                && (*kind == "array" || *kind == "object")
                && let Some(raw) = value.as_str()
                && let Ok(decoded) = serde_json::from_str::<Value>(raw.trim())
                && (decoded.is_array() && *kind == "array"
                    || decoded.is_object() && *kind == "object")
            {
                *value = decoded;
            }
        }
        // memory_manage carries the memory_write payload under replacement.
        // Keep its optimistic-lock field compatible with providers that quote
        // unsigned integers, just like the top-level memory_write argument.
        if name == "memory_manage"
            && let Some(value) = fields
                .get_mut("replacement")
                .and_then(Value::as_object_mut)
                .and_then(|replacement| replacement.get_mut("expected_revision"))
            && let Some(raw) = value.as_str()
            && !raw.is_empty()
            && raw.bytes().all(|b| b.is_ascii_digit())
            && let Ok(number) = raw.parse::<u64>()
        {
            *value = json!(number);
        }
    }
}

pub fn envelope(result: Result<Value>) -> Value {
    match result {
        Ok(data) => {
            // Batch tools retain successful items but must not hide failed ones
            // behind an outer success envelope.
            let failed = data["results"].as_array().map_or(0, |items| {
                items
                    .iter()
                    .filter(|item| recovery::is_failure(recovery::batch_result(item)))
                    .count()
            });
            if failed > 0 {
                let succeeded = data["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|item| recovery::batch_result(item)["status"] == "ok")
                    .count();
                let message = format!(
                    "batch_partial_failure: {failed} items failed, {succeeded} succeeded; inspect each failure before retrying; successful items are retained"
                );
                json!({"status":"error","error":message,"recovery":recovery::describe(&message),"partial_success":succeeded > 0,"data":data,"truncated":false,"next_cursor":null})
            } else {
                json!({"status":"ok","data":data,"truncated":false,"next_cursor":null})
            }
        }
        Err(error) => {
            let message = format!("{error:#}");
            let status = if message == "cancelled" || message.starts_with("cancelled:") {
                "cancelled"
            } else if message.starts_with("unsupported") {
                "unsupported"
            } else {
                "error"
            };
            let mut result = json!({"status":status,"error":message,"recovery":recovery::describe(&message),"truncated":false,"next_cursor":null});
            if let Some(diagnostic) = error.downcast_ref::<recovery::DiagnosticError>() {
                result["data"] = diagnostic.data.clone();
            }
            result
        }
    }
}

fn excluded(p: &Project, rel: &Path) -> Result<bool> {
    file_edit::excluded_case_alias(p, rel)
}
pub(crate) fn utf8_path(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "unsupported_non_utf8_path: path must contain valid UTF-8: {}",
            path.display()
        )
    })
}

pub fn output_path(p: &Project) -> Result<PathBuf> {
    let root = p.root.canonicalize()?;
    utf8_path(&root)?;
    utf8_path(&p.output)?;
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
    if path.exists() && std::fs::metadata(&path)?.is_dir() {
        bail!("output_path_is_directory: configured output must be a file");
    }
    Ok(path)
}
pub fn read_path(p: &Project, path: &str) -> Result<PathBuf> {
    let root = p.root.canonicalize()?;
    let mut candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        root.join(path)
    };
    // A bare file name that no project file has but the configured output
    // does names the output (live runs sent "generated.md" for an output
    // outside the project root).
    if !candidate.exists()
        && Path::new(path).components().count() == 1
        && let Ok(output) = output_path(p)
        && output.file_name() == Some(std::ffi::OsStr::new(path))
    {
        candidate = output;
    }
    let canonical = candidate.canonicalize().map_err(|e| {
        let code = match e.kind() {
            std::io::ErrorKind::NotFound => "file_not_found",
            std::io::ErrorKind::PermissionDenied => "file_permission_denied",
            _ => "file_access_error",
        };
        // Reading the output before its first write: say so, rather than
        // suggesting a different project file (a live run asked three times).
        let is_output = output_path(p).is_ok_and(|output| output == candidate);
        let hint = if e.kind() == std::io::ErrorKind::NotFound && is_output {
            " The configured output does not exist yet: nothing has been written to it. Create it with document_edit action=create (or document_edit_batch) before reading or auditing it.".to_owned()
        } else if e.kind() == std::io::ErrorKind::NotFound {
            if let Some(shape) = path_shape_hint(&root, path) {
                shape
            } else {
                match similar_paths(p, &candidate).as_slice() {
                    [] => " No project file or directory has this name; list files with file_list mode=paths and path_glob before reading.".to_owned(),
                    similar => format!(" Existing project paths with a similar name: {}. Copy one exactly.", similar.join(", ")),
                }
            }
        } else {
            String::new()
        };
        anyhow::anyhow!("{code}: resolved path {}; project root {}; configured output {}. Relative paths use project.root; use document_inspect with no path for configured output.{hint} {e}", candidate.display(), root.display(), p.output.display())
    })?;
    utf8_path(&canonical)?;
    let output = output_path(p)?;
    if output.exists() && canonical == output.canonicalize()? {
        return Ok(canonical);
    }
    if !canonical.starts_with(&root) {
        bail!(
            "path_outside_project: {} resolves to {} (symlinks are followed), outside project root {}; read files inside the project, or the configured output via document_inspect",
            candidate.display(),
            canonical.display(),
            root.display()
        );
    }
    // Include patterns select files. A directory scope need not itself match
    // `**/*.rs`; its children are still filtered when enumerated. Exclusion
    // rules continue to apply to the directory itself.
    let directory_rules;
    let rules = if canonical.is_dir() {
        directory_rules = Project {
            include: vec![],
            ..p.clone()
        };
        &directory_rules
    } else {
        p
    };
    if excluded(rules, canonical.strip_prefix(&root)?)? {
        bail!(
            "path_excluded: {} is excluded by the project's include/exclude settings and cannot be read; choose a listed file from file_list",
            canonical.strip_prefix(&root)?.display()
        );
    }
    Ok(canonical)
}
/// A missing path that is a citation or a glob rather than a file name.
/// Documents cite `path:line-line`, and models copy that (or `#L12-L20`)
/// into path; a glob in path is never expanded.
fn path_shape_hint(root: &Path, path: &str) -> Option<String> {
    let lines = |range: &str| -> Option<(u64, u64)> {
        let range = range.replace(['L', 'l'], "");
        let (start, end) = range.split_once('-').unwrap_or((&range, &range));
        let (start, end) = (start.trim().parse().ok()?, end.trim().parse().ok()?);
        (start >= 1 && end >= start).then_some((start, end))
    };
    let exists = |base: &str| {
        if Path::new(base).is_absolute() {
            Path::new(base).exists()
        } else {
            root.join(base).exists()
        }
    };
    let reference = path
        .rsplit_once("#L")
        .or_else(|| path.rsplit_once('#'))
        .and_then(|(base, range)| Some((base, lines(range)?)))
        .or_else(|| {
            // path:12, path:12-20, or path:12:5 (line:column), where the
            // earlier number is the line: prefer the split whose file exists.
            let last = path
                .rsplit_once(':')
                .and_then(|(base, range)| Some((base, lines(range)?)));
            let earlier = last.and_then(|(base, _)| {
                let (base, range) = base.rsplit_once(':')?;
                Some((base, lines(range)?))
            });
            match (last, earlier) {
                (Some(last), _) if exists(last.0) => Some(last),
                (_, Some(earlier)) => Some(earlier),
                (last, None) => last,
            }
            .filter(|(base, _)| !base.is_empty())
        });
    if let Some((base, (start, end))) = reference {
        let exists = exists(base);
        let base_note = if exists {
            format!(" {base:?} exists.")
        } else {
            String::new()
        };
        return Some(format!(
            " path contains a line reference {:?}; send only the file path {base:?} in path and the lines separately (file_read: start_line={start}, max_lines={}).{base_note}",
            &path[base.len()..],
            end - start + 1
        ));
    }
    path.contains(['*', '?', '[', '{']).then(|| format!(
        " path takes one exact file or directory and does not expand globs; to match files by pattern send path_glob:{path:?} instead (file_list, source_search and symbol_search accept path_glob)."
    ))
}

/// Project files whose name matches a missing path: the same file name
/// first, then the same stem with another extension (agent.py -> agent.js),
/// then directories of that name. Ignore and exclude rules apply, so hidden
/// files are never suggested.
fn similar_paths(p: &Project, missing: &Path) -> Vec<String> {
    const MAX_SUGGESTIONS: usize = 5;
    const MAX_SCANNED: usize = 20_000;
    let Some(name) = missing
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
    else {
        return vec![];
    };
    let stem = missing
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let Ok(root) = p.root.canonicalize() else {
        return vec![];
    };
    let Ok(paths) = candidate_paths(p, None, &tokio_util::sync::CancellationToken::new()) else {
        return vec![];
    };
    let mut exact = vec![];
    let mut same_stem = vec![];
    let mut directories = std::collections::BTreeSet::new();
    for path in paths.iter().take(MAX_SCANNED) {
        let Ok(relative) = path.strip_prefix(&root) else {
            continue;
        };
        // A missing directory (file_list or source_search path) matches a
        // directory of the same name elsewhere in the project.
        if let Some(parent) = relative.parent() {
            for ancestor in parent.ancestors() {
                if ancestor
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().to_lowercase() == name)
                {
                    directories.insert(format!(
                        "{}/",
                        ancestor.to_string_lossy().replace('\\', "/")
                    ));
                }
            }
        }
        let relative = relative.to_string_lossy().replace('\\', "/");
        let file = path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if file == name {
            exact.push(relative);
        } else if !stem.is_empty()
            && path
                .file_stem()
                .is_some_and(|s| s.to_string_lossy().to_lowercase() == stem)
        {
            same_stem.push(relative);
        }
    }
    exact.sort_by_key(|path| (path.len(), path.clone()));
    same_stem.sort_by_key(|path| (path.len(), path.clone()));
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by_key(|path| (path.len(), path.clone()));
    exact
        .into_iter()
        .chain(same_stem)
        .chain(directories)
        .take(MAX_SUGGESTIONS)
        .collect()
}
/// A file operation named a directory. File helpers do not know the
/// project, so the tool layer adds the directory's entries under the
/// project's ignore/include/exclude rules (see `directory_error`).
#[derive(Debug)]
pub(crate) struct DirectoryPath(PathBuf);

impl std::fmt::Display for DirectoryPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "path_is_directory: {} is a directory; use file_list with path_glob (e.g. backend/**), then file_read with a file path",
            self.0.display()
        )
    }
}

impl std::error::Error for DirectoryPath {}

/// Entries of a directory a tool could read, as immediate children (`name`
/// or `name/`). Built from the same scan as file_list, so ignored and
/// excluded files are never named: a live run saw an excluded credentials
/// file listed in this error.
fn directory_entries(p: &Project, directory: &Path) -> (Vec<String>, usize) {
    const MAX_ENTRIES: usize = 12;
    let Ok(paths) = candidate_paths_bounded(
        p,
        None,
        &tokio_util::sync::CancellationToken::new(),
        Some(directory),
        None,
        FileScanLimits {
            max_paths: 20_000,
            max_path_bytes: 4 * 1024 * 1024,
        },
    ) else {
        return (vec![], 0);
    };
    let mut entries = std::collections::BTreeSet::new();
    for path in &paths {
        let Ok(relative) = path.strip_prefix(directory) else {
            continue;
        };
        let mut components = relative.components();
        let Some(first) = components.next() else {
            continue;
        };
        let name = first.as_os_str().to_string_lossy();
        entries.insert(if components.next().is_some() {
            format!("{name}/")
        } else {
            name.into_owned()
        });
    }
    let total = entries.len();
    (entries.into_iter().take(MAX_ENTRIES).collect(), total)
}

fn directory_error(p: &Project, directory: &DirectoryPath, tool: &str) -> anyhow::Error {
    let (entries, total) = directory_entries(p, &directory.0);
    let listing = match total.saturating_sub(entries.len()) {
        _ if entries.is_empty() => String::new(),
        0 => format!(" containing {}", entries.join(", ")),
        more => format!(" containing {} and {more} more", entries.join(", ")),
    };
    // The next call is the same tool on one file: a live model sent
    // code_outline the project root twice and was told to use file_read.
    let next = match tool {
        // The output needs no path: a live model sent document_inspect the
        // project root and was told to find a file with file_list.
        "document_inspect" => format!(
            "document_inspect without path inspects the configured output {}; pass path only for another Markdown file",
            p.output.display()
        ),
        "code_outline" | "symbol_read" | "symbol_relations" | "file_read" => {
            format!(
                "{tool} reads one file: find it with file_list and path_glob (e.g. backend/**), then call {tool} with that file path"
            )
        }
        _ => "use file_list with path_glob (e.g. backend/**), then file_read with a file path"
            .to_owned(),
    };
    anyhow::anyhow!(
        "path_is_directory: {} is a directory{listing}; {next}",
        directory.0.display()
    )
}

fn regular_metadata(metadata: &std::fs::Metadata, path: &Path) -> Result<()> {
    if metadata.is_dir() {
        return Err(DirectoryPath(path.to_path_buf()).into());
    }
    if !metadata.is_file() {
        bail!(
            "unsupported_file_type: {} is not a regular file (device, socket or pipe); choose a regular text file",
            path.display()
        );
    }
    Ok(())
}
fn file_access_error(path: &Path, error: std::io::Error) -> anyhow::Error {
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => "file_not_found",
        std::io::ErrorKind::PermissionDenied => "file_permission_denied",
        _ => "file_access_error",
    };
    anyhow::anyhow!("{code}: {}: {error}", path.display())
}
pub(crate) fn open_regular_file(path: &Path) -> Result<std::fs::File> {
    regular_metadata(
        &path.metadata().map_err(|e| file_access_error(path, e))?,
        path,
    )?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A path can become a FIFO between metadata and open. Opening it must
        // never block waiting for a writer. Regular files ignore O_NONBLOCK.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|e| file_access_error(path, e))?;
    regular_metadata(
        &file.metadata().map_err(|e| file_access_error(path, e))?,
        path,
    )?;
    Ok(file)
}

const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;

fn read_bytes_bounded(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;

    let file = open_regular_file(path)?;
    let length = file.metadata()?.len();
    if length > MAX_FILE_BYTES as u64 {
        bail!("unsupported_large_file: maximum 16MiB");
    }
    // Bound the actual read too: the file may grow after the metadata check.
    // Reserve the expected length and one EOF probe byte. Starting empty or
    // filling the capacity exactly makes read_to_end grow to the next power
    // of two even for an unchanged file.
    let mut bytes = Vec::with_capacity(length as usize + 1);
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FILE_BYTES {
        bail!("unsupported_large_file: maximum 16MiB");
    }
    Ok(bytes)
}

pub(crate) fn hash_file(path: &Path) -> Result<String> {
    hash_file_cancelled(path, &tokio_util::sync::CancellationToken::new())
}

pub(crate) fn hash_file_cancelled(
    path: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String> {
    let file = open_regular_file(path)?;
    if file.metadata()?.len() > MAX_FILE_BYTES as u64 {
        bail!("unsupported_large_file: maximum 16MiB");
    }
    hash_reader_cancelled(file, cancel)
}

#[cfg(test)]
fn hash_reader(reader: impl std::io::Read) -> Result<String> {
    hash_reader_cancelled(reader, &tokio_util::sync::CancellationToken::new())
}

fn hash_reader_cancelled(
    reader: impl std::io::Read,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String> {
    use std::io::Read;
    // Freshness checks need only a digest. Never allocate the full file for
    // each check, and enforce the same bound if it grows after metadata lookup.
    let mut reader = reader.take((MAX_FILE_BYTES + 1) as u64);
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut bytes = 0;
    loop {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes += read;
        if bytes > MAX_FILE_BYTES {
            bail!("unsupported_large_file: maximum 16MiB");
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub(crate) fn read_text(path: &Path) -> Result<String> {
    let bytes = read_bytes_bounded(path)?;
    if bytes.contains(&0) {
        bail!(
            "unsupported_binary_file: {} contains NUL bytes and is not a text file; choose a text file",
            path.display()
        );
    }
    String::from_utf8(bytes).map_err(|_| {
        anyhow::anyhow!(
            "unsupported_non_utf8_file: {} is not valid UTF-8 text; choose a UTF-8 text file",
            path.display()
        )
    })
}
pub(crate) fn read_text_preview(path: &Path, max_bytes: usize) -> Result<(String, bool)> {
    use std::io::Read;
    let limit = max_bytes
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("invalid_preview_limit"))?;
    let mut bytes = Vec::new();
    open_regular_file(path)?
        .take(limit as u64)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > max_bytes;
    bytes.truncate(max_bytes);
    if let Err(error) = std::str::from_utf8(&bytes) {
        if truncated && error.error_len().is_none() {
            // Only an incomplete character at the preview boundary is omitted.
            bytes.truncate(error.valid_up_to());
        } else {
            bail!("unsupported_non_utf8_file");
        }
    }
    Ok((String::from_utf8(bytes)?, truncated))
}

fn candidate_paths(
    p: &Project,
    pattern: Option<&str>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<PathBuf>> {
    candidate_paths_scoped(p, pattern, cancel, None, None)
}

fn candidate_paths_scoped(
    p: &Project,
    pattern: Option<&str>,
    cancel: &tokio_util::sync::CancellationToken,
    directory: Option<&Path>,
    deadline: Option<std::time::Instant>,
) -> Result<Vec<PathBuf>> {
    candidate_paths_bounded(
        p,
        pattern,
        cancel,
        directory,
        deadline,
        FileScanLimits {
            max_paths: 100_000,
            max_path_bytes: 16 * 1024 * 1024,
        },
    )
}

struct FileScanLimits {
    max_paths: usize,
    max_path_bytes: usize,
}

/// Directories no project walk enters: version control, dependencies and
/// build output.
const SKIPPED_DIRECTORIES: [&str; 10] = [
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

fn candidate_paths_bounded(
    p: &Project,
    pattern: Option<&str>,
    cancel: &tokio_util::sync::CancellationToken,
    directory: Option<&Path>,
    deadline: Option<std::time::Instant>,
    limits: FileScanLimits,
) -> Result<Vec<PathBuf>> {
    let check = || -> Result<()> {
        if let Some(deadline) = deadline {
            structure::check_budget(cancel, deadline)
        } else if cancel.is_cancelled() {
            bail!("cancelled")
        } else {
            Ok(())
        }
    };
    check()?;
    let root = p.root.canonicalize()?;
    if let Some(scope) = directory.filter(|scope| !scope.starts_with(&root)) {
        bail!(
            "path_outside_project: {} is outside project root {}; list a directory inside the project",
            scope.display(),
            root.display()
        );
    }
    let scope = directory.map(Path::to_path_buf);
    let mut entries = vec![];
    let mut path_bytes = 0usize;
    let filter = pattern
        .map(globset::Glob::new)
        .transpose()?
        .map(|g| g.compile_matcher());
    for entry in ignore::WalkBuilder::new(&root)
        .hidden(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            !entry.file_type().is_some_and(|t| t.is_dir())
                || (!SKIPPED_DIRECTORIES.contains(&entry.file_name().to_string_lossy().as_ref())
                    && scope.as_ref().is_none_or(|scope| {
                        entry.path().starts_with(scope) || scope.starts_with(entry.path())
                    }))
        })
        .build()
    {
        check()?;
        let entry = entry?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        // JSON tool paths cannot address OS names that are not UTF-8. A lossy
        // path can name a different file, and serializing the real path panics.
        if entry.path().to_str().is_none() {
            continue;
        }
        let rel = entry.path().strip_prefix(&root)?;
        if directory.is_some_and(|scope| !entry.path().starts_with(scope))
            || excluded(p, rel)?
            || filter.as_ref().is_some_and(|f| !f.is_match(rel))
        {
            continue;
        }
        path_bytes = path_bytes.saturating_add(entry.path().as_os_str().as_encoded_bytes().len());
        // Page limits are applied after sorting/fingerprinting. Bound discovery
        // itself so a broad call cannot retain an arbitrary number of paths.
        if entries.len() >= limits.max_paths || path_bytes > limits.max_path_bytes {
            bail!(
                "file_scan_capacity: matched paths exceed {} entries or {} bytes; narrow path, path_glob or project include/exclude rules",
                limits.max_paths,
                limits.max_path_bytes
            );
        }
        entries.push(entry.path().to_path_buf());
    }
    entries.sort();
    check()?;
    Ok(entries)
}
// Keep the existing text-only listing contract. Searches use candidates directly
// so text validation and matching share one read instead of opening every file twice.
fn paths(
    p: &Project,
    pattern: Option<&str>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for path in candidate_paths(p, pattern, cancel)? {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        if search_text(&path)?.is_some() {
            result.push(path);
        }
    }
    Ok(result)
}
fn search_text(path: &Path) -> Result<Option<String>> {
    match read_text(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.to_string().starts_with("unsupported_") => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy)]
struct EvidenceQuality {
    line_start_complete: bool,
    line_end_complete: bool,
    evidence_truncated: bool,
}
fn observe_hashed_quality(
    s: &mut Session,
    path: &Path,
    content_hash: String,
    start: usize,
    end: usize,
    excerpt: &str,
    quality: EvidenceQuality,
) -> Source {
    if let Some(source) = s.sources.values().find(|source| {
        source.path.as_deref() == path.to_str()
            && source.hash.as_deref() == Some(&content_hash)
            && source.start_line == Some(start)
            && source.end_line == Some(end)
            && source.line_start_complete == quality.line_start_complete
            && source.line_end_complete == quality.line_end_complete
            && source.evidence_truncated == quality.evidence_truncated
            && source.excerpt == excerpt.chars().take(2000).collect::<String>()
    }) {
        return source.clone();
    }
    let source = Source {
        id: crate::memory::source_id(),
        observed_at: chrono::Utc::now(),
        origin: "file".into(),
        path: Some(path.display().to_string()),
        start_line: Some(start),
        end_line: Some(end),
        line_start_complete: quality.line_start_complete,
        line_end_complete: quality.line_end_complete,
        evidence_truncated: quality.evidence_truncated,
        hash: Some(content_hash),
        excerpt: excerpt.chars().take(2000).collect(),
    };
    s.memory
        .stale_path(source.path.as_deref().unwrap(), source.hash.as_deref());
    s.sources.insert(source.id.clone(), source.clone());
    source
}
/// Observations of the same content view, regardless of when they were made.
pub(crate) fn same_source(a: &crate::memory::Source, b: &crate::memory::Source) -> bool {
    a.origin == b.origin
        && a.path == b.path
        && a.start_line == b.start_line
        && a.end_line == b.end_line
        && a.line_start_complete == b.line_start_complete
        && a.line_end_complete == b.line_end_complete
        && a.evidence_truncated == b.evidence_truncated
        && a.hash == b.hash
        && a.excerpt == b.excerpt
}

pub(crate) struct Revalidation {
    hashes: BTreeMap<String, Option<String>>,
    stale_memories: BTreeSet<String>,
}

impl Revalidation {
    pub(crate) fn changed(&self) -> bool {
        !self.stale_memories.is_empty()
    }

    pub(crate) fn apply(&self, s: &mut Session) {
        for (path, hash) in &self.hashes {
            s.memory.stale_path(path, hash.as_deref());
        }
    }
}

/// Inspect immutable display snapshots on a file worker. Apply the result only
/// to that same snapshot, so a slow check cannot overwrite newer session state.
pub(crate) fn inspect_freshness(
    s: &Session,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Revalidation> {
    inspect_freshness_with(s, cancel, |path| hash_file_cancelled(path, cancel))
}

pub(crate) fn inspect_freshness_with(
    s: &Session,
    cancel: &tokio_util::sync::CancellationToken,
    mut file_hash: impl FnMut(&Path) -> Result<String>,
) -> Result<Revalidation> {
    let paths: BTreeSet<&str> = s
        .memory
        .entries
        .values()
        .flat_map(|m| &m.sources)
        .filter_map(|source| source.path.as_deref())
        .collect();
    let mut hashes = BTreeMap::new();
    for path in paths {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        let current = read_path(&s.project, path).and_then(|p| file_hash(&p)).ok();
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        hashes.insert(path.to_owned(), current);
    }
    let source_changed = |source: &Source| {
        source.path.as_ref().is_some_and(|path| {
            hashes
                .get(path)
                .is_none_or(|hash| hash.as_deref() != source.hash.as_deref())
        })
    };
    let stale_memories: BTreeSet<_> = s
        .memory
        .entries
        .values()
        .filter(|memory| memory.status == crate::memory::MemoryStatus::Active)
        .filter(|memory| memory.sources.iter().any(source_changed))
        .map(|memory| memory.id.clone())
        .collect();
    Ok(Revalidation {
        hashes,
        stale_memories,
    })
}

pub fn revalidate(s: &mut Session) -> Result<()> {
    inspect_freshness(s, &tokio_util::sync::CancellationToken::new())?.apply(s);
    Ok(())
}
/// Session sources that cover part of a missing cited range: complete lines
/// of the current file version, not already supplied. Truncated excerpts and
/// stale versions never attest a citation.
fn delivered_sources_for(
    s: &Session,
    missing: &[Value],
    supplied: &[crate::memory::Source],
) -> Result<Vec<crate::memory::Source>> {
    let mut wanted = Vec::new();
    for range in missing {
        let (Some(path), Some(start), Some(end)) = (
            range["path"].as_str(),
            range["start_line"].as_u64(),
            range["end_line"].as_u64(),
        ) else {
            continue;
        };
        let resolved = read_path(&s.project, path)?;
        let current = hash_file(&resolved)?;
        wanted.push((resolved, current, start as usize, end as usize));
    }
    let mut found: Vec<crate::memory::Source> = Vec::new();
    for source in s.sources.values() {
        if source.origin != "file"
            || source.evidence_truncated
            || supplied
                .iter()
                .chain(&found)
                .any(|known| known.id == source.id)
        {
            continue;
        }
        let (Some(path), Some(start), Some(end)) =
            (source.path.as_deref(), source.start_line, source.end_line)
        else {
            continue;
        };
        let Ok(resolved) = read_path(&s.project, path) else {
            continue;
        };
        if wanted
            .iter()
            .any(|(want_path, current, want_start, want_end)| {
                *want_path == resolved
                    && source.hash.as_deref() == Some(current.as_str())
                    && start <= *want_end
                    && end >= *want_start
            })
        {
            found.push(source.clone());
        }
    }
    found.sort_by_key(|source| (source.path.clone(), source.start_line));
    Ok(found)
}

/// Most recent delivered sources kept for one path given as a source ID.
const PATH_SOURCE_LIMIT: usize = 8;

/// Live runs pass project paths, and `path:10-20` citations, as memory input
/// source_ids. Replace each with the evidence already delivered from that
/// file (range) in its current version, as investigation verify does; a path
/// with no delivered evidence still fails, so nothing unobserved is cited.
fn resolve_path_source_ids(
    s: &Session,
    ids: &[String],
) -> Result<(Vec<String>, serde_json::Map<String, Value>)> {
    let mut out: Vec<String> = Vec::new();
    let mut resolved = serde_json::Map::new();
    for id in ids {
        let path = strip_line_suffix(id);
        if s.source_refs(std::slice::from_ref(id)).is_ok()
            || !(id.contains('/') || id.contains('.'))
            || read_path(&s.project, path).is_err()
        {
            out.push(id.clone());
            continue;
        }
        let (start, end) = id[path.len()..]
            .strip_prefix(':')
            .map(|lines| {
                let mut bounds = lines.split('-').map(|b| b.parse::<usize>().unwrap_or(0));
                let start = bounds.next().unwrap_or(0);
                (start, bounds.next().unwrap_or(start))
            })
            .unwrap_or((1, usize::MAX));
        let mut found = delivered_sources_for(
            s,
            &[json!({"path":path,"start_line":start,"end_line":end})],
            &[],
        )?;
        if found.is_empty() {
            bail!(
                "unknown_source: {id} is a path, not a source ID, and no complete lines of its current version were delivered in this session; file_read the relevant range first, then pass the returned S-ID"
            );
        }
        found.sort_by_key(|source| std::cmp::Reverse(source.observed_at));
        let ids: Vec<String> = found
            .into_iter()
            .take(PATH_SOURCE_LIMIT)
            .map(|source| source.id)
            .filter(|found| !out.contains(found))
            .collect();
        out.extend(ids.iter().cloned());
        resolved.insert(id.clone(), json!(ids));
    }
    Ok((out, resolved))
}

/// A leading `1.`, `2.3` or `4)` label, without its trailing punctuation.
/// A bare number such as a year is not a label.
fn section_number(title: &str) -> Option<&str> {
    let end = title
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(title.len());
    let (raw, rest) = title.split_at(end);
    let label = raw.trim_end_matches('.');
    let (rest, paren) = match rest.strip_prefix(')') {
        Some(rest) => (rest, true),
        None => (rest, false),
    };
    (label.starts_with(|c: char| c.is_ascii_digit())
        && (paren || raw.ends_with('.') || label.contains('.'))
        && rest.starts_with(char::is_whitespace))
    .then_some(label)
}

/// `path:10` or `path:10-20` without its line range; anything else unchanged.
fn strip_line_suffix(id: &str) -> &str {
    let Some((path, lines)) = id.rsplit_once(':') else {
        return id;
    };
    let mut bounds = lines.split('-');
    let numeric = |bound: Option<&str>| {
        bound.is_some_and(|b| !b.is_empty() && b.bytes().all(|c| c.is_ascii_digit()))
    };
    let valid = numeric(bounds.next())
        && bounds.next().is_none_or(|b| numeric(Some(b)))
        && bounds.next().is_none();
    if valid && !path.is_empty() { path } else { id }
}
fn section_text<'a>(doc: &'a str, heading: &str) -> Result<&'a str> {
    let h = documentation::resolve_heading(doc, heading)?;
    Ok(&doc[h.start..h.end])
}
fn bounded_text(s: &Session, content: &str, offset: usize) -> Value {
    let byte_offset = content
        .char_indices()
        .nth(offset)
        .map_or(content.len(), |(byte, _)| byte);
    let (body, truncated) = context::truncate(
        &content[byte_offset..],
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
/// A cursor that this tool did not issue, or that no longer points into the
/// result. Models sometimes invent one; say how to get a real one.
pub(crate) const INVALID_CURSOR: &str = "invalid_cursor: not a cursor this tool issued for these results; copy next_cursor exactly from the previous result, or omit cursor to start from the beginning";

fn page_cursor(args: &Value, fingerprint: &str) -> Result<usize> {
    if let Some(cursor) = args["cursor"].as_str() {
        let (h, index) = cursor
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!(INVALID_CURSOR))?;
        if h != fingerprint {
            bail!(
                "cursor_expired: this cursor was issued for different arguments (path_glob, mode, query or filters) or the listing changed; pass the cursor with exactly the original arguments, or restart without a cursor"
            );
        }
        index
            .parse::<usize>()
            .map_err(|_| anyhow::anyhow!(INVALID_CURSOR))
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
    args: Value,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    if s.read_only_turn && !ToolRegistry::question_allows(name) {
        bail!("question_tools_not_allowed: {name} changes task state or files; task preserved");
    }
    let _write = writes::acquire(name, cancel, &s.write_outcome_uncertain)?;
    let shape = arguments::shape(&args);
    let result = execute_arguments(s, name, args, cancel).map_err(|error| {
        match error.downcast_ref::<DirectoryPath>() {
            Some(directory) => directory_error(&s.project, directory, name),
            None => arguments::annotate_runtime(error, name, &shape),
        }
    });
    // Publish an uncertain commit before releasing the gate; another session
    // must not start writing in the gap before the agent receives this result.
    if external_write(name)
        && result
            .as_ref()
            .err()
            .is_some_and(|error| uncertain_write_error(&error.to_string()))
    {
        s.write_outcome_uncertain
            .store(true, std::sync::atomic::Ordering::Release);
    }
    result
}

fn execute_arguments(
    s: &mut Session,
    name: &str,
    mut args: Value,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    reject_project_write_in_source_document(s, name)?;
    if !repair_leaked_argument_markup(&mut args)
        .map_err(|error| memory_tools::argument_error(error, name, &args))?
    {
        return execute_repaired(s, name, args, cancel);
    }
    // The model believes it sent what its markup held. Say which fields
    // arrived so a missing one is resent instead of repeating the call.
    let received = args
        .as_object()
        .map(|object| object.keys().cloned().collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    execute_repaired(s, name, args, cancel).map_err(|error| {
        // Cancellation is matched exactly.
        let message = error.to_string();
        if message == "cancelled" || message.starts_with("cancelled:") {
            return error;
        }
        let message = format!(
            "{error}. Note: this call's arguments arrived with native tool-call markup (<arg_key>/<arg_value>) inside a JSON key; they were recovered, but only these fields were received: {received}. Resend every argument, including any reported missing, as a plain JSON field without <arg_key> tags"
        );
        if let Some(diagnostic) = error.downcast_ref::<recovery::DiagnosticError>() {
            let mut data = diagnostic.data.clone();
            if data["input_error"].is_object() {
                data["input_error"]["native_markup"] = json!(true);
                data["input_error"]["markup_recovered"] = json!(true);
            }
            return recovery::DiagnosticError { message, data }.into();
        }
        anyhow::anyhow!(message)
    })
}

fn execute_repaired(
    s: &mut Session,
    name: &str,
    mut args: Value,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    unwrap_task_state_call(name, &mut args);
    normalize_integer_arguments(name, &mut args);
    unwrap_continuation_cursor(name, &mut args);
    normalize_argument_aliases(s, name, &mut args)?;
    normalize_lone_carriage_returns(name, &mut args);
    drop_empty_unaccepted_fields(name, &mut args);
    if name == "source_search"
        && args["query"]
            .as_str()
            .is_some_and(|query| !query.is_empty())
        && args["queries"].as_array().is_some_and(Vec::is_empty)
    {
        args.as_object_mut().unwrap().remove("queries");
    }
    if name == "memory_manage"
        && args["action"] == "replace"
        && let Some(replacement) = args.get_mut("replacement")
    {
        normalize_integer_arguments("memory_write", replacement);
        normalize_argument_aliases(s, "memory_write", replacement)?;
    }
    if name == "memory_write"
        && args["expected_revision"] == 0
        && let Some(key) = args["key"].as_str()
        && !s
            .memory
            .entries
            .values()
            .any(|entry| entry.key.as_deref() == Some(key))
    {
        args.as_object_mut().unwrap().remove("expected_revision");
    }
    if name == "document_edit_batch" {
        hoist_batch_expected_hash(&mut args)?;
        if let Some(object) = args.as_object_mut() {
            fill_hash_of_own_write(s, object);
        }
    }
    if name == "file_read"
        && let Some(fields) = args.as_object_mut()
        && let Some(limit) = fields.remove("limit")
    {
        if fields.get("offset").is_some_and(|v| v != &json!(0)) {
            bail!(
                "ambiguous_file_read_range: limit means max_lines, but offset is a character offset within the selected range, NOT a line number. For a new range use path/start_line/max_lines and omit offset. For continuation copy only {{cursor: next_cursor.cursor}}"
            );
        }
        if let Some(max_lines) = fields.get("max_lines") {
            if max_lines == &json!(0) && limit != json!(0) {
                fields.insert("max_lines".into(), limit);
            } else if limit != json!(0) && max_lines != &limit {
                bail!(
                    "conflicting_arguments: file_read limit and max_lines differ; supply only max_lines (number of lines)"
                );
            }
        } else {
            fields.insert("max_lines".into(), limit);
        }
    }
    ToolRegistry::validate(s, name, &args)
        .map_err(|error| memory_tools::argument_error(error, name, &args))?;
    memory_tools::validate_input(name, &args)?;
    check_whole_write(s, name, &args)?;
    if let Some(cp) = s
        .checkpoint
        .as_ref()
        .filter(|_| !ToolRegistry::checkpoint_allowed(name))
    {
        bail!(
            "checkpoint_pending: {name} is withheld while checkpoint {} is pending; nothing was executed. Call checkpoint_complete with this id and progress (memory_write first only for a reusable finding not saved yet); {name} is available again afterwards. Allowed now: {}",
            cp.id,
            ToolRegistry::CHECKPOINT_TOOLS.join(", ")
        );
    }
    if s.run_guidance["phase"] == "verify"
        && s.document_written
        && ((name == "file_list"
            && args["cursor"].as_str().is_none()
            && !["path", "path_glob", "pattern"].iter().any(|key| {
                args[*key].as_str().is_some_and(|pattern| {
                    !matches!(pattern.trim(), "" | "." | "./" | "*" | "**" | "**/*")
                })
            }))
            || (name == "symbol_search"
                && args["query"].as_str().unwrap_or("").is_empty()
                && !["path", "path_glob", "pattern"].iter().any(|key| {
                    args[*key].as_str().is_some_and(|path| {
                        !matches!(path.trim(), "" | "." | "./" | "*" | "**" | "**/*")
                    })
                })))
    {
        bail!(
            "verification_reserve: use a targeted path_glob or nonempty symbol query for missing evidence; broad discovery is paused"
        );
    }
    match name {
        "db_execute" => crate::database::execute_free(
            &s.config.database,
            &args,
            cancel,
            s.config.tool_timeout_secs,
        ),
        "db_query" => crate::database::execute(
            &s.config.database,
            &args,
            cancel,
            s.config.tool_timeout_secs,
        ),
        "code_outline" | "symbol_read" => structure::execute(s, name, &args, cancel),
        "symbol_search" => navigation::search(s, &args, cancel),
        "symbol_relations" => navigation::relations(s, &args, cancel),
        "document_inspect" | "document_audit" => documentation::execute(s, name, &args, cancel),
        "tool_catalog" => {
            let q = args["query"].as_str().unwrap_or("").to_lowercase();
            let terms: Vec<_> = q
                .split(|c: char| c.is_whitespace() || c == '_' || c == '-')
                .filter(|t| !t.is_empty())
                .collect();
            Ok(
                json!({"groups":["source-docs"],"tools":ToolRegistry::specs().into_iter().filter(|t|t.name != "db_query" || s.config.database.active_queries().next().is_some()).filter(|t|t.name != "db_execute" || s.config.database.free_execution_enabled()).filter(|t|terms.is_empty() || terms.iter().any(|term| t.name.contains(term) || t.description.to_lowercase().contains(term))).map(|t|json!({"name":t.name,"description":t.description,"basic":!t.optional,"active":!t.optional||s.active_tools.contains(t.name),"selectable":!s.workflow_forbidden_tools().contains(&t.name)})).collect::<Vec<_>>()}),
            )
        }
        "tool_select" => {
            let available = ToolRegistry::optional_names();
            let mut chosen = BTreeSet::new();
            for name in list(&args, "names") {
                if name == "source-docs" {
                    let forbidden = s.workflow_forbidden_tools();
                    chosen.extend(
                        available
                            .iter()
                            .filter(|n| !forbidden.contains(&n.as_str()))
                            .cloned(),
                    );
                } else if available.contains(&name) {
                    chosen.insert(name);
                } else {
                    let basic = ToolRegistry::specs()
                        .iter()
                        .any(|spec| spec.name == name && !spec.optional);
                    bail!(
                        "unknown_optional_tool_or_basic_tool: {name} {}; tool_select accepts only optional tool names or the source-docs group: {}",
                        if basic {
                            "is a basic tool that is always available and cannot be selected"
                        } else {
                            "is not a tool name"
                        },
                        available.iter().cloned().collect::<Vec<_>>().join(", ")
                    );
                }
            }
            let mut pending = s.pending_tools.clone().unwrap_or(s.active_tools.clone());
            match text(&args, "action")? {
                "add" => pending.extend(chosen),
                "remove" => pending.retain(|n| !chosen.contains(n)),
                "replace" => pending = chosen,
                _ => unreachable!(),
            };
            ToolRegistry::validate_tool_selection(s, &pending)?;
            s.pending_tools = Some(pending.clone());
            Ok(json!({"pending":pending,"applies":"next_request"}))
        }
        "memory_write" => {
            let mut input: MemoryInput = serde_json::from_value(args).map_err(|error| {
                anyhow::anyhow!("invalid_argument_value: memory_write fields: {error}")
            })?;
            let resolved_paths;
            (input.source_ids, resolved_paths) = resolve_path_source_ids(s, &input.source_ids)?;
            let sources = s.source_refs(&input.source_ids)?;
            let result = s
                .memory
                .save(input, sources, &s.config)
                .map_err(|error| memory_tools::revision_error(error, name))?;
            // A source can change between its original observation and this
            // write. Revalidate the newly stored entry too, so stale evidence
            // cannot be reported as an active fact until it is reread.
            let id = result.id.clone();
            revalidate(s)?;
            let mut meta = json!(s.memory.get(&id)?.meta());
            if !resolved_paths.is_empty() {
                meta["resolved_source_ids"] = Value::Object(resolved_paths);
            }
            Ok(meta)
        }
        "memory_read" => {
            if !s.config.memory_reuse {
                bail!("unsupported: memory reuse disabled for evaluation");
            }
            s.memory_loads += 1;
            revalidate(s)?;
            let m = s.memory.get(text(&args, "id")?)?;
            Ok(
                json!({"metadata":m.meta(),"custom_metadata":m.metadata,"body":bounded_text(s,&m.body,n(&args,"offset",0)),"sources":m.sources,"inferred":m.inferred,"kind":m.kind}),
            )
        }
        "memory_find" => {
            revalidate(s)?;
            s.memory.page(
                args["query"].as_str().unwrap_or(""),
                &list(&args, "tags"),
                args["cursor"].as_str(),
                n(&args, "limit", 20),
            )
        }
        "memory_manage" => {
            let ids = list(&args, "ids");
            match text(&args, "action")? {
                "candidates" => Ok(
                    json!({"bytes":s.memory.bytes(),"candidates":s.memory.candidates(&s.protected())}),
                ),
                "delete" => {
                    if ids.is_empty() {
                        bail!("missing_argument: ids for memory_manage action=delete");
                    }
                    let mut copy = s.memory.clone();
                    let mut seen = BTreeSet::new();
                    let actual = ids
                        .iter()
                        .map(|id| s.memory.get(id).map(|memory| memory.id.clone()))
                        .collect::<Result<Vec<_>>>()?
                        .into_iter()
                        .filter(|id| seen.insert(id.clone()))
                        .collect::<Vec<_>>();
                    for id in actual {
                        copy.delete(&id, &s.protected())?;
                    }
                    s.memory = copy;
                    Ok(json!({"deleted":true}))
                }
                "replace" => {
                    if ids.is_empty() {
                        bail!("missing_argument: ids for memory_manage action=replace");
                    }
                    let mut seen = BTreeSet::new();
                    let actual = ids
                        .iter()
                        .map(|id| s.memory.get(id).map(|m| m.id.clone()))
                        .collect::<Result<Vec<_>>>()?
                        .into_iter()
                        .filter(|id| seen.insert(id.clone()))
                        .collect::<Vec<_>>();
                    let task_memory_ids = s
                        .task
                        .memory_ids
                        .iter()
                        .map(|ident| s.canonical_memory_id(ident))
                        .collect::<Vec<_>>();
                    let mut input: MemoryInput =
                        serde_json::from_value(args["replacement"].clone()).map_err(|error| {
                            anyhow::anyhow!(
                                "invalid_argument_value: memory_manage replacement fields: {error}"
                            )
                        })?;
                    let resolved_paths;
                    (input.source_ids, resolved_paths) =
                        resolve_path_source_ids(s, &input.source_ids)?;
                    let sources = s.source_refs(&input.source_ids)?;
                    let replacement = s
                        .memory
                        .replace(&actual, input, sources, &s.config)
                        .map_err(|error| memory_tools::revision_error(error, name))?;
                    let replacement_id = replacement.id.clone();
                    revalidate(s)?;
                    let m = s.memory.get(&replacement_id)?.meta();
                    for (id, canonical) in s.task.memory_ids.iter_mut().zip(task_memory_ids) {
                        *id = if actual.contains(&canonical) {
                            m.id.clone()
                        } else {
                            canonical
                        };
                    }
                    s.task.memory_ids.sort();
                    s.task.memory_ids.dedup();
                    s.task.revision += 1;
                    let mut meta = json!(m);
                    if !resolved_paths.is_empty() {
                        meta["resolved_source_ids"] = Value::Object(resolved_paths);
                    }
                    Ok(meta)
                }
                _ => unreachable!(),
            }
        }
        "task_plan" => task_plan::execute(s, &args),
        "task_state" => match text(&args, "action")? {
            "read" => Ok(context::ContextManager::task_snapshot(
                &s.task,
                &s.config.model,
                s.config.state_tokens,
            )),
            "details" => {
                let offset = n(&args, "offset", 0);
                let limit = n(&args, "limit", 20).clamp(1, 100);
                Ok(
                    json!({"items":s.task.details.iter().skip(offset).take(limit).collect::<Vec<_>>(),"next_offset":(offset.saturating_add(limit)<s.task.details.len()).then_some(offset.saturating_add(limit))}),
                )
            }
            "update" => {
                let patch = args["patch"].as_object().ok_or_else(|| {
                    anyhow::anyhow!("missing_argument: patch for task_state action=update")
                })?;
                if patch.is_empty() {
                    return Ok(
                        json!({"unchanged":true,"guidance":"Use read to inspect state; update only changed fields."}),
                    );
                }
                let mut value = json!(s.task);
                for (k, v) in patch {
                    if k == "revision" {
                        bail!(
                            "invalid_argument_value: patch.revision is program-owned; remove revision from patch and resend the other fields; state unchanged"
                        );
                    }
                    value[k] = v.clone();
                }
                let mut next: TaskState = serde_json::from_value(value).map_err(|error| {
                    anyhow::anyhow!("invalid_argument_value: task_state patch: {error}")
                })?;
                if patch.contains_key("completion")
                    && (next.completion.is_empty()
                        || next
                            .completion
                            .iter()
                            .any(|criterion| criterion.trim().is_empty()))
                {
                    bail!(
                        "completion_required: provide non-empty completion criteria, or omit completion to preserve the current criteria"
                    );
                }
                // The user selects the workflow in the session window; the
                // model fills in the task but cannot reclassify the request.
                if next.workflow != s.task.workflow {
                    bail!(
                        "workflow_selected_by_user: the user selected workflow={} for this session; omit workflow from the patch",
                        s.task.workflow
                    );
                }
                if !["", "investigate", "draft", "verify", "answer"].contains(&next.phase.as_str())
                {
                    bail!("invalid_task_phase: use investigate, draft, verify or answer");
                }
                for id in &next.memory_ids {
                    s.memory.get(id)?;
                }
                // Store canonical IDs so protection and replacement remain
                // correct even when the caller supplied a human-readable key.
                next.memory_ids = next
                    .memory_ids
                    .iter()
                    .map(|ident| s.memory.get(ident).map(|memory| memory.id.clone()))
                    .collect::<Result<Vec<_>>>()?;
                next.memory_ids.sort();
                next.memory_ids.dedup();
                // A declared phase only ratchets forward. Document work without
                // a written document or with pending plan items is not ready
                // for verify/answer; some providers fill every enum in the
                // patch schema, and accepting that value would pause discovery
                // before sections exist.
                let rank = |phase: &str| match phase {
                    "answer" => 3,
                    "verify" => 2,
                    "draft" => 1,
                    _ => 0,
                };
                let pending = next.todos.iter().filter(|item| !item.done).count();
                let phase_deferred = s.is_document_work()
                    && (pending > 0 || !s.document_written)
                    && rank(&next.phase) >= rank("verify")
                    && rank(&next.phase) > rank(&s.task.phase);
                if phase_deferred {
                    next.phase = s.task.phase.clone();
                }
                next.revision = s.task.revision + 1;
                let compact = context::ContextManager::task_snapshot(
                    &next,
                    &s.config.model,
                    s.config.state_tokens,
                );
                let tokens = context::count(&compact, &s.config.model);
                if tokens > s.config.state_tokens {
                    bail!(
                        "task_state_limit: the updated task state needs {tokens} tokens but state_tokens allows {}; shorten the patched fields and keep long notes in patch.details or memory_write; state unchanged",
                        s.config.state_tokens
                    );
                }
                let bytes = serde_json::to_vec(&next)?.len();
                if bytes > s.config.memory_bytes {
                    bail!(
                        "task_detail_limit: the task state would be {bytes} bytes but memory_bytes allows {}; remove obsolete details; state unchanged",
                        s.config.memory_bytes
                    );
                }
                s.task = next;
                s.activate_workflow_tools();
                if phase_deferred {
                    return Ok(json!({"revision":s.task.revision,"phase_deferred":format!(
                        "phase stays {}: {}; other fields were saved",
                        if s.task.phase.is_empty() { "investigate" } else { s.task.phase.as_str() },
                        if s.document_written {
                            format!("{pending} task_plan items are pending; finish or remove them first")
                        } else {
                            "the document has not been written yet".into()
                        }
                    )}));
                }
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
        "source_lookup" => {
            if let Some(cp) = &mut s.checkpoint {
                if cp.source_lookup_calls >= crate::context::CHECKPOINT_SOURCE_LOOKUP_LIMIT {
                    bail!(
                        "checkpoint_lookup_limit: source IDs already available from earlier lookups; save needed memories and call checkpoint_complete"
                    );
                }
                cp.source_lookup_calls += 1;
            }
            let mut sources = std::collections::BTreeMap::new();
            for source in s
                .memory
                .entries
                .values()
                .filter(|_| s.config.memory_reuse)
                .flat_map(|m| &m.sources)
                .chain(s.sources.values())
            {
                sources.insert(source.id.clone(), source);
            }
            let path = args["path"].as_str().unwrap_or("");
            let id = args["id"].as_str();
            let mut matches: Vec<_> = sources
                .into_values()
                .filter(|source| {
                    id.is_none_or(|id| source.id == id)
                        && (path.is_empty()
                            || source.path.as_deref().is_some_and(|p| p.contains(path)))
                })
                .collect();
            matches.sort_by(|a, b| {
                b.observed_at
                    .cmp(&a.observed_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
            let offset = n(&args, "offset", 0);
            let limit = n(&args, "limit", 3).clamp(1, 10);
            let items: Vec<_> = matches
                .iter()
                .skip(offset)
                .take(limit)
                .map(|source| {
                    json!({
                        "id":source.id, "path":source.path, "start_line":source.start_line,
                        "end_line":source.end_line,
                        "line_start_complete":source.line_start_complete,
                        "line_end_complete":source.line_end_complete,
                        "evidence_truncated":source.evidence_truncated,
                        "hash":source.hash,
                        "excerpt":source.excerpt.chars().take(600).collect::<String>(),
                        "excerpt_truncated":source.excerpt.chars().count() > 600
                    })
                })
                .collect();
            let next = offset.saturating_add(items.len());
            // An empty lookup is not proof that the ID is valid or that the
            // file has no content; name where evidence IDs come from.
            let notice = if matches.is_empty() && (id.is_some() || !path.is_empty()) {
                Some("No observed source matches this id/path. Source IDs are created only by delivered file_read, symbol_read, source_search or detailed code_outline results in this session; copy one exactly, or read the file to observe new evidence. Do not invent an ID.".to_owned())
            } else if offset >= matches.len() && !matches.is_empty() {
                Some(format!(
                    "offset {offset} is past the {} matching sources; use offset 0 to {}",
                    matches.len(),
                    matches.len() - 1
                ))
            } else {
                None
            };
            let mut page = json!({"items":items,"total":matches.len(),"next_offset":(next < matches.len()).then_some(next),
                    "checkpoint_lookup_remaining":s.checkpoint.as_ref().map(|cp| crate::context::CHECKPOINT_SOURCE_LOOKUP_LIMIT.saturating_sub(cp.source_lookup_calls)),
                    "checkpoint_next_action":s.checkpoint.as_ref().map(|_| "Use delivered source IDs in memory_write, then call checkpoint_complete; repeated lookup does not preserve findings")});
            if let Some(notice) = notice {
                page["notice"] = json!(notice);
            }
            Ok(page)
        }
        "checkpoint_complete" => {
            let cp = s
                .checkpoint
                .as_ref()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no_checkpoint: no checkpoint is pending; call checkpoint_complete only in answer to a CHECKPOINT CONTROL REQUEST and continue the task"
                    )
                })?;
            // Only one checkpoint is active, so a copied ID that the model cut
            // short or mistyped in a character or two still identifies it; a
            // tiny prefix or an unrelated ID does not.
            let supplied = text(&args, "id")?.trim();
            let typo = supplied.len() == cp.id.len()
                && supplied
                    .bytes()
                    .zip(cp.id.bytes())
                    .filter(|(a, b)| a != b)
                    .count()
                    <= 2;
            if cp.id != supplied && !typo && !(supplied.len() >= 8 && cp.id.starts_with(supplied)) {
                bail!("checkpoint_id_mismatch: expected checkpoint ID {}", cp.id);
            }
            if cp.failed {
                bail!(
                    "checkpoint_has_failed_operations: an earlier operation in this batch failed; inspect its error and repair it on the next model request before checkpoint_complete. Successful writes are retained; do not repeat them. Completion cannot succeed in this failed batch"
                );
            }
            let progress = text(&args, "progress")?;
            if progress.trim().is_empty() {
                bail!("invalid_argument_value: checkpoint progress must be nonempty");
            }
            let mut task = s.task.clone();
            task.checkpoint_summary = progress.to_string();
            if let Some(next) = args["next"].as_str().filter(|next| !next.trim().is_empty()) {
                task.checkpoint_summary.push_str(&format!("\n{next}"));
            }
            task.revision += 1;
            let compact = context::ContextManager::task_snapshot(
                &task,
                &s.config.model,
                s.config.state_tokens,
            );
            if context::count(&compact, &s.config.model) > s.config.state_tokens {
                bail!("task_state_limit: shorten checkpoint summary; original plan retained");
            }
            if serde_json::to_vec(&task)?.len() > s.config.memory_bytes {
                bail!("task_detail_limit: shorten checkpoint summary; original plan retained");
            }
            s.task = task;
            s.checkpoint.as_mut().unwrap().acknowledged = true;
            Ok(json!({"acknowledged":true,"state_revision":s.task.revision}))
        }
        "file_list" => {
            // A continuation that omits its original mode and file or
            // directory scope keeps the scope its cursor was issued for.
            if let Some(scope) = args["cursor"]
                .as_str()
                .and_then(|cursor| cursor.split_once(':'))
                .and_then(|(fingerprint, _)| {
                    s.list_cursor_scopes
                        .iter()
                        .find(|(known, _)| known == fingerprint)
                        .map(|(_, scope)| scope.clone())
                })
            {
                for key in ["mode", "path_glob", "path"] {
                    if args.get(key).is_none() && !scope[key].is_null() {
                        args[key] = scope[key].clone();
                    }
                }
            }
            // A directory path is literal. Turning it into a glob would make
            // names such as [route] select a different directory (and reject
            // a valid directory containing an unmatched '[').
            let directory = if let Some(path) = args.get("path").and_then(Value::as_str) {
                if args.get("path_glob").is_some() || args.get("pattern").is_some() {
                    bail!(
                        "conflicting_arguments: use path for one directory OR path_glob/pattern for a file glob{}",
                        path_glob_conflict_hint(s, &args)
                    );
                }
                let dir = read_path(&s.project, path)?;
                if !dir.is_dir() {
                    bail!(
                        "invalid_argument_value: path must be a directory for file_list; read a file with file_read"
                    );
                }
                Some(dir)
            } else {
                None
            };
            let mode = args["mode"].as_str().unwrap_or("text");
            let mut files = candidate_paths_scoped(
                &s.project,
                path_glob(&args)?,
                cancel,
                directory.as_deref(),
                None,
            )?;
            if mode != "paths" {
                let mut searchable = Vec::new();
                for path in files {
                    if cancel.is_cancelled() {
                        bail!("cancelled");
                    }
                    if search_text(&path)?.is_some() {
                        searchable.push(path);
                    }
                }
                files = searchable;
            }
            let root = s.project.root.canonicalize()?;
            let names = files
                .iter()
                // The root can be replaced after the walk canonicalized it.
                .map(|p| p.strip_prefix(&root).unwrap_or(p).display().to_string())
                .collect::<Vec<_>>();
            let fingerprint = hash(
                serde_json::to_string(&(mode, path_glob(&args)?, &directory, &names))?.as_bytes(),
            );
            let offset = page_cursor(&args, &fingerprint)?;
            if offset > names.len() {
                bail!(INVALID_CURSOR);
            }
            let end = (offset + n(&args, "limit", 100).clamp(1, 500)).min(names.len());
            if end < names.len() {
                s.remember_list_scope(
                    fingerprint.clone(),
                    json!({"mode":mode,"path_glob":path_glob(&args)?,"path":args.get("path")}),
                );
            }
            Ok(
                json!({"hash":fingerprint,"mode":mode,"total_files":names.len(),"paths":names[offset..end],"next_cursor":(end<names.len()).then(||format!("{fingerprint}:{end}"))}),
            )
        }
        "source_search" => search::execute(s, &args, cancel),
        "file_read" => {
            // A provider that fills every field sends each continuation as
            // its cursor beside start_line 1 and a one-line page. Read as a
            // new range, that returned line 1 alone, and even with the note
            // below a live run sent the same call again and gave up on
            // another cursor. Line 1 alone is never what a cursor continues,
            // so those values are placeholders while the cursor still holds.
            let placeholder_range = args["cursor"]
                .as_str()
                .filter(|_| {
                    args["start_line"].as_u64().is_some_and(|line| line <= 1)
                        && args["max_lines"].as_u64().is_some_and(|lines| lines <= 1)
                        && args.get("offset").is_none()
                })
                .and_then(|id| {
                    Some((id.to_owned(), cursor_next_line(s, s.file_cursors.get(id)?)?))
                });
            if placeholder_range.is_some() {
                let fields = args.as_object_mut().unwrap();
                fields.remove("start_line");
                fields.remove("max_lines");
            }
            // A cursor continues its original range, so a page size sent with
            // it changes nothing. Ignore it rather than reject a correct
            // continuation; new-range arguments with a cursor still conflict.
            // read_file still rejects a path naming another file.
            let ignored_page_size = args["cursor"].is_string()
                && args.get("max_lines").is_some()
                && !["start_line", "offset"]
                    .iter()
                    .any(|key| args.get(key).is_some());
            if ignored_page_size {
                args.as_object_mut().unwrap().remove("max_lines");
            }
            // read_file reads an explicit start_line as a new range and drops
            // the cursor. Without a word about it, a live model took that
            // range for the continuation and re-read it in another request.
            let dropped_cursor = args["cursor"]
                .as_str()
                .filter(|_| args.get("start_line").is_some())
                .and_then(|id| Some((id.to_owned(), s.file_cursors.get(id)?.clone())));
            let start = n(&args, "start_line", 1).max(1);
            let mut result = read_file(s, &mut args, cancel)?;
            if let Some((id, next)) = placeholder_range {
                result["ignored_arguments"] = json!(["start_line", "max_lines"]);
                result["ignored_note"] = json!(format!(
                    "start_line 1 with a one-line page beside cursor {id} was taken as unfilled placeholders, so {id} was continued from line {next}. For a new range, send start_line and max_lines without cursor."
                ));
            }
            if ignored_page_size {
                result["ignored_arguments"] = json!(["max_lines"]);
                result["ignored_note"] = json!(
                    "A cursor continues its original range; max_lines was ignored. Pass only the cursor to continue."
                );
            }
            if let Some((id, cursor)) = dropped_cursor {
                let note = match cursor_next_line(s, &cursor) {
                    // The new range already starts where the cursor would.
                    Some(next) if next == start => None,
                    Some(next) => Some(format!(
                        "start_line was given, so this read is a new range from line {start} and cursor {id} was not continued. {id} continues at line {next}: send only {{\"cursor\":\"{id}\"}} (no path, start_line, max_lines or limit), or read from start_line {next}."
                    )),
                    None => Some(format!(
                        "start_line was given, so this read is a new range from line {start} and cursor {id} was not continued. {id} no longer applies: its file changed after it was issued."
                    )),
                };
                if let Some(note) = note {
                    result["ignored_arguments"] = json!(["cursor"]);
                    result["ignored_note"] = json!(note);
                }
            }
            Ok(result)
        }
        "file_edit" | "file_write" | "file_patch" if ToolRegistry::project_writes_withheld(s) => {
            bail!(WORKFLOW_WRITE_SCOPE_ERROR)
        }
        "file_edit" | "file_write" | "file_patch" => file_edit::execute(s, name, &args, cancel),
        "document_edit" => {
            let path = output_path(&s.project)?;
            let exists = path.exists();
            let old = if exists {
                read_text(&path)?
            } else {
                String::new()
            };
            let action = text(&args, "action")?;
            if action == "create" && exists {
                bail!(
                    "document_exists: the configured output already exists (hash {}); create was not applied. Edit it with section or text actions, or replace all of it with action=write and this expected_hash",
                    hash(old.as_bytes())
                );
            }
            // Anchored text edits match an exact old_text once in the current
            // document; that match is their precondition, so a hash is
            // optional for them. A supplied hash is still enforced.
            let anchored = is_anchored_text_edit(action);
            let omitted = args.get("expected_hash").is_none();
            if exists && action != "create" && !anchored && args["expected_hash"].as_str().is_none()
            {
                let failures: Vec<String> = apply_document_edit_operation(&old, &args)
                    .err()
                    .map(|error| error.to_string())
                    .into_iter()
                    .collect();
                return Err(missing_document_hash(
                    &format!("document_edit action={action}"),
                    false,
                    &failures,
                ));
            }
            let digest = hash(old.as_bytes());
            if exists
                && !(anchored && omitted)
                && args["expected_hash"].as_str() != Some(digest.as_str())
            {
                let checked = args["expected_hash"]
                    .as_str()
                    .is_some_and(|expected| !is_document_hash(expected))
                    .then(|| {
                        let failures: Vec<String> = apply_document_edit_operation(&old, &args)
                            .err()
                            .map(|error| error.to_string())
                            .into_iter()
                            .collect();
                        checked_clause(false, &failures)
                    });
                return Err(revision_conflict(&args, &digest, checked));
            }
            if !exists && action != "create" && action != "write" {
                bail!(
                    "document_missing: the output document does not exist yet; create it with document_edit action=create and no expected_hash"
                );
            }
            let result = apply_document_edit_operation(&old, &args)?;
            persist_document_edit(s, &path, &old, exists, result, cancel)
        }
        "document_edit_batch" => {
            let path = output_path(&s.project)?;
            let exists = path.exists();
            if !exists {
                bail!(
                    "document_missing: the output document does not exist yet; create it with document_edit action=create and no expected_hash"
                );
            }
            let old = read_text(&path)?;
            let digest = hash(old.as_bytes());
            // A missing hash still lets the edits be checked in memory, so
            // the error names every other problem too.
            let hash_missing = args["expected_hash"].as_str().is_none();
            // So does a malformed hash, which no tool issued; a stale one
            // means the document changed and must be read again first.
            let hash_malformed = args["expected_hash"]
                .as_str()
                .is_some_and(|expected| !is_document_hash(expected));
            if !hash_missing
                && !hash_malformed
                && args["expected_hash"].as_str() != Some(digest.as_str())
            {
                return Err(revision_conflict(&args, &digest, None));
            }
            let edits = args["edits"].as_array().expect("validated edits array");
            let mut current = old.clone();
            // states[k] is the text after edits[applied[k - 1]].
            let mut states = vec![old.clone()];
            let mut applied = Vec::with_capacity(edits.len());
            let mut operations = Vec::with_capacity(edits.len());
            // Keep checking after a failure so one response names every
            // operation to fix instead of one per retry.
            let mut failures = Vec::new();
            let mut failed_edits = Vec::new();
            for (index, edit) in edits.iter().enumerate() {
                if cancel.is_cancelled() {
                    bail!("cancelled");
                }
                let action = edit["action"].as_str().unwrap_or("unknown");
                let before_hash = hash(current.as_bytes());
                let next = match apply_document_edit_operation(&current, edit) {
                    Ok(next) => next,
                    Err(error) => {
                        let hint = batch_target_hint(&states, &applied, edit, &error.to_string());
                        failed_edits.push(json!({
                            "index":index,"action":action,
                            "code":recovery::error_code(&error.to_string())
                        }));
                        failures.push(format!(
                            "index={index}; action={action}; cause={error}{hint}"
                        ));
                        continue;
                    }
                };
                current = next;
                states.push(current.clone());
                applied.push(index);
                operations.push(json!({
                    "index":index,
                    "action":action,
                    "changed":before_hash != hash(current.as_bytes()),
                    "hash":hash(current.as_bytes())
                }));
            }
            if hash_missing {
                return Err(missing_document_hash(
                    "document_edit_batch",
                    true,
                    &failures,
                ));
            }
            if hash_malformed {
                return Err(revision_conflict(
                    &args,
                    &digest,
                    Some(checked_clause(true, &failures)),
                ));
            }
            if let Some((first, rest)) = failures.split_first() {
                let rest = if rest.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; {} operations failed, also {}; operations after a failed one were checked without it, so fix every listed operation",
                        failures.len(),
                        rest.iter()
                            .map(|failure| format!("[{failure}]"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                // Each failure keeps its own cause code: a stale section hash
                // needs a fresh inspection, a wrong old_text a corrected edit.
                return Err(recovery::DiagnosticError {
                    message: format!(
                        "document_batch_operation_failed: {first}{rest}; no changes persisted"
                    ),
                    data: json!({"execution":"rejected_without_changes","failed_edits":failed_edits}),
                }
                .into());
            }
            let result = persist_document_edit(s, &path, &old, exists, current, cancel)?;
            let final_hash = result["hash"].clone();
            let mut result = result;
            result["operations"] = json!(operations);
            result["operation_count"] = json!(edits.len());
            result["batch"] =
                json!({"base_hash":args["expected_hash"],"final_hash":final_hash,"atomic":true});
            Ok(result)
        }
        _ => bail!("unsupported_tool"),
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
struct FileReadMetadata {
    path: String,
    source: FileReadHash,
    read_start: usize,
    read_offset: usize,
    read_max_lines: usize,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct FileReadHash {
    hash: String,
}

fn file_read_metadata(message: &Value) -> Option<FileReadMetadata> {
    if message["role"] != "tool" {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct ReadResult {
        data: FileReadMetadata,
    }
    // Ignore message bodies during deserialization rather than allocating a
    // complete Value for every previous tool result just to inspect its range.
    serde_json::from_str::<ReadResult>(message["content"].as_str()?)
        .ok()
        .map(|result| result.data)
}

/// Isolated read workers need only active read identities for suppression,
/// and the owner's history sequence for any new result archives they create.
pub(crate) fn parallel_read_history(
    history: &crate::session::SessionHistory,
) -> crate::session::SessionHistory {
    crate::session::SessionHistory {
        bundles: history
            .bundles
            .iter()
            .filter(|bundle| bundle.active)
            .filter_map(|bundle| {
                let messages: Vec<_> = bundle
                    .messages
                    .iter()
                    .filter_map(file_read_metadata)
                    .map(|metadata| {
                        json!({"role":"tool","content":json!({"data":metadata}).to_string()})
                    })
                    .collect();
                (!messages.is_empty()).then_some(crate::session::Bundle {
                    id: bundle.id,
                    messages: messages.into(),
                    active: true,
                    reviewed: bundle.reviewed,
                    complete: bundle.complete,
                })
            })
            .collect(),
        next_id: history.next_id,
        pruned_through: history.pruned_through,
    }
}

/// The line a file cursor continues from, while its file is unchanged. The
/// cursor offset counts characters of its range, as read_file does.
fn cursor_next_line(s: &Session, cursor: &crate::session::FileCursor) -> Option<usize> {
    let contents = read_text(&read_path(&s.project, &cursor.path).ok()?).ok()?;
    if hash(contents.as_bytes()) != cursor.hash {
        return None;
    }
    let newlines = contents
        .split_inclusive('\n')
        .skip(cursor.start_line.saturating_sub(1))
        .take(cursor.max_lines)
        .flat_map(str::chars)
        .take(cursor.offset)
        .filter(|c| *c == '\n')
        .count();
    Some(cursor.start_line + newlines)
}

/// Shared line reader for explicit file reads and Tree-sitter symbol bodies.
fn read_file(
    s: &mut Session,
    args: &mut Value,
    _cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    let cursor = if let Some(id) = args["cursor"].as_str() {
        let cursor = s.file_cursors.get(id).cloned().ok_or_else(|| anyhow::anyhow!("invalid_file_cursor: copy next_cursor.cursor exactly from this session, or start a new read with path/start_line/max_lines"))?;
        if let Some(path) = args.get("path").and_then(Value::as_str)
            && read_path(&s.project, path)? != read_path(&s.project, &cursor.path)?
        {
            bail!(
                "cursor_arguments_conflict: cursor belongs to {}; for another file omit cursor and use path/start_line/max_lines",
                cursor.path
            );
        }
        args["path"] = json!(cursor.path);
        if args.get("start_line").is_some() {
            // An explicit start line on the cursor's file is a new range: read
            // it and drop the cursor (a live run was refused twice for this).
            if args.get("offset").is_some() {
                bail!(
                    "cursor_arguments_conflict: pass cursor alone to continue, or start_line/max_lines without cursor or offset for a new range"
                );
            }
            args.as_object_mut().unwrap().remove("cursor");
            None
        } else if args.get("offset").is_some() || args.get("max_lines").is_some() {
            bail!(
                "cursor_arguments_conflict: pass only cursor (and optional force_read); for a new range omit cursor and use path/start_line/max_lines"
            );
        } else {
            args["start_line"] = json!(cursor.start_line);
            args["max_lines"] = json!(cursor.max_lines);
            args["offset"] = json!(cursor.offset);
            Some(cursor)
        }
    } else {
        None
    };
    // A provider that fills every field sent path "" (dropped as a blank
    // placeholder) to read the output, and the bare missing-path error did
    // not say how to read it.
    let Some(path) = args["path"].as_str() else {
        let output = output_path(&s.project).unwrap_or_else(|_| s.project.output.clone());
        bail!(
            "missing_argument: path; file_read reads a project file by path (relative to project.root) or continues a truncated read with cursor alone. To read the configured output document, call document_inspect with section set to a heading from its outline, or file_read with path {}",
            output.display()
        );
    };
    let path = read_path(&s.project, path)?;
    let contents = read_text(&path)?;
    let digest = hash(contents.as_bytes());
    if cursor.as_ref().is_some_and(|c| c.hash != digest) {
        bail!(
            "file_cursor_expired: file changed; start a new read with path/start_line/max_lines and no cursor"
        );
    }
    let start = n(args, "start_line", 1).max(1);
    let lines = n(args, "max_lines", 120).clamp(1, 2000);
    let offset = n(args, "offset", 0);
    let total_lines = contents.lines().count();
    if start > total_lines {
        if offset != 0 {
            bail!(
                "invalid_offset: start_line {start} is beyond total_lines {total_lines}; start a new read without offset"
            );
        }
        // An empty page is not empty content; say which range was requested.
        let notice = if total_lines == 0 {
            "The file is empty; there are no lines to read.".to_owned()
        } else {
            format!(
                "start_line {start} is past the end of the file (total_lines {total_lines}); no lines were returned. Read with start_line between 1 and {total_lines}."
            )
        };
        return Ok(
            json!({"path":path,"hash":digest,"total_lines":total_lines,"content":{"text":"","truncated":false,"next_offset":null},"source":null,"eof":true,"next_line":null,"next_offset":0,"notice":notice}),
        );
    }
    let byte_start = contents
        .split_inclusive('\n')
        .take(start - 1)
        .map(str::len)
        .sum::<usize>();
    let byte_len = contents[byte_start..]
        .split_inclusive('\n')
        .take(lines)
        .map(str::len)
        .sum::<usize>();
    // The requested range is contiguous in the original file. Borrow it so
    // unreturned lines and long lines do not create another full-size buffer.
    let selected = &contents[byte_start..byte_start + byte_len];
    // Preserve internal line endings so a delivered multiline range can be
    // copied into an exact patch. Omit only the final line terminator, as
    // before; cursor offsets now count characters in the original text.
    let selected = selected
        .strip_suffix("\r\n")
        .or_else(|| selected.strip_suffix('\n'))
        .unwrap_or(selected);
    if offset > selected.chars().count() {
        bail!(
            "invalid_offset: offset {offset} exceeds {} characters in start_line={start}, max_lines={lines}. Offset is relative to this range, not the file. Copy next_cursor.cursor for continuation, or omit offset for a new range",
            selected.chars().count()
        );
    }
    let prior = s
        .history
        .bundles
        .iter()
        .filter(|b| b.active)
        .flat_map(|b| &b.messages)
        .filter_map(file_read_metadata)
        .filter(|metadata| {
            metadata.path == path.to_string_lossy()
                && metadata.source.hash == digest
                && metadata.read_start == start
                && metadata.read_offset == offset
                && metadata.read_max_lines == lines
        })
        .take(s.config.repeated_read_limit)
        .count();
    if prior >= s.config.repeated_read_limit && !args["force_read"].as_bool().unwrap_or(false) {
        return Ok(
            json!({"path":path,"hash":digest,"total_lines":contents.lines().count(),"repeated_read":true,"suppressed":true,"guidance":"Unchanged range already present repeatedly in active context. Reuse it, read another range, use document_inspect for output metadata, or force_read=true for deliberate verification."}),
        );
    }
    let mut content = bounded_text(s, selected, offset);
    let shown = content["text"].as_str().unwrap();
    let observed_start = start + selected.chars().take(offset).filter(|c| *c == '\n').count();
    let first_line_complete = offset == 0 || selected.chars().nth(offset - 1) == Some('\n');
    let shown_end = offset + shown.chars().count();
    let last_line_complete = if shown.ends_with('\n') {
        true
    } else {
        match selected.chars().nth(shown_end) {
            None => true,
            Some('\n') => !shown.ends_with('\r'),
            Some(_) => false,
        }
    };
    let source = observe_hashed_quality(
        s,
        &path,
        digest.clone(),
        observed_start,
        observed_start + shown.lines().count().saturating_sub(1),
        shown,
        EvidenceQuality {
            line_start_complete: first_line_complete,
            line_end_complete: last_line_complete,
            evidence_truncated: false,
        },
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
    content["line_end"] = json!(source.end_line);
    content["first_line_complete"] = json!(first_line_complete);
    content["last_line_complete"] = json!(last_line_complete);
    content["line_offsets"] = json!(line_offsets);
    let truncated = content["truncated"].as_bool().unwrap();
    let next_line = if truncated {
        Some(start)
    } else {
        (start - 1 + lines < contents.lines().count()).then_some(start + lines)
    };
    Ok(
        json!({"path":path,"total_lines":contents.lines().count(),"hash":digest,"read_start":start,"read_offset":offset,"read_max_lines":lines,"content":content,"source":source,"next_line":next_line,"next_offset":if truncated{content["next_offset"].clone()}else{json!(0)}}),
    )
}

/// Machine-readable source citations in the configured output; zero when
/// the document cannot be compared with any source.
pub fn citation_count(s: &Session) -> usize {
    output_path(&s.project)
        .and_then(|path| read_text(&path))
        .and_then(|doc| documentation::citation_spans(&doc))
        .map_or(0, |citations| citations.len())
}

/// Cited ranges of the configured output never delivered to the model as
/// complete lines of the current file version in this session.
pub fn unread_citations(s: &Session) -> Result<Vec<Value>> {
    let doc = read_text(&output_path(&s.project)?)?;
    documentation::unread_citations(s, &doc)
}

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
    let raw = context::count(
        &json!({"role":"tool","tool_call_id":call.id,"content":result.to_string()}),
        model,
    );
    let rendered = model_result(result);
    if rendered == *result {
        return raw;
    }
    let shown = context::count(
        &json!({"role":"tool","tool_call_id":call.id,"content":rendered.to_string()}),
        model,
    );
    raw.max(shown)
}

/// Present absolute line labels to the model without changing source text,
/// Unicode cursor offsets, archives or delivery coverage in the session.
pub fn model_result(result: &Value) -> Value {
    let mut shown = result.clone();
    if result["status"] != "ok" || !result["data"]["read_start"].is_u64() {
        return shown;
    }
    let content = &mut shown["data"]["content"];
    let (Some(text), Some(start)) = (content["text"].as_str(), content["line_start"].as_u64())
    else {
        return shown;
    };
    if text.is_empty() {
        return shown;
    }
    let numbered = text
        .split_inclusive('\n')
        .enumerate()
        .map(|(index, line)| format!("{}|{}", start + index as u64, line))
        .collect::<String>();
    let fields = content.as_object_mut().unwrap();
    fields.remove("text");
    fields.remove("line_offsets");
    fields.insert("numbered_text".into(), json!(numbered));
    shown
}

fn new_file_cursor_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    format!(
        "R{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}
fn file_cursor(result: &Value) -> crate::session::FileCursor {
    let data = &result["data"];
    crate::session::FileCursor {
        path: data["path"].as_str().unwrap().into(),
        hash: data["hash"].as_str().unwrap().into(),
        start_line: data["read_start"].as_u64().unwrap() as usize,
        max_lines: data["read_max_lines"].as_u64().unwrap() as usize,
        offset: data["next_offset"].as_u64().unwrap() as usize,
    }
}

fn existing_file_cursor<'a>(
    s: &'a Session,
    cursor: &crate::session::FileCursor,
) -> Option<&'a str> {
    s.file_cursors
        .iter()
        .find(|(_, existing)| *existing == cursor)
        .map(|(id, _)| id.as_str())
}

fn register_file_cursor(s: &mut Session, id: String, result: &Value) -> String {
    let cursor = file_cursor(result);
    // Repeated reads and repeated result bounding must not retain another
    // opaque ID for the same immutable continuation position.
    if let Some(existing) = existing_file_cursor(s, &cursor) {
        return existing.into();
    }
    s.file_cursors.insert(id.clone(), cursor);
    id
}

/// Keep results structured. Repeated bounding reuses the original archive rather
/// than serializing a preview of a preview (which expands escapes and hides IDs).
pub fn limit_result(
    s: &mut Session,
    call: &crate::llm::ToolCall,
    mut result: Value,
    limit: usize,
) -> Value {
    if call.name == "code_outline"
        && result["status"] == "ok"
        && let (Some(path), Some(cursor)) = (
            result["data"]["path"].as_str(),
            result["data"]["next_cursor"].as_str(),
        )
        && let Ok(mut args) = serde_json::from_str::<Value>(&call.arguments)
    {
        normalize_integer_arguments(call.name.as_str(), &mut args);
        result["next_cursor"] = structure::continuation(&args, path, cursor);
    }
    if matches!(call.name.as_str(), "symbol_search" | "symbol_relations")
        && result["status"] == "ok"
        && let Some(cursor) = result["data"]["next_cursor"].as_str()
        && let Ok(args) = serde_json::from_str::<Value>(&call.arguments)
    {
        result["next_cursor"] = navigation::continuation(&call.name, &args, cursor);
    }
    if matches!(call.name.as_str(), "file_read" | "symbol_read")
        && result["status"] == "ok"
        && result["data"]["content"]["truncated"] == true
        && !result["next_cursor"]["cursor"].is_string()
    {
        let id = register_file_cursor(s, new_file_cursor_id(), &result);
        result["truncated"] = json!(true);
        result["next_cursor"] = json!({"tool":"file_read","cursor":id});
    }
    if call.name == "document_inspect" && result["data"]["content"]["truncated"] == true {
        let mut args: Value = serde_json::from_str(&call.arguments).unwrap_or_default();
        normalize_integer_arguments(call.name.as_str(), &mut args);
        let mut next = json!({"tool":"document_inspect","path":result["data"]["path"],"section":args["section"],"offset":result["data"]["content"]["next_offset"],"expected_hash":result["data"]["hash"]});
        if result["data"]["coverage"]["revision"].is_string() {
            next["expected_coverage_revision"] = result["data"]["coverage"]["revision"].clone();
        }
        result["next_cursor"] = next;
        result["truncated"] = json!(true);
    }
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
    if call.name == "code_outline"
        && let Some(page) = structure::limit_outline(call, &output, limit, &s.config.model)
    {
        return page;
    }
    if matches!(call.name.as_str(), "symbol_search" | "symbol_relations")
        && result["status"] == "ok"
        && let Some(page) = navigation::limit_page(call, &output, limit, &s.config.model)
    {
        return page;
    }
    let mut args: Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    normalize_integer_arguments(call.name.as_str(), &mut args);
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
            // Empty/EOF results have no range metadata and cannot yield a
            // smaller body or a meaningful continuation. Use archive fallback.
            if text.is_empty() {
                continue;
            }
            let chars: Vec<_> = text.chars().collect();
            let mut template = output.clone();
            // Each shortened view gets its own immutable source observation.
            // Archives and previously delivered results keep their original IDs.
            let narrowed_source = if matches!(call.name.as_str(), "file_read" | "symbol_read")
                && pointer == "/data/content/text"
            {
                template["data"]["source"]["id"]
                    .as_str()
                    .and_then(|id| s.sources.get(id))
                    .cloned()
                    .map(|mut source| {
                        source.id = crate::memory::source_id();
                        template["data"]["source"]["id"] = json!(source.id);
                        source
                    })
            } else {
                None
            };
            let narrowed_cursor = (matches!(call.name.as_str(), "file_read" | "symbol_read")
                && pointer == "/data/content/text")
                .then(new_file_cursor_id);
            let (mut low, mut high) = (0, chars.len());
            let candidate = |length: usize| {
                let mut v = template.clone();
                *v.pointer_mut(pointer).unwrap() =
                    json!(chars[..length].iter().collect::<String>());
                let offset = args["offset"].as_u64().unwrap_or(0) + length as u64;
                if pointer == "/data/content/text"
                    && matches!(call.name.as_str(), "file_read" | "symbol_read")
                {
                    let line = template["data"]["read_start"].as_u64().unwrap();
                    let offset = template["data"]["read_offset"].as_u64().unwrap() + length as u64;
                    let shown = v["data"]["content"]["text"].as_str().unwrap();
                    let end = v["data"]["content"]["line_start"].as_u64().unwrap_or(line)
                        + shown.lines().count().saturating_sub(1) as u64;
                    let complete = if length < chars.len() {
                        shown.ends_with('\n') || (chars[length] == '\n' && !shown.ends_with('\r'))
                    } else {
                        template["data"]["content"]["last_line_complete"]
                            .as_bool()
                            .unwrap_or(false)
                    };
                    v["data"]["content"]["line_end"] = json!(end);
                    v["data"]["content"]["last_line_complete"] = json!(complete);
                    v["data"]["source"]["end_line"] = json!(end);
                    v["data"]["source"]["line_end_complete"] = json!(complete);
                    v["data"]["source"]["evidence_truncated"] = json!(false);
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
                    v["next_cursor"] = json!({"tool":"file_read","cursor":narrowed_cursor});
                    // Use the actual retained ID while measuring the candidate;
                    // its token cost can affect the final continuation offset.
                    if let Some(id) = existing_file_cursor(s, &file_cursor(&v)) {
                        v["next_cursor"]["cursor"] = json!(id);
                    }
                } else if pointer == "/data/content/text" && call.name == "document_inspect" {
                    let offset = n(&template["data"], "read_offset", 0) + length;
                    v["data"]["content"]["truncated"] = json!(true);
                    v["data"]["content"]["next_offset"] = json!(offset);
                    v["next_cursor"] = json!({"tool":"document_inspect","path":v["data"]["path"],"section":args["section"],"offset":offset,"expected_hash":v["data"]["hash"]});
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
                if narrowed_cursor.is_some() {
                    let id = output["next_cursor"]["cursor"].as_str().unwrap().into();
                    register_file_cursor(s, id, &output);
                }
                if let Some(mut source) = narrowed_source {
                    source.end_line = output["data"]["source"]["end_line"]
                        .as_u64()
                        .map(|n| n as usize);
                    source.excerpt = output["data"]["content"]["text"]
                        .as_str()
                        .unwrap()
                        .chars()
                        .take(2000)
                        .collect();
                    source.line_end_complete = output["data"]["content"]["last_line_complete"]
                        .as_bool()
                        .unwrap_or(false);
                    source.evidence_truncated = false;
                    // Re-reading the same oversized range yields the same view.
                    // Reuse its observation so a repeated read is not new evidence.
                    if let Some(existing) = s
                        .sources
                        .values()
                        .find(|existing| same_source(existing, &source))
                    {
                        output["data"]["source"]["id"] = json!(existing.id);
                    } else {
                        s.sources.insert(source.id.clone(), source);
                    }
                }
                return output;
            }
        }
    }
    fn adjust_collection_continuation(
        output: &mut Value,
        call: &crate::llm::ToolCall,
        args: &Value,
        field: &str,
        original_len: usize,
        retained_len: usize,
        archive: u64,
    ) {
        if retained_len >= original_len {
            return;
        }
        output["truncated"] = json!(true);
        let data = &mut output["data"];
        let start = args["cursor"]
            .as_str()
            .and_then(|cursor| cursor.rsplit_once(':'))
            .and_then(|(_, offset)| offset.parse::<usize>().ok())
            .or_else(|| args["offset"].as_u64().map(|offset| offset as usize))
            .unwrap_or(0);
        if field == "outline" {
            let next_offset = start + retained_len;
            data["next_offset"] = json!(next_offset);
            let mut cursor = json!({"tool":"document_inspect","offset":next_offset,"expected_hash":data["hash"]});
            for key in [
                "path",
                "limit",
                "coverage_offset",
                "expected_coverage_revision",
            ] {
                if let Some(value) = args.get(key) {
                    cursor[key] = value.clone();
                }
            }
            output["next_cursor"] = cursor;
            return;
        }
        if let Some(cursor) = data["next_cursor"].as_str()
            && let Some((prefix, _)) = cursor.rsplit_once(':')
        {
            data["next_cursor"] = json!(format!("{prefix}:{}", start + retained_len));
            return;
        }
        if call.name == "history"
            && args["action"] == "search"
            && let Some(id) = data[field]
                .as_array()
                .and_then(|items| items.last())
                .and_then(|item| item["id"].as_u64())
        {
            data["next_cursor"] = json!(id);
            return;
        }
        if data["next_offset"].is_u64() {
            data["next_offset"] = json!(start + retained_len);
            return;
        }
        // There is no native continuation for this shortened page. Expose the
        // immutable archive instead of silently dropping the removed items.
        output["archive_id"] = json!(archive);
        output["next_cursor"] = json!({"tool":"history","action":"read","id":archive,"offset":0});
    }

    // Collections stay structured; a shortened native page resumes at the
    // first omitted item, while non-paginated collections use the archive.
    for field in ["paths", "matches", "items", "outline"] {
        let original_len = output["data"][field].as_array().map_or(0, Vec::len);
        while output["data"][field]
            .as_array()
            .is_some_and(|a| !a.is_empty())
        {
            if result_tokens(call, &output, &s.config.model) <= limit {
                let retained_len = output["data"][field].as_array().unwrap().len();
                adjust_collection_continuation(
                    &mut output,
                    call,
                    &args,
                    field,
                    original_len,
                    retained_len,
                    archive,
                );
                return output;
            }
            output["data"][field].as_array_mut().unwrap().pop();
        }
    }
    let mut compact = json!({"status":result["status"],"truncated":true,"data":{"message":"Result retained in history; follow next_cursor."},"next_cursor":{"tool":"history","action":"read","id":archive,"offset":0}});
    // Mutating tools must keep the resulting document hash visible even when
    // their detailed payload is archived. The agent uses it to chain edits in
    // the same response, and callers need it to retry safely.
    if result["status"] == "ok"
        && matches!(call.name.as_str(), "document_edit" | "document_edit_batch")
        && result["data"]["hash"].is_string()
    {
        compact["data"] = json!({"hash":result["data"]["hash"]});
    }
    if call.name == "task_plan" && result["data"]["applied"] == true {
        // An applied mutation must remain distinguishable from an archived
        // read, even when long item text does not fit the result budget.
        compact["data"] = json!({
            "applied":true,
            "input_normalized":result["data"]["input_normalized"],
            "unchanged":result["data"]["unchanged"] == true,
            "plan":{
                "revision":result["data"]["plan"]["revision"],
                "pending_count":result["data"]["plan"]["pending_count"],
                "current_id":result["data"]["plan"]["current"]["id"]
            }
        });
    }
    if call.name == "task_plan" && result["data"]["applied"] == false {
        // The unchanged plan can be large. Keep the correction visible so
        // the model need not rediscover it through history during recovery.
        compact["data"] = json!({
            "applied":false,"plan":{"revision":result["data"]["plan"]["revision"]},
            "conflict":task_plan::compact_conflict(&result["data"]["conflict"]),
            "reason":context::truncate(result["data"]["reason"].as_str().unwrap_or("Plan unchanged"), 32, &s.config.model).0
        });
        if let Some(input) = result["data"].get("input_error") {
            compact["data"]["input_error"] = json!({
                "field":input["field"],"expected":input["expected"],"received":input["received"]
            });
        }
    }
    if let Some(data) = memory_tools::compact_data(&call.name, &result["data"]) {
        compact["data"] = data;
    }
    if let Some(data) = arguments::compact_data(&result["data"]) {
        compact["data"] = data;
    }
    if let Some(recovery) = result.get("recovery") {
        compact["recovery"] = recovery.clone();
        // Detailed recovery tools remain in the archive if the tiny result
        // budget cannot carry them, but the stable code/action must survive.
        if let Some(recovery) = compact["recovery"].as_object_mut() {
            recovery.remove("tools");
        }
    }
    if result["partial_success"].is_boolean() {
        compact["partial_success"] = result["partial_success"].clone();
    }
    // Archive fallback must not erase the cause of a failed operation.
    if let Some(error) = result["error"].as_str() {
        compact["error"] =
            json!(context::truncate(error, (limit / 3).clamp(8, 128), &s.config.model).0);
    }
    if result_tokens(call, &compact, &s.config.model) > limit
        && (compact["data"]["conflict"].is_object()
            || memory_tools::compact_data(&call.name, &result["data"]).is_some()
            || arguments::compact_data(&result["data"]).is_some())
    {
        // Structured causes take priority over duplicate prose at small
        // budgets. The full correction and examples remain in the archive.
        compact["data"].as_object_mut().unwrap().remove("reason");
        if let Some(error) = result["recovery"]["code"].as_str() {
            compact["error"] = json!(error);
        }
    }
    if result_tokens(call, &compact, &s.config.model) > limit {
        // Long enum/allowed-field lists belong in the archive, but retain the
        // offending path and execution state before discarding the diagnosis.
        if arguments::compact_data(&result["data"]).is_some()
            && let Some(input) = compact["data"]["input_error"].as_object_mut()
        {
            input.remove("expected");
        }
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
    let mut result = guard_tool(s, |s| run_call_inner(s, call, cancel));
    recovery::attach(s, call, &mut result);
    if result["status"] != "ok"
        && let Some(cp) = &mut s.checkpoint
    {
        cp.failed = true;
        cp.acknowledged = false;
    }
    limit_result(s, call, result, s.config.result_tokens)
}

fn guard_tool(s: &mut Session, operation: impl FnOnce(&mut Session) -> Value) -> Value {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(s))) {
        Ok(result) => result,
        Err(_) => {
            // Preserve state and completed writes; a panic is not a rollback.
            if let Some(cp) = &mut s.checkpoint {
                cp.failed = true;
                cp.acknowledged = false;
            }
            envelope(Err(anyhow::anyhow!(
                "tool_worker_panic: execution interrupted; retained state and write outcomes require review"
            )))
        }
    }
}

fn run_call_inner(
    s: &mut Session,
    call: &crate::llm::ToolCall,
    cancel: &tokio_util::sync::CancellationToken,
) -> Value {
    // OpenAI's stream parser rejects missing IDs and names, but alternate
    // LlmClient implementations can construct ToolCall values directly. Do
    // not let an invalid ID enter the ledger or execute a mutation: an empty
    // key would make unrelated calls share one idempotency slot.
    if call.id.trim().is_empty() || call.name.trim().is_empty() {
        return envelope(Err(anyhow::anyhow!(
            "malformed_tool_call: tool call id and name must be non-empty"
        )));
    }
    if call.id.len() > crate::llm::MAX_TOOL_CALL_ID_BYTES
        || call.name.len() > crate::llm::MAX_TOOL_NAME_BYTES
    {
        return envelope(Err(anyhow::anyhow!(
            "malformed_tool_call: call ID or name is too long"
        )));
    }
    if call.arguments.len() > crate::llm::MAX_COMPLETION_BYTES {
        return envelope(Err(anyhow::anyhow!("response_size_limit")));
    }
    let signature = format!("{}:{}", call.name, call.arguments);
    if let Some((stored, result)) = s.ledger.get(&call.id) {
        return if stored == &signature {
            result.clone()
        } else {
            envelope(Err(anyhow::anyhow!(
                "call_id_collision: call ID {:?} was already used for a different {} call in this session; this call was not executed. Resend it with a new unique call ID",
                call.id.chars().take(80).collect::<String>(),
                stored.split(':').next().unwrap_or("tool")
            )))
        };
    }
    // An empty argument string is a call with no arguments, as in llm.rs.
    let arguments = if call.arguments.trim().is_empty() {
        "{}"
    } else {
        call.arguments.as_str()
    };
    let result = serde_json::from_str(arguments)
        .map_err(|e| recovery::DiagnosticError {
            message: format!("invalid_tool_arguments: {e}; send one complete JSON object with double-quoted property names; no tool operation was executed"),
            data: json!({"execution":"not_started","input_error":{
                "tool":call.name,"field":"arguments","expected":"complete JSON object",
                "received":"invalid JSON","line":e.line(),"column":e.column()
            }}),
        }.into())
        .and_then(|args| execute_cancellable(s, &call.name, args, cancel));
    let result = envelope(result);
    // Attach recovery before the first archival pass so history retains the
    // complete diagnosis, including failed batch items and available remedies.
    if result["status"] != "ok" {
        return result;
    }
    let unapplied_plan = call.name == "task_plan" && result["data"]["applied"] == false;
    let output = limit_result(s, call, result, s.config.result_tokens);
    // Failed mutations are not cached, allowing deliberate recovery with corrected arguments.
    if output["status"] == "ok" && !unapplied_plan {
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
        hasher.update(read_bytes_bounded(&path)?);
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Confirm the last successful edit still exists unchanged. This is a persistence
/// check, not a semantic or source-evidence attestation.
pub fn verify_document_write(s: &Session) -> Result<()> {
    if let Some((path, expected)) = &s.last_document_write {
        let actual = read_text(path).map_err(|error| {
            anyhow::anyhow!(
                "document_write_verification_failed: {}: {error}",
                path.display()
            )
        })?;
        if hash(actual.as_bytes()) != *expected {
            bail!(
                "document_changed_after_write: inspect the saved document and reconcile changes before completing"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod panic_tests {
    use super::*;

    #[test]
    fn worker_panic_preserves_completed_write_and_invalidates_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(
            Project {
                root: dir.path().into(),
                ..Default::default()
            },
            Default::default(),
        );
        s.checkpoint = Some(crate::session::Checkpoint {
            id: "checkpoint".into(),
            bundle_ids: vec![],
            maintenance_bundle_ids: vec![],
            acknowledged: true,
            attempts: 1,
            failed_attempts: 0,
            last_failure: None,
            source_lookup_calls: 0,
            starting_state_revision: 0,
            starting_memory_generation: 0,
            failed: false,
        });
        let path = dir.path().join("saved.md");
        let result = guard_tool(&mut s, |s| {
            std::fs::write(&path, "saved before panic").unwrap();
            s.last_document_write = Some((path.clone(), hash(b"saved before panic")));
            panic!("injected failure after write");
        });
        assert_eq!(result["status"], "error");
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .starts_with("tool_worker_panic")
        );
        assert!(s.checkpoint.as_ref().unwrap().failed);
        assert!(!s.checkpoint.as_ref().unwrap().acknowledged);
        assert!(verify_document_write(&s).is_ok());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "saved before panic");
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;

    #[test]
    fn file_discovery_bounds_count_and_path_bytes_after_applying_scope_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let kept = root.join("kept");
        std::fs::create_dir(&kept).unwrap();
        for name in ["a.rs", "b.rs"] {
            std::fs::write(kept.join(name), "fn observed() {}\n").unwrap();
        }
        for index in 0..32 {
            std::fs::write(root.join(format!("other_{index}.txt")), "unrelated\n").unwrap();
        }
        let project = Project {
            root,
            ..Default::default()
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let discover = |pattern, scope, count, bytes| {
            candidate_paths_bounded(
                &project,
                pattern,
                &cancel,
                scope,
                None,
                FileScanLimits {
                    max_paths: count,
                    max_path_bytes: bytes,
                },
            )
        };
        let expected = vec![kept.join("a.rs"), kept.join("b.rs")];
        let bytes: usize = expected
            .iter()
            .map(|p| p.as_os_str().as_encoded_bytes().len())
            .sum();
        // A narrow scope must fit even when the rest of the project does not.
        assert_eq!(
            discover(None, Some(kept.as_path()), 2, bytes).unwrap(),
            expected
        );
        assert_eq!(discover(Some("**/*.rs"), None, 2, bytes).unwrap(), expected);
        for result in [
            discover(None, None, 2, usize::MAX),
            discover(None, Some(kept.as_path()), 1, usize::MAX),
            discover(None, Some(kept.as_path()), 2, bytes - 1),
        ] {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .starts_with("file_scan_capacity:")
            );
        }
        cancel.cancel();
        assert_eq!(
            discover(None, None, 2, bytes).unwrap_err().to_string(),
            "cancelled"
        );
    }

    #[test]
    fn streaming_file_hash_matches_text_and_binary_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        for bytes in [
            vec![],
            "한글🙂\r\nplain text\n".as_bytes().to_vec(),
            vec![0, 0xff, 0xfe],
            vec![b'x'; 128 * 1024 + 3],
        ] {
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(hash_file(&path).unwrap(), hash(&bytes));
        }
        assert!(hash_file(dir.path()).is_err());
    }

    #[test]
    fn streaming_hash_bounds_the_actual_read_after_metadata_lookup() {
        use std::io::Read;
        let within_limit = std::io::repeat(0).take(MAX_FILE_BYTES as u64);
        assert!(hash_reader(within_limit).is_ok());
        let error = hash_reader(std::io::repeat(0)).unwrap_err();
        assert!(error.to_string().starts_with("unsupported_large_file:"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("too-large");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_FILE_BYTES as u64 + 1)
            .unwrap();
        assert!(hash_file(&path).is_err());
    }

    #[test]
    fn preview_preserves_valid_text_and_rejects_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("text.md");
        for text in ["", "hello", "한글🙂", "one\r\ntwo\n"] {
            std::fs::write(&path, text).unwrap();
            for limit in 0..=text.len() + 1 {
                let (preview, truncated) = read_text_preview(&path, limit).unwrap();
                assert!(text.starts_with(&preview));
                assert!(preview.len() <= limit);
                assert_eq!(truncated, text.len() > limit);
                if !truncated {
                    assert_eq!(preview, text);
                }
            }
        }
        for bytes in [vec![0xff], vec![0xe3, 0x81]] {
            std::fs::write(&path, bytes).unwrap();
            assert!(read_text_preview(&path, 100).is_err());
        }
    }

    #[test]
    fn text_reader_rejects_directories_and_oversized_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            read_text(dir.path())
                .unwrap_err()
                .to_string()
                .contains("path_is_directory")
        );
        let path = dir.path().join("large.md");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(16 * 1024 * 1024 + 1)
            .unwrap();
        assert!(
            read_text(&path)
                .unwrap_err()
                .to_string()
                .contains("unsupported_large_file")
        );
    }

    #[test]
    fn missing_file_after_path_check_keeps_path_and_recovery_code() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vanished.md");
        let error = file_access_error(&path, std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(error.to_string().starts_with("file_not_found:"));
        assert!(error.to_string().contains(&path.display().to_string()));
    }
}
