//! Name the argument or value a rejected call most likely meant. Live runs
//! resent calls with `file_path`, `old_string`, `context` or `kind:"fn"`
//! after an error that only listed the allowed names. A suggestion is advice
//! appended to the error; no value is moved or reinterpreted, so the model
//! still resends the corrected call itself.
use serde_json::Value;

pub(crate) struct Suggestion {
    /// The accepted field or value to send instead, when it is a direct rename.
    pub target: Option<String>,
    /// A clause appended to the error message, starting with "; ".
    pub text: String,
}

/// Lowercase snake form: `filePath`, `file-path` and `File Path` become
/// `file_path`.
fn snake(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 4);
    let mut previous_lower = false;
    for c in text.trim().chars() {
        if c.is_uppercase() && previous_lower {
            out.push('_');
        }
        previous_lower = c.is_lowercase() || c.is_ascii_digit();
        match c {
            '-' | ' ' | '.' => out.push('_'),
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

fn squashed(text: &str) -> String {
    snake(text).replace('_', "")
}

/// Optimal string alignment distance, bounded to short identifiers.
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > 2 {
        return 3;
    }
    let mut rows = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in rows[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (rows[i - 1][j] + 1)
                .min(rows[i][j - 1] + 1)
                .min(rows[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = best;
        }
    }
    rows[a.len()][b.len()]
}

/// The one allowed name that is a spelling variant of `received`: the same
/// name in another case or separator style, a singular/plural form, or a
/// typo of at most two characters (one for short names). Ambiguous matches
/// suggest nothing.
fn closest<'a>(received: &str, allowed: &[&'a str]) -> Option<&'a str> {
    let wanted = squashed(received);
    if wanted.len() < 3 {
        return None;
    }
    let exact: Vec<_> = allowed
        .iter()
        .filter(|name| squashed(name) == wanted)
        .collect();
    if let [only] = exact.as_slice() {
        return Some(only);
    }
    let plural: Vec<_> = allowed
        .iter()
        .filter(|name| {
            let name = squashed(name);
            [
                format!("{name}s"),
                format!("{name}es"),
                name.trim_end_matches('s').to_owned(),
            ]
            .contains(&wanted)
                || wanted.trim_end_matches('s') == name.trim_end_matches('s')
        })
        .collect();
    if let [only] = plural.as_slice() {
        return Some(only);
    }
    let limit = if wanted.len() < 6 { 1 } else { 2 };
    let near: Vec<_> = allowed
        .iter()
        .map(|name| (distance(&wanted, &squashed(name)), *name))
        .filter(|(d, _)| *d <= limit)
        .collect();
    let best = near.iter().map(|(d, _)| *d).min()?;
    match near
        .iter()
        .filter(|(d, _)| *d == best)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [(_, only)] => Some(only),
        _ => None,
    }
}

/// Field names models send for an accepted field. A name maps to the first
/// target the tool accepts, so `name` means query for a search tool and
/// title for a memory.
#[rustfmt::skip]
const FIELD_ALIASES: &[(&str, &[&str])] = &[
    ("path", &[
        "file", "file_path", "filepath", "filename", "file_name", "dir", "directory", "folder",
        "dir_path", "directory_path", "location", "source_path", "relative_path",
    ]),
    ("path_glob", &[
        "glob", "include", "includes", "file_glob", "files_glob", "file_pattern",
        "path_pattern", "glob_pattern", "file_filter",
    ]),
    ("queries", &["terms", "keywords", "patterns", "words", "search_terms", "query_list"]),
    ("query", &[
        "q", "text", "search", "search_text", "search_query", "keyword", "term", "name",
        "symbol", "symbol_name", "needle", "find", "filter", "string",
    ]),
    ("start_line", &[
        "line", "line_number", "lineno", "start", "from", "from_line", "begin", "begin_line",
        "first_line", "line_start", "start_row",
    ]),
    ("max_lines", &["lines", "num_lines", "line_count", "n_lines", "max_line_count", "length"]),
    ("limit", &[
        "max", "max_results", "max_items", "max_count", "page_size", "per_page", "count",
        "top_k", "size", "n", "num_results",
    ]),
    ("cursor", &["next_cursor", "page_token", "next_page", "continuation", "continuation_token"]),
    ("regex", &["is_regex", "use_regex", "regexp", "as_regex", "pattern_is_regex"]),
    ("whole_word", &["word", "whole_words", "word_boundary", "words_only", "match_word", "whole"]),
    ("case_sensitive", &["case", "match_case", "case_sensitivity", "casesensitive"]),
    ("before", &["before_context", "lines_before", "context_before"]),
    ("after", &["after_context", "lines_after", "context_after"]),
    ("symbol_id", &["id", "symbol", "sid", "symbolid"]),
    ("relation", &["relation_type", "relationship", "direction", "type", "kind"]),
    ("kind", &["symbol_kind", "kinds", "type", "symbol_type", "category", "memory_type"]),
    ("container", &["parent", "class", "class_name", "scope", "within", "owner", "parent_name"]),
    ("max_depth", &["depth", "level", "nesting", "max_level"]),
    ("view", &["format", "detail", "verbosity", "output", "style"]),
    ("old_text", &[
        "old", "old_string", "old_str", "old_content", "find", "search", "search_text",
        "target", "target_text", "anchor", "anchor_text", "original", "original_text", "match",
        "from_text", "before_text",
    ]),
    ("new_text", &[
        "new", "new_string", "new_str", "new_content", "replacement", "replace",
        "replace_with", "to_text", "after_text", "text",
    ]),
    ("text", &[
        "content", "contents", "new_text", "new", "new_content", "new_string", "body",
        "markdown", "replacement", "value", "string", "section_text", "data",
    ]),
    ("content", &[
        "text", "contents", "body", "data", "file_content", "new_content", "source", "code",
    ]),
    ("expected_hash", &[
        "hash", "document_hash", "doc_hash", "file_hash", "base_hash", "sha", "sha256",
        "current_hash", "revision_hash", "content_hash",
    ]),
    ("expected_section_hash", &["section_hash", "heading_hash"]),
    ("section", &[
        "heading", "section_title", "title", "section_path", "target_section", "header",
        "section_name", "heading_path",
    ]),
    ("edits", &["operations", "ops", "changes", "items", "edit_list", "actions", "patches"]),
    ("operations", &["ops", "changes", "edits", "items", "actions", "patches", "operation"]),
    ("action", &["op", "operation", "type", "mode", "command", "cmd", "method", "edit_action"]),
    ("replace_all", &["all", "global", "replace_every", "every", "all_occurrences"]),
    ("to_path", &[
        "destination", "dest", "new_path", "target_path", "to", "destination_path", "new_name",
    ]),
    ("title", &["name", "heading", "subject", "label"]),
    ("summary", &["description", "abstract", "short", "brief", "tldr", "overview"]),
    ("body", &[
        "content", "text", "details", "detail", "description", "notes", "value", "contents",
    ]),
    ("source_ids", &[
        "sources", "source", "source_id", "evidence", "evidence_ids", "refs", "references",
        "citations", "sourceids", "source_refs",
    ]),
    ("tags", &["tag", "labels", "keywords", "categories"]),
    ("expected_revision", &[
        "revision", "rev", "version", "plan_revision", "current_revision", "base_revision",
        "expected_version",
    ]),
    ("inferred", &["is_inferred", "inference", "hypothesis", "speculative"]),
    ("key", &["memory_key", "slug"]),
    ("ids", &["id", "memory_ids", "memory_id", "keys", "id_list"]),
    ("replacement", &[
        "memory", "new_memory", "new", "merged", "entry", "merged_memory", "replace_with",
    ]),
    ("patch", &["changes", "update", "updates", "fields", "data", "state", "set", "values"]),
    ("names", &["tools", "tool_names", "tool", "name", "tool_name"]),
    ("id", &[
        "item_id", "query_id", "checkpoint_id", "history_id", "bundle_id",
        "source_id", "memory_id", "key", "name",
    ]),
    ("params", &[
        "parameters", "binds", "bind", "bind_params", "bind_values", "values", "variables",
        "arguments", "args",
    ]),
    ("sql", &["query", "statement", "sql_text", "sql_query", "text", "command"]),
    ("name", &["procedure", "function", "proc", "func", "procedure_name", "function_name"]),
    ("args", &["arguments", "parameters", "positional_args"]),
    ("return_type", &["returns", "return", "result_type", "returns_type"]),
    ("progress", &[
        "summary", "notes", "message", "status", "text", "content", "checkpoint_summary",
    ]),
    ("no_save_reason", &["reason", "no_save", "skip_reason", "why", "no_memory_reason"]),
    ("verification_note", &[
        "note", "notes", "comment", "verification", "evidence_note", "explanation",
    ]),
    ("status", &["state"]),
    ("direction", &["dir", "param_direction", "io", "inout_mode"]),
    ("memory_ids", &["memories", "memory", "memory_id"]),
    ("reason", &["why", "gap_reason", "explanation", "gap"]),
    ("after", &["since", "after_id", "from_id"]),
    ("items", &["verifications", "batch", "entries"]),
    ("texts", &["items", "todos", "tasks", "steps", "titles", "entries", "list", "subtasks"]),
    ("coverage_offset", &["coverage_page"]),
];

/// A field that cannot simply be renamed: the tool has no such concept or
/// expects the value in another shape.
fn special_field(
    tool: &str,
    scope: &str,
    key: &str,
    original: &str,
    siblings: &Value,
) -> Option<Suggestion> {
    let note = |text: String| Some(Suggestion { target: None, text });
    let renamed = |target: &str, text: String| {
        Some(Suggestion {
            target: Some(target.into()),
            text,
        })
    };
    match (tool, scope, key) {
        (
            "file_read" | "symbol_read",
            "",
            "end_line" | "end" | "line_end" | "to_line" | "stop_line" | "last_line" | "until_line"
            | "end_row",
        ) => {
            let computed = match (siblings[original].as_u64(), siblings["start_line"].as_u64()) {
                (Some(end), Some(start)) if end >= start => {
                    format!(" (here max_lines: {})", end - start + 1)
                }
                _ => String::new(),
            };
            renamed(
                "max_lines",
                format!(
                    "; {tool} has no end line: send max_lines = end_line - start_line + 1{computed}"
                ),
            )
        }
        ("file_read", "", "paths" | "files" | "file_paths") => note(format!(
            "; {tool} reads one file per call: send a separate file_read call with path for each file"
        )),
        ("memory_read", "", "ids" | "keys") => note(
            "; memory_read loads one memory per call: send a separate memory_read with id for each".into(),
        ),
        ("memory_read", "", "key" | "memory_key" | "memory_id" | "name") => renamed(
            "id",
            "; did you mean id? id accepts a memory ID or its key".into(),
        ),
        ("source_lookup", "", "ids" | "source_ids") => note(
            "; source_lookup takes one exact source ID in id per call, or a path substring in path to list a file's observed sources".into(),
        ),
        ("source_lookup", "", "source_id" | "sid") => {
            renamed("id", "; did you mean id? send the source ID as id".into())
        }
        (
            "source_search",
            "",
            "context" | "context_lines" | "surrounding" | "around",
        ) => note(
            "; source_search has no context argument: send before and after (0 to 20 lines each)".into(),
        ),
        (
            "source_search" | "code_outline" | "symbol_search",
            "",
            "ignore_case" | "case_insensitive" | "insensitive" | "icase" | "nocase",
        ) => renamed(
            "case_sensitive",
            "; send case_sensitive:false instead (note the inverted meaning)".into(),
        ),
        ("file_list", "", "recursive" | "depth" | "max_depth" | "deep" | "recurse") => note(
            "; file_list always lists every file below path (or the whole project); drop it".into(),
        ),
        (
            "document_inspect",
            "",
            "start_line" | "line" | "end_line" | "max_lines" | "lines" | "line_number",
        ) => note(
            "; document_inspect pages by outline heading index (offset) or by section; to read document lines by number use file_read with the output path and start_line".into(),
        ),
        (
            "symbol_read" | "symbol_relations",
            "",
            "name" | "symbol" | "symbol_name" | "function" | "fn" | "method" | "query",
        ) => note(format!(
            "; {tool} needs symbol_id copied from code_outline or symbol_search, not a name: find it with symbol_search {{\"query\":{},\"match\":\"exact\"}}",
            siblings[original]
        )),
        ("file_patch", "", "patch" | "diff" | "unified_diff" | "input") => note(
            "; file_patch takes operations, an array of {action,path,...} objects, not diff text: e.g. {\"operations\":[{\"action\":\"update\",\"path\":\"src/a.rs\",\"expected_hash\":\"<hash from file_read>\",\"old_text\":\"...\",\"new_text\":\"...\"}]}".into(),
        ),
        (
            "task_plan",
            "",
            "op" | "texts" | "text" | "before" | "id" | "result" | "reason" | "item" | "todo"
            | "todos" | "task" | "tasks",
        ) => note(format!(
            "; {key} belongs inside one plan operation: {{\"action\":\"apply\",\"expected_revision\":<plan revision>,\"operations\":[{{\"op\":\"insert\",\"texts\":[\"...\"]}}]}}"
        )),
        ("memory_manage", "", "id" | "memory_id" | "key") => renamed(
            "ids",
            "; did you mean ids? send an array, e.g. \"ids\":[\"<memory id>\"]".into(),
        ),
        ("task_state", "patch", "current" | "next" | "done" | "todos" | "todo" | "plan" | "tasks") => {
            note("; the ordered work list is managed by task_plan (list, then apply with operations), not by task_state".into())
        }
        _ => None,
    }
}

/// Suggest the accepted field for an unaccepted `key`. `scope` names the
/// enclosing argument for nested objects ("" at the top level; "edits",
/// "operations", "patch", ...). `siblings` is the object holding `key`.
pub(crate) fn field(
    tool: &str,
    scope: &str,
    key: &str,
    allowed: &[&str],
    siblings: &Value,
) -> Option<Suggestion> {
    let normalized = snake(key);
    if let Some(found) = special_field(tool, scope, &normalized, key, siblings) {
        return Some(found);
    }
    let target = FIELD_ALIASES
        .iter()
        .find(|(target, aliases)| {
            *target != normalized && allowed.contains(target) && aliases.contains(&&*normalized)
        })
        .map(|(target, _)| *target)
        .or_else(|| closest(key, allowed).filter(|target| *target != key))?;
    let text = if siblings.get(target).is_some() {
        format!("; {target} is already supplied, so drop {key}")
    } else {
        format!("; did you mean {target}? send this value as {target}")
    };
    Some(Suggestion {
        target: Some(target.into()),
        text,
    })
}

/// Values models send for an accepted enum value of the same field.
#[rustfmt::skip]
const VALUE_ALIASES: &[(&str, &str, &[&str])] = &[
    ("kind", "function", &["fn", "func", "def", "functions", "fun", "proc"]),
    ("kind", "method", &["methods", "member_function", "fn_method"]),
    ("kind", "variable", &["var", "let", "vars", "variables"]),
    ("kind", "constant", &["const", "consts", "constants"]),
    ("kind", "class", &["cls", "classes"]),
    ("kind", "interface", &["iface", "interfaces"]),
    ("kind", "property", &["prop", "props", "properties"]),
    ("kind", "constructor", &["ctor", "init", "constructors"]),
    ("kind", "struct", &["structs", "record_struct"]),
    ("kind", "module", &["mod", "modules", "namespace", "package"]),
    ("kind", "type", &["type_alias", "typedef", "types"]),
    ("kind", "enum_member", &["variant", "enum_variant", "enum_value"]),
    ("kind", "fact", &["note", "finding", "observation", "info", "information"]),
    ("kind", "failure", &["lesson", "error", "bug", "problem", "issue", "mistake"]),
    ("kind", "procedure", &["howto", "how_to", "steps", "process", "recipe", "instructions"]),
    ("kind", "decision", &["choice", "rationale", "design"]),
    ("kind", "question", &["open_question", "unknown"]),
    ("relation", "calls", &["callees", "outgoing", "calls_to", "call", "outbound", "callee"]),
    ("relation", "callers", &["incoming", "called_by", "caller", "inbound", "callers_of"]),
    ("relation", "references", &["refs", "usages", "uses", "reference", "usage", "ref"]),
    ("match", "contains", &[
        "substring", "partial", "fuzzy", "includes", "prefix", "starts_with", "like",
    ]),
    ("match", "exact", &["equals", "exact_match", "full", "strict", "eq", "equal", "whole"]),
    ("mode", "matches", &["lines", "line", "content", "match", "grep", "text"]),
    ("mode", "files", &["files_with_matches", "file", "names", "filenames", "list"]),
    ("mode", "count", &["counts", "total", "number"]),
    ("mode", "paths", &["tree", "list", "all", "files", "names", "path"]),
    ("view", "compact", &["summary", "brief", "short", "names", "list", "simple"]),
    ("view", "detailed", &["full", "verbose", "detail", "details", "signatures", "long"]),
    ("action", "read", &["get", "show", "view", "fetch", "load", "open", "display"]),
    ("action", "update", &["set", "patch", "edit", "modify", "change", "save"]),
    ("action", "search", &["find", "query", "grep", "lookup", "list"]),
    ("action", "candidates", &["list", "candidate", "cleanup", "review", "find"]),
    ("action", "delete", &["remove", "del", "drop", "purge"]),
    ("action", "replace", &["merge", "update", "consolidate", "supersede", "set"]),
    ("action", "add", &["enable", "activate", "include", "select", "load"]),
    ("action", "remove", &["disable", "deactivate", "exclude", "drop", "unload"]),
    ("action", "run", &["execute", "exec", "query", "call", "invoke"]),
    ("action", "list", &["ls", "describe", "show", "get", "catalog"]),
    ("action", "add", &["create", "new", "add_file"]),
    ("action", "update", &["edit", "modify", "patch", "change"]),
    ("action", "move", &["rename", "mv"]),
    ("action", "apply", &["update", "edit", "change", "modify", "set"]),
    ("mode", "query", &["select", "read", "read_only", "fetch"]),
    ("mode", "statement", &[
        "dml", "ddl", "exec", "execute", "sql", "update", "insert", "delete", "write",
    ]),
    ("mode", "procedure", &["proc", "call", "stored_procedure"]),
    ("mode", "function", &["func", "fn", "stored_function"]),
    ("phase", "investigate", &[
        "investigation", "investigating", "research", "explore", "exploration", "discover",
    ]),
    ("phase", "draft", &["drafting", "write", "writing", "compose"]),
    ("phase", "verify", &[
        "verification", "verifying", "review", "check", "validate", "validation",
    ]),
    ("phase", "answer", &["final", "done", "complete", "respond", "answering", "finish", "report"]),
    ("status", "met", &[
        "pass", "passed", "satisfied", "fulfilled", "done", "complete", "completed", "ok", "yes",
        "true", "achieved",
    ]),
    ("status", "unmet", &[
        "fail", "failed", "not_met", "unsatisfied", "missing", "no", "false", "incomplete",
        "not_done",
    ]),
    ("status", "unverified", &[
        "unknown", "unclear", "uncertain", "not_verified", "unconfirmed", "insufficient",
        "needs_verification", "cannot_verify",
    ]),
    ("status", "uninvestigated", &["pending", "todo", "new", "not_started", "planned", "open"]),
    ("status", "in_progress", &[
        "investigating", "working", "started", "wip", "ongoing", "active", "drafting",
    ]),
    ("status", "written", &["drafted", "wrote", "complete", "completed", "done", "finished"]),
    ("op", "insert", &["add", "create", "new", "append", "push"]),
    ("op", "update", &["edit", "rename", "change", "modify", "set"]),
    ("op", "complete", &["done", "finish", "mark_done", "close", "resolve", "completed", "check"]),
    ("op", "remove", &["delete", "drop", "cancel", "discard"]),
    ("op", "move", &["reorder", "reposition"]),
    ("op", "reopen", &["undo", "reset", "uncomplete", "restore"]),
    ("op", "split", &["divide", "breakdown", "break_down", "expand"]),
    ("direction", "inout", &["in_out", "both", "io"]),
    ("type", "string", &["str", "text", "varchar", "varchar2", "char", "clob"]),
    ("type", "number", &["int", "integer", "float", "double", "numeric", "decimal"]),
    ("type", "boolean", &["bool"]),
    ("type", "cursor", &["refcursor", "sys_refcursor", "ref_cursor"]),
    ("return_type", "string", &["str", "text", "varchar", "varchar2", "char", "clob"]),
    ("return_type", "number", &["int", "integer", "float", "double", "numeric", "decimal"]),
    ("return_type", "boolean", &["bool"]),
    ("return_type", "cursor", &["refcursor", "sys_refcursor", "ref_cursor"]),
];

/// Document edit actions depend on which anchor arguments came with them.
fn document_action(received: &str, siblings: &Value, batch: bool) -> Option<Suggestion> {
    let anchored = siblings.get("old_text").is_some();
    let sectioned = siblings.get("section").is_some();
    let chosen = |target: &str, why: &str| {
        Some(Suggestion {
            target: Some(target.into()),
            text: format!("; did you mean \"{target}\"? {why}"),
        })
    };
    let options = |text: &str| {
        Some(Suggestion {
            target: None,
            text: format!("; {text}"),
        })
    };
    match received {
        "replace" | "update" | "edit" | "modify" | "change" | "substitute" | "rewrite"
        | "replace_section" | "update_section" | "rewrite_section" | "edit_section" => {
            if anchored {
                chosen(
                    "replace_text",
                    "it replaces the exact old_text passage with text",
                )
            } else if sectioned {
                chosen(
                    "section",
                    "it replaces the named section, including its heading, and needs expected_section_hash",
                )
            } else {
                options(
                    "use replace_text with old_text for a passage, section with section and expected_section_hash for a whole section, or write to replace the whole document",
                )
            }
        }
        "insert" | "add" | "add_section" | "insert_section" | "new_section" => {
            if anchored {
                chosen(
                    "insert_after_text",
                    "it inserts text after the exact old_text passage (insert_before_text inserts before it)",
                )
            } else if sectioned {
                options(
                    "use insert_after or insert_before for a sibling section of the same level, or insert_last_child or insert_first_child for a subsection under section",
                )
            } else {
                chosen("append", "it adds text at the end of the document")
            }
        }
        "prepend" => {
            if anchored {
                chosen(
                    "insert_before_text",
                    "it inserts text before the exact old_text passage",
                )
            } else {
                chosen(
                    "insert_before",
                    "it inserts a sibling section before section",
                )
            }
        }
        "delete" | "remove" | "delete_section" | "remove_text" | "remove_section" => chosen(
            "delete_text",
            "it deletes the exact old_text passage; to remove a section, copy the section's whole text into old_text",
        ),
        "overwrite" | "replace_all" | "rewrite_all" | "replace_document" | "full" => {
            chosen("write", "it replaces the whole document")
        }
        "create" if batch => options(
            "document_edit_batch edits an existing document: create it first with document_edit action=create, or use write here to replace all of it",
        ),
        "new" | "init" | "create_document" | "new_document" if !batch => {
            chosen("create", "it creates the configured output")
        }
        _ => None,
    }
}

/// Suggest an allowed value for a rejected enum `received` of `field` (the
/// last path segment). `siblings` is the object holding the field.
pub(crate) fn value(
    tool: &str,
    field: &str,
    received: &Value,
    allowed: &[Value],
    siblings: &Value,
) -> Option<Suggestion> {
    let received = received.as_str()?;
    let allowed: Vec<&str> = allowed.iter().filter_map(Value::as_str).collect();
    let normalized = snake(received);
    if field == "action"
        && matches!(tool, "document_edit" | "document_edit_batch")
        && let Some(found) = document_action(&normalized, siblings, tool == "document_edit_batch")
    {
        return Some(found);
    }
    if field == "match" && matches!(normalized.as_str(), "regex" | "regexp" | "pattern" | "glob") {
        return Some(Suggestion {
            target: None,
            text: format!(
                "; {tool} matches symbol names only as contains (substring) or exact; for a regular expression over source text use source_search with regex:true"
            ),
        });
    }
    let target = VALUE_ALIASES
        .iter()
        .find(|(name, target, aliases)| {
            *name == field && allowed.contains(target) && aliases.contains(&&*normalized)
        })
        .map(|(_, target, _)| *target)
        .or_else(|| closest(received, &allowed))
        .or_else(|| {
            // A unique allowed value that starts with what was sent ("ref"
            // for references) or that the sent value starts with.
            let starts: Vec<_> = allowed
                .iter()
                .filter(|value| {
                    normalized.len() >= 3
                        && (value.starts_with(normalized.as_str())
                            || normalized.starts_with(*value))
                })
                .collect();
            match starts.as_slice() {
                [only] => Some(**only),
                _ => None,
            }
        })
        .filter(|target| *target != received)?;
    Some(Suggestion {
        target: Some(target.into()),
        text: format!("; did you mean \"{target}\"?"),
    })
}

/// Tool names models carry over from other agents, mapped to the tools that
/// do that work here. A name maps to the first of its targets offered now,
/// so "edit_file" means file_edit where project writes are allowed and
/// document_edit in source-document work.
#[rustfmt::skip]
const TOOL_ALIASES: &[(&str, &[&str])] = &[
    ("file_read", &[
        "read", "read_file", "readfile", "open", "open_file", "cat", "view", "view_file",
        "get_file", "read_lines", "file_view", "read_result",
    ]),
    ("file_list", &[
        "ls", "list", "list_files", "list_dir", "list_directory", "glob", "find_files", "files",
        "dir", "tree", "file_search",
    ]),
    ("source_search", &[
        "grep", "search", "search_code", "rg", "ripgrep", "find", "code_search", "search_files",
        "find_in_files", "search_source",
    ]),
    ("code_outline", &["outline", "symbols", "list_symbols", "file_outline", "document_symbols"]),
    ("symbol_search", &["find_symbol", "search_symbols", "symbol_find", "workspace_symbols"]),
    ("symbol_read", &["read_symbol", "show_symbol", "get_symbol"]),
    ("symbol_relations", &["callers", "references", "find_references", "call_graph", "calls"]),
    ("file_edit", &["edit", "edit_file", "str_replace", "replace", "replace_in_file", "apply_edit"]),
    ("file_write", &["write", "write_file", "create_file", "save_file"]),
    ("file_patch", &["patch", "apply_patch", "multi_edit"]),
    ("document_edit", &[
        "edit", "edit_file", "str_replace", "replace", "write", "edit_document", "write_document",
        "update_document", "doc_edit", "create_document", "save_document",
    ]),
    ("document_inspect", &[
        "inspect", "inspect_document", "read_document", "outline_document", "document_outline",
        "view_document", "document_read",
    ]),
    ("document_audit", &["audit", "audit_document", "check_document", "validate_document"]),
    ("task_plan", &[
        "tool_plan", "plan", "todo", "todos", "update_plan", "todo_write", "task_list", "tasks",
        "plan_update", "task_plans",
    ]),
    ("task_state", &["state", "get_state", "task", "task_status", "update_task"]),
    ("memory_write", &["remember", "save_memory", "write_memory", "memory_save", "store_memory"]),
    ("memory_find", &["search_memory", "recall", "find_memory", "memory_search", "memories"]),
    ("memory_read", &["get_memory", "load_memory", "read_memory", "memory_get"]),
    ("history", &["search_history", "read_history", "get_history"]),
    ("checkpoint_complete", &["checkpoint", "complete_checkpoint", "finish_checkpoint"]),
];

/// Suggest the offered tool a call to the unknown tool `name` most likely
/// meant. A name carrying leaked call markup ("read_result_key>a.jsx
/// </arg_value>") is matched by its leading identifier.
pub(crate) fn tool(name: &str, offered: &[&str]) -> Option<Suggestion> {
    let identifier: String = name
        .trim()
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .collect();
    let markup = identifier.len() < name.trim().len();
    // Leaked native markup (<arg_key>) leaves a "_key"/"_value" tail on the
    // identifier.
    let identifier = if markup {
        ["_arg_key", "_arg_value", "_key", "_value", "_arg"]
            .iter()
            .find_map(|tail| identifier.strip_suffix(tail))
            .unwrap_or(&identifier)
            .to_owned()
    } else {
        identifier
    };
    let markup_note = if markup {
        "; the name also carries extra text or call markup: send only the exact tool name as the function name and its arguments as a JSON object"
    } else {
        ""
    };
    let normalized = snake(&identifier);
    if normalized == "run_guidance" {
        return Some(Suggestion {
            target: None,
            text: "; run_guidance is program state included in every request, not a tool: read it there and call the tool its instruction names".into(),
        });
    }
    let target = TOOL_ALIASES
        .iter()
        .find(|(target, aliases)| offered.contains(target) && aliases.contains(&&*normalized))
        .map(|(target, _)| *target)
        .or_else(|| closest(&identifier, offered))
        .filter(|target| *target != name);
    match target {
        Some(target) => Some(Suggestion {
            target: Some(target.into()),
            text: format!(
                "; did you mean {target}? send the same arguments to {target} if they match its parameters{markup_note}"
            ),
        }),
        None if markup => Some(Suggestion {
            target: None,
            text: markup_note.into(),
        }),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spelling_variants_and_aliases_name_one_target() {
        let allowed = [
            "query",
            "queries",
            "path",
            "path_glob",
            "case_sensitive",
            "limit",
        ];
        let target = |key: &str| {
            field("source_search", "", key, &allowed, &json!({})).and_then(|s| s.target)
        };
        assert_eq!(target("filePath").as_deref(), Some("path"));
        assert_eq!(target("Query").as_deref(), Some("query"));
        assert_eq!(target("glob").as_deref(), Some("path_glob"));
        assert_eq!(target("ignore_case").as_deref(), Some("case_sensitive"));
        assert_eq!(target("max_results").as_deref(), Some("limit"));
        assert_eq!(target("zebra"), None);
    }

    #[test]
    fn tool_names_map_to_an_offered_tool() {
        let offered = ["file_read", "task_plan", "document_edit", "source_search"];
        let target = |name: &str| tool(name, &offered).and_then(|s| s.target);
        assert_eq!(target("read").as_deref(), Some("file_read"));
        assert_eq!(target("tool_plan").as_deref(), Some("task_plan"));
        // file_edit is not offered, so an edit means the document.
        assert_eq!(target("edit_file").as_deref(), Some("document_edit"));
        assert_eq!(target("source_serach").as_deref(), Some("source_search"));
        assert_eq!(target("zebra"), None);
        let leaked = tool("read_result_key>main.jsx</arg_value>", &offered).unwrap();
        assert_eq!(leaked.target.as_deref(), Some("file_read"));
        assert!(leaked.text.contains("call markup"), "{}", leaked.text);
        let state = tool("run_guidance", &offered).unwrap();
        assert!(state.target.is_none() && state.text.contains("program state"));
    }

    #[test]
    fn values_follow_aliases_case_and_unique_prefixes() {
        let kinds = [json!("function"), json!("method"), json!("class")];
        let target = |sent: &str| {
            value("code_outline", "kind", &json!(sent), &kinds, &json!({})).and_then(|s| s.target)
        };
        assert_eq!(target("fn").as_deref(), Some("function"));
        assert_eq!(target("Functions").as_deref(), Some("function"));
        assert_eq!(target("meth").as_deref(), Some("method"));
        assert_eq!(target("struct"), None);
    }
}
