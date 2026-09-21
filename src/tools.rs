pub mod answer_review;
mod coverage;
pub mod document_review;
mod documentation;
pub mod recovery;
mod search;
mod structure;
use crate::{
    config::Project,
    context::{self},
    memory::{MemoryInput, Source},
    session::{Investigation, Session, TaskState},
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
            "metadata":{"type":"object"},
            "expected_revision":number()
        }),
        &["title", "summary", "body", "kind"],
    )
}
fn action(values: &[&str]) -> Value {
    json!({"type":"string","enum":values})
}
pub struct ToolRegistry;
fn task_patch_schema() -> Value {
    let mut properties = serde_json::Map::new();
    for field in ["purpose", "scope", "current", "next"] {
        properties.insert(field.into(), string());
    }
    for field in [
        "deliverables",
        "constraints",
        "completion",
        "done",
        "findings",
        "unresolved",
        "memory_ids",
    ] {
        properties.insert(field.into(), strings());
    }
    properties.insert("details".into(), json!({"type":"array","items":{}}));
    properties.insert("require_investigation".into(), json!({"type":"boolean"}));
    properties.insert(
        "workflow".into(),
        action(&["answer", "source_document", "document_edit"]),
    );
    properties.insert(
        "phase".into(),
        action(&["investigate", "draft", "verify", "answer"]),
    );
    json!({"type":"object","properties":properties,"additionalProperties":false})
}

impl ToolRegistry {
    pub fn specs() -> Vec<ToolSpec> {
        let mut specs = vec![
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
                description: "Save one reusable memory. Same key requires expected_revision. Updates replace evidence: resupply valid source_ids for observed facts; existing sources are not inherited. Copy source_ids exactly from tool results; never omit them to recover from unknown_source. Facts without sources, inferred memories and memories whose file evidence changed are needs_review. Source IDs must come from program observations. Metadata has no per-entry token rejection; keep it concise because memory index context still consumes index_tokens. Oversized entries may be omitted from state and remain available through memory_find/memory_read. Put details in body. kind: fact/decision/failure/question/procedure",
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
                description: "Search metadata by key/tag/keywords, or list all with empty query; use next cursor until exhausted. A cursor is bound to the memory generation and the exact query/tag filters and expires if either changes",
                optional: false,
                read_only: true,
                parameters: schema(
                    json!({"query":string(),"tags":strings(),"cursor":string(),"limit":number()}),
                    &[],
                ),
            },
            ToolSpec {
                name: "memory_manage",
                description: "List cleanup candidates, delete unreferenced memories, or atomically replace IDs and redirect references. delete requires ids; replace requires ids and replacement; duplicate IDs are ignored. replacement uses memory_write fields and must use a new key or a key among the replaced IDs; when it reuses a replaced key, expected_revision and observed-source rules are checked before removal",
                optional: false,
                read_only: false,
                parameters: schema(
                    json!({"action":action(&["candidates","delete","replace"]),"ids":strings(),"replacement":memory_input_schema()}),
                    &["action"],
                ),
            },
            ToolSpec {
                name: "task_state",
                description: "Read/update structured goals and compact progress, or read/write detailed work list. State fields belong inside patch, e.g. {action:update,patch:{phase:verify}}; phase is not a top-level argument. For source documentation START with patch.workflow=source_document; this locks evidence requirements and activates investigation/document_edit/document_audit immediately. Use investigation upsert for investigation items, NOT task_state. Do not send empty patches. Updates preserve omitted fields. Set patch.require_investigation=true BEFORE source-documentation work requiring evidence coverage; simple document edits do not need it. Once required, it cannot be disabled during the same request. Preserve user constraints unless explicitly changed by user",
                optional: false,
                read_only: false,
                parameters: schema(
                    json!({"action":action(&["read","update","details"]),"patch":task_patch_schema(),"offset":number(),"limit":number()}),
                    &["action"],
                ),
            },
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
                description: "List project files. Default mode=text validates UTF-8 text; mode=paths lists regular file paths without reading contents (may include binary/large files). path_glob is a file glob such as backend/**/*.js (pattern is a legacy alias). Paginated; respects project boundaries and exclusions",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"path_glob":string(),"pattern":string(),"cursor":string(),"limit":number(),"mode":action(&["text","paths"])}),
                    &[],
                ),
            },
            ToolSpec {
                name: "source_search",
                description: "Search source lines: query is literal text by default. Prefer queries:[\"agent\",\"run\",\"db\"] for literal OR without regex escaping. Supply exactly one of query or queries. Use regex:true only for intentional regular expressions; punctuation such as .on( is literal unless regex:true. case_sensitive defaults true; whole_word defaults false (Unicode word boundaries). path selects one exact file (no glob syntax); path_glob filters multiple files (pattern is a legacy alias). Do not combine path with path_glob or pattern. mode=matches (default) returns matching lines and source IDs; files returns matching paths; count returns matching-line counts per file. before/after add up to 20 context lines each in matches mode. Each displayed line is capped at 500 characters with truncation marked. Reuse the same search options with cursor for pagination; limit may change. Hashes detect source changes",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"query":string(),"queries":strings(),"path":string(),"regex":{"type":"boolean"},"case_sensitive":{"type":"boolean"},"whole_word":{"type":"boolean"},"mode":action(&["matches","files","count"]),"before":{"type":"integer","minimum":0,"maximum":20},"after":{"type":"integer","minimum":0,"maximum":20},"path_glob":string(),"pattern":string(),"cursor":string(),"limit":number()}),
                    &[],
                ),
            },
            ToolSpec {
                name: "document_inspect",
                description: "Inspect a document using path (relative to project.root); omit path for project.output. Uses file_read path restrictions. Returns session delivery coverage for the current hash, not proof of understanding or current context retention. coverage_offset pages missing line ranges; copy coverage.revision as expected_coverage_revision on continuation to detect intervening reads. With no section, offset is an OUTLINE HEADING INDEX, not a document line number; copy next_offset from the prior outline page. With section, offset is a CHARACTER INDEX inside that section; copy content.next_offset or the returned next_cursor arguments. For a document line number use file_read with start_line. section accepts a full Markdown heading or a unique title without #; duplicate titles require disambiguation. For any offset or coverage_offset > 0, copy expected_hash from the first result's hash; if unavailable, restart at offset 0 with coverage_offset 0.",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"path":string(),"section":string(),"offset":number(),"limit":number(),"coverage_offset":number(),"expected_hash":string(),"expected_coverage_revision":string()}),
                    &[],
                ),
            },
            ToolSpec {
                name: "symbol_search",
                description: "Heuristic declaration search for JS/TS, Rust and Python. query is a symbol-name substring, e.g. handleQuestion; path_glob is a file glob, e.g. backend/src/agent.js (pattern is a legacy alias). For declaration regex use source_search instead. Returns matched_files/scanned_files to distinguish no files from no symbols. Cursor expires on source changes.",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"query":string(),"path_glob":string(),"pattern":string(),"cursor":string(),"limit":number()}),
                    &[],
                ),
            },
            ToolSpec {
                name: "code_outline",
                description: "Explore one Rust, JS/JSX, TS/TSX, Python, Java or C# file. For a function list use {path,view:\"compact\",max_depth:0,kind:\"function\"}; use kind:\"method\" with container and without max_depth:0 for class methods. Omit kind only for a mixed overview. Find a name with {path,query:\"handleQuestion\",match:\"exact\",kind:\"function\"}. query defaults to case-insensitive substring matching. kind filters normalized symbol_kind; container is an exact enclosing path: copy the parent class qualified_name, e.g. Store or Outer::Inner, never convert :: to Java or C# member dots. qualified_name is navigation, not a unique ID for overloaded methods. max_depth uses symbol nesting (0=top level, 1=direct members), not AST depth. Defaults: all kinds/depths, view=detailed. Compact returns names, kinds, containers, positions, a ready-to-copy location and symbol IDs, without source evidence. For parameters, defaults and declared return types use view=detailed with query and match=exact; an untruncated signature often answers without a body read. Detailed includes signature_source and signature_truncated; if truncated, read only the missing signature lines. A signature does not establish runtime behavior. Copy symbol_id into symbol_read for the body. Follow next_cursor preserving ALL filters and view; limit may change. Edits expire cursors/IDs. Positions are 1-based Unicode. Syntax structure does not resolve semantic references.",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"path":string(),"query":string(),"match":action(&["contains","exact"]),"case_sensitive":{"type":"boolean"},"kind":action(&["function","method","constructor","class","struct","interface","trait","impl","enum","enum_member","record","annotation","field","variable","constant","type","module","macro","property","accessor","event","delegate","operator","destructor"]),"container":{"type":"string","description":"Exact enclosing symbol path, case-sensitive. Empty string selects top-level symbols."},"max_depth":{"type":"integer","minimum":0,"description":"Maximum symbol nesting depth; 0 selects top-level declarations."},"view":action(&["compact","detailed"]),"cursor":string(),"limit":number()}),
                    &["path"],
                ),
            },
            ToolSpec {
                name: "symbol_read",
                description: "Read implementation only when the requested fact is missing from code_outline's detailed signature. Use {path,symbol_id,max_lines:30} for the start of a function; do not read a whole long function just to list arguments. start_line is an optional ABSOLUTE file line within the symbol (default symbol.start_line), max_lines is a count from 1 to 2000, capped at the symbol end. Omit both to read the whole symbol subject to file_read limits. Copy exact symbol_id from code_outline; stale IDs are rejected. If text is truncated, finish that requested range with file_read cursor before starting another range at next_line, at most symbol.end_line. Sources and coverage attest only delivered text. Shared declaration lines can include neighboring declarations.",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"path":string(),"symbol_id":string(),"start_line":{"type":"integer","minimum":1,"description":"Absolute file line inside the symbol, NOT a relative offset. Omit to start at the symbol's first line."},"max_lines":{"type":"integer","minimum":1,"maximum":2000,"description":"Requested line count; prefer a small range such as 30 for a function prologue."},"force_read":{"type":"boolean"}}),
                    &["path", "symbol_id"],
                ),
            },
            ToolSpec {
                name: "document_audit",
                description: "Check output citations path:line[-line], source freshness, section coverage and pending investigations in one call. Structural checks do NOT prove semantic correctness. Paginated issues; the first result returns revision, and offset > 0 requires expected_revision copied from that result. Restart from offset 0 when the revision changes.",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"offset":number(),"limit":number(),"expected_revision":string()}),
                    &[],
                ),
            },
            ToolSpec {
                name: "file_read",
                description: "Read a new range with path, 1-based start_line and max_lines (line count). For a targeted source question, first locate the identifier/route with source_search or code_outline, then supply an explicit range; do not start with default first-page reads of every file. limit is accepted as a compatibility alias for max_lines; prefer max_lines. Do not use offset as a line number. Example: {path:\"src/agent.rs\",start_line:160,max_lines:140}. Relative paths resolve against project.root, NEVER the output directory or workspace parent. For project.output outside the project, copy the absolute path returned by document_inspect; do not shorten it to a basename. Example: root=/workspace/app and output=/workspace/app_summary.md requires path=/workspace/app_summary.md, not app_summary.md. Use document_inspect to read the configured output without supplying a path. If truncated, continue ONLY with {cursor: next_cursor.cursor}; never combine cursor with path/start_line/max_lines/offset. A cursor completes the original requested range and expires if the file changes. Once that range is complete, next_line indicates where a NEW range can start. Returned line_start/line_end describe delivered text; boundary flags mark partial lines. Use force_read=true only for deliberate repeat verification.",
                optional: true,
                read_only: true,
                parameters: schema(
                    json!({"path":{"type":"string","description":"File path: relative to project.root, or the absolute configured output path returned by document_inspect. Never infer the output path from its basename."},"cursor":string(),"start_line":number(),"max_lines":number(),"limit":{"type":"integer","minimum":0,"description":"Compatibility alias for max_lines (number of lines). Prefer max_lines; never use a different value alongside max_lines."},"offset":number(),"force_read":{"type":"boolean"}}),
                    &[],
                ),
            },
            ToolSpec {
                name: "document_edit",
                description: "Edit ONLY configured Markdown output: create, replace entire file, append, or unique exact text patch. Existing file requires expected_hash. section accepts a full heading or a unique title without # and also requires expected_section_hash. Replacement text must retain the original full Markdown heading including #. Simple edits do not require investigation items; source documentation must first set task_state patch.require_investigation=true. Returns measured lines and new hash",
                optional: true,
                read_only: false,
                parameters: schema(
                    json!({"action":action(&["create","write","append","patch","section"]),"text":string(),"old_text":string(),"expected_hash":string(),"section":string(),"expected_section_hash":string()}),
                    &["action", "text"],
                ),
            },
            ToolSpec {
                name: "investigation",
                description: "Manage source documentation items. upsert creates or updates ONE item per call: new items require title; when id identifies an existing item, omitted title is preserved. Optional id/status/memory_ids/source_ids/section; items and verification_note are NOT accepted. To register several items, issue separate upsert calls. verify requires id, source_ids and verification_note. Both verify and verify_batch require existing written items. If not written, write the section and upsert with status=written and section first; source IDs alone do not mark an item written. list accepts only offset/limit; final_check accepts no other arguments. Only verify_batch accepts items; it verifies existing written items, never creates them. verify_batch items is an object keyed by item ID, each value {source_ids:[...],verification_note:string}; each is independently verified; summary groups failures by code and retry_ids identifies only failed items. Already verified items in verify_batch reuse their existing evidence after section/source/memory freshness checks; new supplied evidence is ignored for those items. Use single verify to explicitly replace evidence. After edits, verify only verification_required_ids returned by document_edit. Coverage failures return all missing_ranges together. status uninvestigated/in_progress/written; verify compares document with source IDs and requires verification_note. status=written requires a non-empty section (supplied now or preserved from the existing item). For written items, upsert checks the current document and normalizes section to its full heading; a unique title without # is accepted, including numbering. Planned sections may be registered before writing with status=in_progress",
                optional: true,
                read_only: false,
                parameters: schema(
                    json!({"action":action(&["list","upsert","verify","verify_batch","final_check"]),"id":{"type":"string","minLength":1},"title":{"type":"string","description":"Non-empty title required for a NEW item. Omit when updating an existing id to preserve its title."},"status":action(&["uninvestigated","in_progress","written"]),"memory_ids":strings(),"source_ids":strings(),"section":string(),"verification_note":string(),"items":{"type":"object","description":"ONLY for action=verify_batch. Object keyed by existing investigation IDs; not an array and not used by upsert.","minProperties":1,"maxProperties":20,"additionalProperties":{"type":"object","properties":{"source_ids":strings(),"verification_note":string()},"required":["source_ids","verification_note"],"additionalProperties":false}},"offset":number(),"limit":number()}),
                    &["action"],
                ),
            },
        ];
        let spec = specs
            .iter_mut()
            .find(|spec| spec.name == "investigation")
            .unwrap();
        let fields = spec.parameters["properties"].as_object().unwrap();
        let branches: Vec<Value> = ["list", "upsert", "verify", "verify_batch", "final_check"]
            .into_iter()
            .map(|name| {
                let (allowed, required, _) = investigation_contract(name, true).unwrap();
                let mut properties = serde_json::Map::new();
                for key in allowed {
                    properties.insert((*key).into(), fields[*key].clone());
                }
                properties.insert("action".into(), json!({"const":name}));
                let mut required = required.to_vec();
                required.push("action");
                json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
            }).collect();
        spec.parameters["oneOf"] = json!(branches);
        specs
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
            "source_lookup",
        ]
        .contains(&name)
    }
    pub fn definitions(s: &Session) -> Vec<Value> {
        Self::specs().into_iter()
            .filter(|t| !t.optional || s.active_tools.contains(t.name))
            .filter(|t| s.config.memory_reuse || !["memory_read", "memory_find"].contains(&t.name))
            .filter(|t| s.checkpoint.is_none() || Self::checkpoint_allowed(t.name))
            .map(|mut t| {
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
                        {"type":"object","properties":{"action":{"enum":["create","write"]},"text":fields["text"],"expected_hash":fields["expected_hash"]},"additionalProperties":false},
                        {"type":"object","properties":{"action":{"const":"append"},"text":fields["text"],"expected_hash":fields["expected_hash"]},"required":["expected_hash"],"additionalProperties":false},
                        {"type":"object","properties":{"action":{"const":"patch"},"text":fields["text"],"expected_hash":fields["expected_hash"],"old_text":fields["old_text"]},"required":["expected_hash","old_text"],"additionalProperties":false},
                        {"type":"object","properties":{"action":{"const":"section"},"text":fields["text"],"expected_hash":fields["expected_hash"],"section":fields["section"],"expected_section_hash":fields["expected_section_hash"]},"required":["expected_hash","section","expected_section_hash"],"additionalProperties":false}
                    ]);
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
                json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.parameters}})
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
        let object = args.as_object().ok_or_else(|| {
            anyhow::anyhow!("invalid_tool_arguments: arguments must be an object")
        })?;
        let fields = spec.parameters["properties"].as_object().unwrap();
        if name == "investigation" {
            validate_investigation_arguments(s, args)?;
        }
        for key in object.keys() {
            if !fields.contains_key(key) {
                if name == "task_state" && task_patch_schema()["properties"].get(key).is_some() {
                    bail!(
                        "unknown_argument: {key} belongs inside task_state patch; use {{\"action\":\"update\",\"patch\":{{\"{key}\":...}}}}; state unchanged"
                    );
                }
                bail!(
                    "unknown_argument: {key} for {name}; allowed arguments: {}",
                    fields.keys().cloned().collect::<Vec<_>>().join(", ")
                );
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
        if name == "task_state" {
            validate_task_state_arguments(args)?;
        } else if name == "document_edit" {
            validate_document_edit_arguments(args)?;
        } else if name == "memory_manage" {
            validate_memory_manage_arguments(args)?;
        } else if name == "history" {
            validate_history_arguments(args)?;
        }
        Ok(spec)
    }
}

fn validate_task_state_arguments(args: &Value) -> Result<()> {
    let action = args["action"].as_str().unwrap_or("");
    validate_action_fields(
        "task_state",
        args,
        match action {
            "read" => &["action"][..],
            "details" => &["action", "offset", "limit"][..],
            "update" => &["action", "patch"][..],
            _ => &["action"][..],
        },
    )?;
    if action != "update" {
        return Ok(());
    }
    let Some(patch) = args.get("patch") else {
        bail!("missing_argument: patch for task_state action=update");
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
                bail!("invalid_argument_value: revision is program-owned");
            }
            bail!(
                "unknown_argument: {key} belongs inside task_state patch schema; state unchanged"
            );
        };
        let valid = match field["type"].as_str() {
            Some("string") => value.is_string(),
            Some("boolean") => value.is_boolean(),
            Some("array") => value.as_array().is_some_and(|items| {
                field["items"]["type"] != "string" || items.iter().all(Value::is_string)
            }),
            Some("object") => value.is_object(),
            _ => true,
        };
        if !valid {
            bail!("invalid_argument_type: patch.{key}");
        }
        if let Some(values) = field["enum"].as_array()
            && !values.contains(value)
        {
            bail!("invalid_argument_value: patch.{key}");
        }
    }
    Ok(())
}

fn validate_document_edit_arguments(args: &Value) -> Result<()> {
    let action = args["action"].as_str().unwrap_or("");
    validate_action_fields(
        "document_edit",
        args,
        match action {
            "create" | "write" | "append" => &["action", "text", "expected_hash"][..],
            "patch" => &["action", "text", "expected_hash", "old_text"][..],
            "section" => &[
                "action",
                "text",
                "expected_hash",
                "section",
                "expected_section_hash",
            ][..],
            _ => &["action", "text"][..],
        },
    )?;
    let require = |key: &str| {
        if args.get(key).is_none() {
            return Err(anyhow::anyhow!(
                "missing_argument: {key} for document_edit action={action}"
            ));
        }
        if args[key]
            .as_str()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(anyhow::anyhow!(
                "invalid_argument_value: {key} must not be empty for document_edit action={action}"
            ));
        }
        Ok(())
    };
    match action {
        "append" => require("expected_hash")?,
        "patch" => {
            require("expected_hash")?;
            require("old_text")?;
        }
        "section" => {
            require("expected_hash")?;
            require("section")?;
            require("expected_section_hash")?;
        }
        "create" | "write" => {}
        _ => {}
    }
    Ok(())
}

fn validate_memory_manage_arguments(args: &Value) -> Result<()> {
    let action = args["action"].as_str().unwrap_or("");
    validate_action_fields(
        "memory_manage",
        args,
        match action {
            "candidates" => &["action"][..],
            "delete" => &["action", "ids"][..],
            "replace" => &["action", "ids", "replacement"][..],
            _ => &["action"][..],
        },
    )?;
    if matches!(action, "delete" | "replace") && args["ids"].as_array().is_none_or(Vec::is_empty) {
        bail!("missing_argument: ids for memory_manage action={action}");
    }
    if action == "replace" && args.get("replacement").is_none() {
        bail!("missing_argument: replacement for memory_manage action=replace");
    }
    Ok(())
}

fn validate_history_arguments(args: &Value) -> Result<()> {
    let action = args["action"].as_str().unwrap_or("");
    validate_action_fields(
        "history",
        args,
        match action {
            "search" => &["action", "query", "after", "limit"][..],
            "read" => &["action", "id", "offset"][..],
            _ => &["action"][..],
        },
    )?;
    if args["action"] == "read" && args.get("id").is_none() {
        bail!("missing_argument: id for history action=read");
    }
    Ok(())
}

fn validate_action_fields(name: &str, args: &Value, allowed: &[&str]) -> Result<()> {
    let object = args
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("invalid_tool_arguments: arguments must be an object"))?;
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        let action = args["action"].as_str().unwrap_or("");
        bail!(
            "invalid_action_arguments: {name} action={action} does not accept {key}; allowed: {}",
            allowed.join(", ")
        );
    }
    Ok(())
}
type InvestigationContract = (
    &'static [&'static str],
    &'static [&'static str],
    &'static str,
);
fn investigation_contract(action: &str, updating: bool) -> Option<InvestigationContract> {
    Some(match action {
        "upsert" => (
            &[
                "action",
                "id",
                "title",
                "status",
                "memory_ids",
                "source_ids",
                "section",
            ],
            if updating { &[] } else { &["title"] },
            r##"{"action":"upsert","id":"overview","title":"Project overview","section":"# Overview"}"##,
        ),
        "verify" => (
            &["action", "id", "source_ids", "verification_note"],
            &["id", "source_ids", "verification_note"],
            r#"{"action":"verify","id":"existing-id","source_ids":["observed-source-id"],"verification_note":"Actual source/document comparison"}"#,
        ),
        "verify_batch" => (
            &["action", "items"],
            &["items"],
            r#"{"action":"verify_batch","items":{"existing-id":{"source_ids":["observed-source-id"],"verification_note":"Actual comparison"}}}"#,
        ),
        "list" => (
            &["action", "offset", "limit"],
            &[],
            r#"{"action":"list","offset":0,"limit":20}"#,
        ),
        "final_check" => (&["action"], &[], r#"{"action":"final_check"}"#),
        _ => return None,
    })
}

// Enforce the advertised action contract even when providers do not validate
// conditional JSON Schema. New-item title checks additionally require state.
fn validate_investigation_arguments(s: &Session, args: &Value) -> Result<()> {
    let action = args["action"].as_str().unwrap_or("");
    let updating = args["id"]
        .as_str()
        .is_some_and(|id| s.investigations.iter().any(|item| item.id == id));
    let Some((allowed, required, example)) = investigation_contract(action, updating) else {
        return Ok(());
    };
    if action == "upsert" && args.get("items").is_some() {
        bail!(
            "invalid_action_arguments: investigation upsert handles ONE item per call; new items require top-level title. items is only for verify_batch of existing written items. Issue separate upsert calls. Example: {example}"
        );
    }
    for key in args.as_object().unwrap().keys() {
        if !allowed.contains(&key.as_str()) {
            bail!(
                "invalid_action_arguments: investigation action={action} does not accept {key}; allowed: {}. Example: {example}",
                allowed.join(", ")
            );
        }
    }
    for key in required {
        if args.get(*key).is_none() {
            bail!("missing_argument: {key} for investigation action={action}. Example: {example}");
        }
        if args[*key]
            .as_str()
            .is_some_and(|value| value.trim().is_empty())
        {
            bail!(
                "invalid_argument_value: {key} must not be empty for investigation action={action}. Example: {example}"
            );
        }
    }
    if action == "verify_batch" && !args["items"].is_object() {
        bail!(
            "invalid_argument_type: items for verify_batch must be an object keyed by existing item IDs, not an array. Example: {example}"
        );
    }
    if action == "upsert"
        && args["title"]
            .as_str()
            .is_some_and(|title| title.trim().is_empty())
    {
        bail!("invalid_argument_value: title must not be empty for investigation action=upsert");
    }
    Ok(())
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
fn n(args: &Value, key: &str, default: usize) -> usize {
    args[key].as_u64().map_or(default, |n| n as usize)
}

// Some compatible tool parsers quote unsigned integer arguments. Normalize
// them once at the execution boundary and again anywhere a raw call is used to
// rebuild a continuation, so both paths use the same typed arguments.
fn normalize_integer_arguments(name: &str, args: &mut Value) {
    if let Some(spec) = ToolRegistry::specs().into_iter().find(|t| t.name == name)
        && let Some(fields) = args.as_object_mut()
    {
        for (key, value) in &mut *fields {
            if spec.parameters["properties"][key]["type"] == "integer"
                && let Some(raw) = value.as_str()
                && !raw.is_empty()
                && raw.bytes().all(|b| b.is_ascii_digit())
                && let Ok(number) = raw.parse::<u64>()
            {
                *value = json!(number);
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
                    .filter(|item| {
                        item["status"] == "error"
                            || item["result"]["status"] == "error"
                            || item["result"]["status"] == "cancelled"
                            || item["result"]["status"] == "unsupported"
                    })
                    .count()
            });
            if failed > 0 {
                let message = format!(
                    "batch_partial_failure: {failed} items failed; inspect results and retry only failed items"
                );
                json!({"status":"error","error":message,"recovery":recovery::describe(&message),"partial_success":true,"data":data,"truncated":false,"next_cursor":null})
            } else {
                json!({"status":"ok","data":data,"truncated":false,"next_cursor":null})
            }
        }
        Err(error) => {
            let message = error.to_string();
            let status = if message == "cancelled" || message.starts_with("cancelled:") {
                "cancelled"
            } else if message.starts_with("unsupported") {
                "unsupported"
            } else {
                "error"
            };
            let mut result = json!({"status":status,"error":message,"recovery":recovery::describe(&message),"truncated":false,"next_cursor":null});
            if let Some(coverage) = error.downcast_ref::<documentation::CoverageMissing>() {
                result["data"] = json!({"item_id":coverage.item_id,"missing_ranges":coverage.missing_ranges,"missing_range_count":coverage.missing_ranges.len()});
            }
            result
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
    if path.exists() && std::fs::metadata(&path)?.is_dir() {
        bail!("output_path_is_directory: configured output must be a file");
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
    let canonical = candidate.canonicalize().map_err(|e| {
        let code = match e.kind() {
            std::io::ErrorKind::NotFound => "file_not_found",
            std::io::ErrorKind::PermissionDenied => "file_permission_denied",
            _ => "file_access_error",
        };
        anyhow::anyhow!("{code}: resolved path {}; project root {}; configured output {}. Relative paths use project.root; use document_inspect with no path for configured output. {e}", candidate.display(), root.display(), p.output.display())
    })?;
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
fn regular_metadata(metadata: &std::fs::Metadata, path: &Path) -> Result<()> {
    if metadata.is_dir() {
        bail!(
            "path_is_directory: {} is a directory; use file_list with path_glob (e.g. backend/**), then file_read with a file path",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!("unsupported_file_type: expected a regular text file");
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
fn open_regular_file(path: &Path) -> Result<std::fs::File> {
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
    if file.metadata()?.len() > MAX_FILE_BYTES as u64 {
        bail!("unsupported_large_file: maximum 16MiB");
    }
    // Bound the actual read too: the file may grow after the metadata check.
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FILE_BYTES {
        bail!("unsupported_large_file: maximum 16MiB");
    }
    Ok(bytes)
}

fn hash_file(path: &Path) -> Result<String> {
    Ok(hash(&read_bytes_bounded(path)?))
}

fn read_text(path: &Path) -> Result<String> {
    let bytes = read_bytes_bounded(path)?;
    if bytes.contains(&0) {
        bail!("unsupported_binary_file");
    }
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("unsupported_non_utf8_file"))
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
        entries.push(entry.path().to_path_buf());
    }
    entries.sort();
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
        id: crate::memory::source_id(),
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
    let paths: BTreeSet<String> = s
        .memory
        .entries
        .values()
        .flat_map(|m| m.sources.clone())
        .chain(s.investigations.iter().flat_map(|i| i.sources.clone()))
        .filter_map(|source| source.path)
        .collect();
    let mut hashes = std::collections::BTreeMap::new();
    for path in paths {
        let current = read_path(&s.project, &path)
            .and_then(|p| hash_file(&p))
            .ok();
        s.memory.stale_path(&path, current.as_deref());
        hashes.insert(path, current);
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
        }) || item.memory_refs.iter().any(|(id, rev)| {
            s.memory.get(id).map_or(true, |m| {
                m.revision != *rev || m.status != crate::memory::MemoryStatus::Active
            })
        });
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
    let h = documentation::resolve_heading(doc, heading)?;
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
        index
            .parse::<usize>()
            .map_err(|_| anyhow::anyhow!("invalid_cursor"))
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
    normalize_integer_arguments(name, &mut args);
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
            if max_lines != &limit {
                bail!(
                    "conflicting_arguments: file_read limit and max_lines differ; supply only max_lines (number of lines)"
                );
            }
        } else {
            fields.insert("max_lines".into(), limit);
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
            || (name == "investigation"
                && args["action"] == "upsert"
                && args["id"]
                    .as_str()
                    .is_none_or(|id| !s.investigations.iter().any(|item| item.id == id))))
    {
        bail!(
            "verification_reserve: focus on existing investigation items; broad discovery and new items are paused"
        );
    }
    match name {
        "code_outline" | "symbol_read" => structure::execute(s, name, &args, cancel),
        "document_inspect" | "document_audit" | "symbol_search" => {
            documentation::execute(s, name, &args, cancel)
        }
        "tool_catalog" => {
            let q = args["query"].as_str().unwrap_or("").to_lowercase();
            let terms: Vec<_> = q
                .split(|c: char| c.is_whitespace() || c == '_' || c == '-')
                .filter(|t| !t.is_empty())
                .collect();
            Ok(
                json!({"groups":["source-docs"],"tools":ToolRegistry::specs().into_iter().filter(|t|terms.is_empty() || terms.iter().any(|term| t.name.contains(term) || t.description.to_lowercase().contains(term))).map(|t|json!({"name":t.name,"description":t.description,"basic":!t.optional,"active":!t.optional||s.active_tools.contains(t.name)})).collect::<Vec<_>>()}),
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
            let input: MemoryInput = serde_json::from_value(args).map_err(|error| {
                anyhow::anyhow!("invalid_argument_value: memory_write fields: {error}")
            })?;
            let sources = s.source_refs(&input.source_ids)?;
            let result = s.memory.save(input, sources, &s.config)?;
            // A source can change between its original observation and this
            // write. Revalidate the newly stored entry too, so stale evidence
            // cannot be reported as an active fact until it is reread.
            let id = result.id.clone();
            revalidate(s)?;
            Ok(json!(s.memory.get(&id)?.meta()))
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
                    // Older investigation records may also contain a memory
                    // key instead of the canonical ID. Normalize those
                    // references before the replacement removes old entries.
                    let investigation_memory_refs = s
                        .investigations
                        .iter()
                        .map(|item| {
                            item.memory_refs
                                .iter()
                                .map(|(ident, revision)| (s.canonical_memory_id(ident), *revision))
                                .collect::<BTreeMap<_, _>>()
                        })
                        .collect::<Vec<_>>();
                    let input: MemoryInput = serde_json::from_value(args["replacement"].clone())
                        .map_err(|error| {
                            anyhow::anyhow!(
                                "invalid_argument_value: memory_manage replacement fields: {error}"
                            )
                        })?;
                    let sources = s.source_refs(&input.source_ids)?;
                    let replacement = s.memory.replace(&actual, input, sources, &s.config)?;
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
                    for (item, refs) in s.investigations.iter_mut().zip(investigation_memory_refs) {
                        item.memory_refs = refs;
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
                        bail!("invalid_argument_value: revision is program-owned");
                    }
                    value[k] = v.clone();
                }
                let mut next: TaskState = serde_json::from_value(value).map_err(|error| {
                    anyhow::anyhow!("invalid_argument_value: task_state patch: {error}")
                })?;
                if !["", "answer", "source_document", "document_edit"]
                    .contains(&next.workflow.as_str())
                {
                    bail!("invalid_workflow: use answer, source_document or document_edit");
                }
                if next.workflow == "source_document" {
                    next.require_investigation = true;
                }
                if s.task.workflow == "source_document" && next.workflow != "source_document" {
                    bail!(
                        "workflow_locked: source-document evidence requirements cannot be disabled during this request"
                    );
                }
                if !["", "investigate", "draft", "verify", "answer"].contains(&next.phase.as_str())
                {
                    bail!("invalid_task_phase: use investigate, draft, verify or answer");
                }
                if s.task.require_investigation && !next.require_investigation {
                    bail!(
                        "investigation_requirement_locked: cannot disable required evidence verification during this request"
                    );
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
                if s.task.require_investigation || s.task.workflow == "document_edit" {
                    for name in ["investigation", "document_edit", "document_audit"] {
                        s.active_tools.insert(name.into());
                        if let Some(pending) = &mut s.pending_tools {
                            pending.insert(name.into());
                        }
                    }
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
                        "end_line":source.end_line, "hash":source.hash,
                        "excerpt":source.excerpt.chars().take(600).collect::<String>(),
                        "excerpt_truncated":source.excerpt.chars().count() > 600
                    })
                })
                .collect();
            let next = offset.saturating_add(items.len());
            Ok(
                json!({"items":items,"total":matches.len(),"next_offset":(next < matches.len()).then_some(next)}),
            )
        }
        "checkpoint_complete" => {
            let cp = s
                .checkpoint
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no_checkpoint"))?;
            if cp.id != text(&args, "id")? {
                bail!("checkpoint_id_mismatch");
            }
            if cp.failed {
                bail!(
                    "checkpoint_has_failed_operations: an earlier operation in this batch failed; inspect its error and repair it on the next model request before checkpoint_complete. Successful writes are retained; do not repeat them. Completion cannot succeed in this failed batch"
                );
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
                bail!("invalid_argument_value: checkpoint progress must be nonempty");
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
            let mode = args["mode"].as_str().unwrap_or("text");
            let files = if mode == "paths" {
                candidate_paths(&s.project, path_glob(&args)?, cancel)?
            } else {
                paths(&s.project, path_glob(&args)?, cancel)?
            };
            let root = s.project.root.canonicalize()?;
            let names = files
                .iter()
                .map(|p| p.strip_prefix(&root).unwrap().display().to_string())
                .collect::<Vec<_>>();
            let fingerprint =
                hash(serde_json::to_string(&(mode, path_glob(&args)?, &names))?.as_bytes());
            let offset = page_cursor(&args, &fingerprint)?;
            if offset > names.len() {
                bail!("invalid_cursor");
            }
            let end = (offset + n(&args, "limit", 100).clamp(1, 500)).min(names.len());
            Ok(
                json!({"hash":fingerprint,"mode":mode,"total_files":names.len(),"paths":names[offset..end],"next_cursor":(end<names.len()).then(||format!("{fingerprint}:{end}"))}),
            )
        }
        "source_search" => search::execute(s, &args, cancel),
        "file_read" => read_file(s, &mut args, cancel),
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
                    let resolved = documentation::resolve_heading(&old, heading)?;
                    let target = &old[resolved.start..resolved.end];
                    if args["expected_section_hash"].as_str()
                        != Some(hash(target.as_bytes()).as_str())
                    {
                        bail!("section_revision_conflict");
                    }
                    if new.lines().next().map(str::trim) != target.lines().next().map(str::trim) {
                        bail!(
                            "invalid_argument_value: section replacement must retain its heading"
                        );
                    }
                    let replacement = format!("{}\n", new.trim_end());
                    let mut candidate = format!(
                        "{}{}{}",
                        &old[..resolved.start],
                        replacement,
                        &old[resolved.end..]
                    );
                    section_text(&candidate, heading)?;
                    if !old.ends_with('\n') && resolved.start == 0 && resolved.end == old.len() {
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
            s.document_review.approved_hash = None;
            s.last_document_write = Some((path.clone(), hash(result.as_bytes())));
            revalidate(s)?;
            // A successful persisted section establishes written state, never
            // verification. Match the registered heading exactly/unambiguously.
            let mut written_items = Vec::new();
            for item in &mut s.investigations {
                if item.status != "verified"
                    && !item.section.trim().is_empty()
                    && section_text(&result, &item.section)
                        .is_ok_and(|text| text.lines().skip(1).any(|line| !line.trim().is_empty()))
                {
                    item.status = "written".into();
                    written_items.push(item.id.clone());
                }
            }
            Ok(
                json!({"written_items":written_items,"verification_required_ids":s.investigations.iter().filter(|i| i.status != "verified").map(|i| &i.id).collect::<Vec<_>>(),"preserved_verified_ids":s.investigations.iter().filter(|i| i.status == "verified").map(|i| &i.id).collect::<Vec<_>>(),"verification_guidance":"Verify only verification_required_ids. Unchanged sections retain verification; do not resubmit all items after a local edit. If none remain, proceed to final completion and document review.","path":path,"hash":hash(result.as_bytes()),"bytes":result.len(),"total_lines":result.lines().count(),"citation_check":documentation::citation_check(s, &path, &result)?}),
            )
        }
        "investigation" => match text(&args, "action")? {
            "list" => {
                revalidate(s)?;
                let offset = n(&args, "offset", 0);
                let limit = n(&args, "limit", 20).clamp(1, 100);
                Ok(
                    json!({"items":s.investigations.iter().skip(offset).take(limit).collect::<Vec<_>>(),"next_offset":(offset.saturating_add(limit)<s.investigations.len()).then_some(offset.saturating_add(limit))}),
                )
            }
            "upsert" => {
                if args["id"].as_str().is_some_and(|id| id.trim().is_empty()) {
                    bail!(
                        "invalid_argument_value: id must not be empty for investigation action=upsert"
                    );
                }
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
                let mut item = Investigation {
                    id: id.clone(),
                    title: args["title"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| previous.as_ref().map(|item| item.title.clone()))
                        .ok_or_else(|| {
                            anyhow::anyhow!("missing_argument: title for new investigation")
                        })?,
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
                if item.status == "written" && item.section.trim().is_empty() {
                    bail!(
                        "investigation_section_required: status=written requires a non-empty section. Copy an exact heading from document_inspect and upsert this id with section and status=written together; existing item retained"
                    );
                }
                if item.status == "written" {
                    let doc = read_text(&output_path(&s.project)?)?;
                    item.section = documentation::resolve_heading(&doc, &item.section)?.heading;
                }
                let registered = json!({"id":id,"status":item.status,"section":item.section});
                if let Some(existing) = s.investigations.iter_mut().find(|i| i.id == id) {
                    *existing = item
                } else {
                    s.investigations.push(item);
                }
                Ok(registered)
            }
            "verify_batch" => {
                let items = args["items"].as_object().ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid_argument_type: items must be an object keyed by investigation ID"
                    )
                })?;
                if items.is_empty() || items.len() > 20 {
                    bail!("invalid_argument_value: items requires 1..20 entries");
                }
                revalidate(s)?;
                let mut reused_ids = vec![];
                let mut results = vec![];
                for (id, entry) in items {
                    if cancel.is_cancelled() {
                        bail!("cancelled");
                    }
                    let mut params = entry.clone();
                    if !params.is_object() {
                        results.push(
                            json!({"id":id,"result":envelope(Err(anyhow::anyhow!("invalid_argument_type: batch item must be an object with source_ids and verification_note")))}),
                        );
                        continue;
                    }
                    if let Some(key) =
                        params.as_object().unwrap().keys().find(|key| {
                            !["source_ids", "verification_note"].contains(&key.as_str())
                        })
                    {
                        results.push(json!({"id":id,"result":envelope(Err(anyhow::anyhow!("invalid_action_arguments: verify_batch item does not accept {key}; allowed: source_ids, verification_note; ID belongs in the items key")))}));
                        continue;
                    }
                    // Batch entries are not passed through the outer tool
                    // schema, so validate required fields and their types
                    // before either reusing or executing an item.
                    params["action"] = json!("verify");
                    params["id"] = json!(id);
                    if let Err(error) = ToolRegistry::validate(s, "investigation", &params) {
                        results.push(json!({"id":id,"result":envelope(Err(error))}));
                        continue;
                    }
                    // Reuse only after checking section, source and memory freshness.
                    // A redundant batch must not replace valid evidence with an incomplete list.
                    if s.investigations
                        .iter()
                        .any(|i| i.id == *id && i.status == "verified")
                    {
                        reused_ids.push(id.clone());
                        results.push(json!({"id":id,"result":envelope(Ok(json!({"verified":id,"reused":true})))}));
                        continue;
                    }
                    let result = execute_cancellable(s, "investigation", params, cancel);
                    results.push(json!({"id":id,"result":envelope(result)}));
                }
                let mut succeeded_ids = vec![];
                let mut retry_ids = vec![];
                let mut failures = std::collections::BTreeMap::<String, Vec<Value>>::new();
                for entry in &results {
                    if entry["result"]["status"] == "ok" {
                        succeeded_ids.push(entry["id"].clone());
                    } else {
                        retry_ids.push(entry["id"].clone());
                        let code = entry["result"]["recovery"]["code"]
                            .as_str()
                            .unwrap_or("tool_error");
                        failures
                            .entry(code.into())
                            .or_default()
                            .push(entry["id"].clone());
                    }
                }
                let reasons: Vec<_> = failures
                    .into_iter()
                    .map(|(code, ids)| json!({"code":code,"count":ids.len(),"ids":ids}))
                    .collect();
                Ok(
                    json!({"summary":{"total":results.len(),"succeeded":succeeded_ids.len(),"failed":retry_ids.len(),"failures_by_code":reasons},"succeeded_ids":succeeded_ids,"reused_ids":reused_ids,"retry_ids":retry_ids,"guidance":"Successful verification updates are retained; failed items are not marked verified. Correct and retry only retry_ids. Do not reverify successful items unless their section, sources or memory references change.","results":results,"semantic_verification":"agent attestation; not program proof"}),
                )
            }
            "verify" => {
                revalidate(s)?;
                let id = text(&args, "id")?;
                let item = s
                    .investigations
                    .iter()
                    .find(|i| i.id == id)
                    .ok_or_else(|| anyhow::anyhow!("item_not_found"))?;
                if item.status != "written" && item.status != "verified" {
                    bail!(
                        "item_must_be_written_before_verification: id={}, status={}, section={:?}. Inspect the document section first; if its content is written, use investigation upsert with this id, the exact section heading from document_inspect, and status=written together, then verify. Otherwise write the section before verifying.",
                        item.id,
                        item.status,
                        item.section
                    );
                }
                let note = text(&args, "verification_note")?;
                if note.trim().is_empty() {
                    bail!("missing_argument: verification_note must be non-empty");
                }
                let sources = s.source_refs(&list(&args, "source_ids"))?;
                if sources.is_empty()
                    || sources.iter().any(|source| {
                        source.origin != "file"
                            || source.path.is_none()
                            || source.hash.is_none()
                            || source.excerpt.trim().is_empty()
                    })
                {
                    bail!(
                        "verification_sources_required: pass non-empty observed file source_ids returned by file_read/source_search/symbol_search"
                    );
                }
                for source in &sources {
                    if let Some(path) = &source.path {
                        let path = read_path(&s.project, path)?;
                        if Some(hash_file(&path)?) != source.hash {
                            bail!("source_changed: read again");
                        }
                    }
                }
                let doc = read_text(&output_path(&s.project)?)?;
                for (id, revision) in &item.memory_refs {
                    let memory = s.memory.get(id)?;
                    if memory.revision != *revision
                        || memory.status != crate::memory::MemoryStatus::Active
                    {
                        bail!("memory_changed: refresh the memory and investigation references");
                    }
                }
                let section = section_text(&doc, &item.section)?;
                let missing = documentation::missing_citation_ranges(s, section, &sources)?;
                if !missing.is_empty() {
                    return Err(documentation::CoverageMissing {
                        item_id: id.into(),
                        missing_ranges: missing,
                    }
                    .into());
                }
                let item = s.investigations.iter_mut().find(|i| i.id == id).unwrap();
                item.document_hash = Some(hash(section.as_bytes()));
                item.sources = sources;
                item.note = note.into();
                item.status = "verified".into();
                Ok(json!({"verified":id}))
            }
            "final_check" => {
                s.task.require_investigation = true;
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
                    json!({"complete":!s.investigations.is_empty()&&incomplete.is_empty(),"semantic_verified":false,"completion_scope":"structural preflight; enabled source-document model review runs before task completion","incomplete":incomplete,"review":s.reviews,"output":s.project.output}),
                )
            }
            _ => unreachable!(),
        },
        _ => bail!("unsupported_tool"),
    }
}

/// Shared line reader for explicit file reads and Tree-sitter symbol bodies.
fn read_file(
    s: &mut Session,
    args: &mut Value,
    _cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    let cursor = if let Some(id) = args["cursor"].as_str() {
        if ["path", "start_line", "max_lines", "offset"]
            .iter()
            .any(|key| args.get(key).is_some())
        {
            bail!(
                "cursor_arguments_conflict: pass only cursor (and optional force_read); for a new range omit cursor and use path/start_line/max_lines"
            );
        }
        let cursor = s.file_cursors.get(id).cloned().ok_or_else(|| anyhow::anyhow!("invalid_file_cursor: copy next_cursor.cursor exactly from this session, or start a new read with path/start_line/max_lines"))?;
        args["path"] = json!(cursor.path);
        args["start_line"] = json!(cursor.start_line);
        args["max_lines"] = json!(cursor.max_lines);
        args["offset"] = json!(cursor.offset);
        Some(cursor)
    } else {
        None
    };
    let path = read_path(&s.project, text(args, "path")?)?;
    let contents = read_text(&path)?;
    if cursor
        .as_ref()
        .is_some_and(|c| c.hash != hash(contents.as_bytes()))
    {
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
        return Ok(
            json!({"path":path,"hash":hash(contents.as_bytes()),"total_lines":total_lines,"content":{"text":"","truncated":false,"next_offset":null},"source":null,"eof":true,"next_line":null,"next_offset":0}),
        );
    }
    let selected = contents
        .lines()
        .skip(start - 1)
        .take(lines)
        .collect::<Vec<_>>()
        .join("\n");
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
    if prior >= s.config.repeated_read_limit && !args["force_read"].as_bool().unwrap_or(false) {
        return Ok(
            json!({"path":path,"hash":hash(contents.as_bytes()),"total_lines":contents.lines().count(),"repeated_read":true,"suppressed":true,"guidance":"Unchanged range already present repeatedly in active context. Reuse it, read another range, use document_inspect for output metadata, or force_read=true for deliberate verification."}),
        );
    }
    let mut content = bounded_text(s, &selected, offset);
    let shown = content["text"].as_str().unwrap();
    let observed_start = start + selected.chars().take(offset).filter(|c| *c == '\n').count();
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
    content["line_end"] = json!(source.end_line);
    content["first_line_complete"] =
        json!(offset == 0 || selected.chars().nth(offset - 1) == Some('\n'));
    let shown_end = offset + content["text"].as_str().unwrap().chars().count();
    content["last_line_complete"] = json!(
        content["text"].as_str().unwrap().ends_with('\n')
            || selected.chars().nth(shown_end).is_none_or(|c| c == '\n')
    );
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
fn register_file_cursor(s: &mut Session, id: String, result: &Value) {
    let data = &result["data"];
    s.file_cursors.insert(
        id,
        crate::session::FileCursor {
            path: data["path"].as_str().unwrap().into(),
            hash: data["hash"].as_str().unwrap().into(),
            start_line: data["read_start"].as_u64().unwrap() as usize,
            max_lines: data["read_max_lines"].as_u64().unwrap() as usize,
            offset: data["next_offset"].as_u64().unwrap() as usize,
        },
    );
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
    if matches!(call.name.as_str(), "file_read" | "symbol_read")
        && result["status"] == "ok"
        && result["data"]["content"]["truncated"] == true
        && !result["next_cursor"]["cursor"].is_string()
    {
        let id = new_file_cursor_id();
        register_file_cursor(s, id.clone(), &result);
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
                        shown.ends_with('\n') || chars[length] == '\n'
                    } else {
                        template["data"]["content"]["last_line_complete"]
                            .as_bool()
                            .unwrap_or(false)
                    };
                    v["data"]["content"]["line_end"] = json!(end);
                    v["data"]["content"]["last_line_complete"] = json!(complete);
                    v["data"]["source"]["end_line"] = json!(end);
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
                if let Some(id) = narrowed_cursor {
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
                    s.sources.insert(source.id.clone(), source);
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
    for field in ["paths", "matches", "items"] {
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
    if let Some(recovery) = result.get("recovery") {
        compact["recovery"] = recovery.clone();
        // Detailed recovery tools remain in the archive if the tiny result
        // budget cannot carry them, but the stable code/action must survive.
        if let Some(recovery) = compact["recovery"].as_object_mut() {
            recovery.remove("tools");
        }
    }
    if result["partial_success"] == true {
        compact["partial_success"] = json!(true);
    }
    // Archive fallback must not erase the cause of a failed operation.
    if let Some(error) = result["error"].as_str() {
        compact["error"] =
            json!(context::truncate(error, (limit / 3).clamp(8, 128), &s.config.model).0);
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
    let signature = format!("{}:{}", call.name, call.arguments);
    if let Some((stored, result)) = s.ledger.get(&call.id) {
        return if stored == &signature {
            result.clone()
        } else {
            envelope(Err(anyhow::anyhow!("call_id_collision")))
        };
    }
    let result = serde_json::from_str(&call.arguments)
        .map_err(|e| anyhow::anyhow!("invalid_tool_arguments: {e}"))
        .and_then(|args| execute_cancellable(s, &call.name, args, cancel));
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
