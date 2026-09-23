//! Shared UI/server inventory and structural documentation coverage. Discovery
//! proposes candidates; explicit source review remains necessary for every file.
use super::*;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
mod discovery;

const MAX_FILES: usize = 10_000;
const MAX_FEATURES: usize = 5_000;
const SCAN_PAGE: usize = 20;
const PAGE: usize = 10;
const FIELDS: &[&str] = &[
    "purpose",
    "trigger",
    "inputs",
    "outputs",
    "behavior",
    "data_changes",
    "errors",
    "permissions",
];
const KINDS: &[&str] = &[
    "screen",
    "tab",
    "dialog",
    "http",
    "rpc",
    "websocket",
    "event",
    "job",
    "cli",
    "lifecycle",
    "shared",
];

#[derive(Clone, Debug, Default, Serialize)]
pub struct Inventory {
    pub active: bool,
    pub revision: u64,
    pub sequence: u64,
    pub generation: u64,
    pub scope: String,
    pub manifest: Vec<String>,
    pub next_file: usize,
    pub files: BTreeMap<String, FileRecord>,
    pub features: Vec<Feature>,
    pub last_audit: Option<Value>,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct FileRecord {
    pub hash: String,
    pub error: Option<String>,
    pub reviewed_hash: Option<String>,
    pub review_note: String,
    pub source_ids: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Feature {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub hash: String,
    pub anchor: String,
    pub status: String,
    pub reason: String,
    pub manual: bool,
    pub present: bool,
    pub documentation: Option<Binding>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Binding {
    pub path: String,
    pub section: String,
    pub source_hash: String,
    pub fields: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Issue {
    pub kind: String,
    pub id: Option<String>,
    pub path: Option<String>,
    pub message: String,
    pub next_action: String,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Audit {
    pub active: bool,
    pub ready: bool,
    pub revision: u64,
    pub fingerprint: String,
    #[serde(skip)]
    pub progress_fingerprint: String,
    pub total: usize,
    pub documented: usize,
    pub excluded: usize,
    pub issues: Vec<Issue>,
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "capability_inventory",
            optional: false,
            read_only: false,
            description: "Collect and manage a shared UI/server function inventory. Begin comprehensive documentation with action=scan (20 source/config files per call); repeat with next_cursor until scan_complete. Discovery is heuristic, NOT exhaustive: list view=files and review EVERY file with review_file after reading all source lines (page large files), registering missed functions with register. list defaults to features, 10 per page (max 20), offset>0 requires expected_revision. classify a candidate with decision=include/exclude; include requires kind, exclude requires a concrete reason (false positive/out of requested scope). No delete/reset: exclusions and source changes remain visible. register needs title/kind/path/start_line/end_line/expected_hash; optionally id to refresh an existing manual feature after a source edit (or classify it as excluded with a reason). review_file needs path/expected_hash/note/source_ids from delivered reads (all file lines must have been delivered; empty files allow source_ids=[]), and all current file candidates classified. All mutations except scan require expected_revision. Inventory is independent of the 100-item to-do limit. Re-scan after source changes; IDs of unchanged anchors and unaffected links survive. Documentation uses documentation_coverage. In verification reserve, new investigation items may use the exact title of an included feature. Budget/conflict errors return applied=false; correct them and continue actual work.",
            parameters: schema(
                json!({"action":action(&["scan","list","classify","register","review_file"]),"cursor":string(),"expected_revision":number(),"view":action(&["features","files"]),"offset":number(),"limit":number(),"id":string(),"decision":action(&["include","exclude"]),"kind":action(KINDS),"title":string(),"reason":string(),"path":string(),"start_line":number(),"end_line":number(),"expected_hash":string(),"note":string(),"source_ids":strings()}),
                &["action"],
            ),
        },
        ToolSpec {
            name: "documentation_coverage",
            optional: false,
            read_only: false,
            description: "Link each included capability to actual Markdown text and audit missing documentation. bind requires expected_revision, id, path, section (exact section_path from document_inspect), expected_hash of the document, and fields: an object mapping ALL purpose/trigger/inputs/outputs/behavior/data_changes/errors/permissions to exact substantive excerpts inside that section (12..1000 characters each). For a nonapplicable field, quote the document's explicit explanation; do not invent applicability. Checks literal evidence and fresh source hashes, NOT semantic truth. audit pages issues (10 default, max20); offset>0 requires expected_fingerprint from the previous audit. enqueue registers the next missing work in task_plan without exceeding 100 pending items or duplicating existing tasks. Audit scans the original source/config scope again, detects new/deleted/modified files and unresolved candidates/reviews, validates every included feature and bound excerpt. A changed unrelated document section does not invalidate intact excerpts. Never equate a nonempty section or a completed to-do with coverage. Same snapshot issues have a stable fingerprint.",
            parameters: schema(
                json!({"action":action(&["bind","audit","enqueue"]),"expected_revision":number(),"expected_fingerprint":string(),"offset":number(),"limit":number(),"id":string(),"path":string(),"section":string(),"expected_hash":string(),"fields":{"type":"object","properties":FIELDS.iter().map(|f|((*f).to_string(),string())).collect::<serde_json::Map<_,_>>(),"required":FIELDS,"additionalProperties":false}}),
                &["action"],
            ),
        },
    ]
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum InventoryOp {
    Scan {
        cursor: Option<String>,
    },
    List {
        view: Option<String>,
        offset: Option<usize>,
        limit: Option<usize>,
        expected_revision: Option<u64>,
    },
    Classify {
        expected_revision: u64,
        id: String,
        decision: String,
        kind: Option<String>,
        title: Option<String>,
        reason: Option<String>,
    },
    Register {
        expected_revision: u64,
        id: Option<String>,
        title: String,
        kind: String,
        path: String,
        start_line: usize,
        end_line: usize,
        expected_hash: String,
    },
    ReviewFile {
        expected_revision: u64,
        path: String,
        expected_hash: String,
        note: String,
        source_ids: Vec<String>,
    },
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum CoverageOp {
    Bind {
        expected_revision: u64,
        id: String,
        path: String,
        section: String,
        expected_hash: String,
        fields: BTreeMap<String, String>,
    },
    Audit {
        offset: Option<usize>,
        limit: Option<usize>,
        expected_fingerprint: Option<String>,
    },
    Enqueue {
        expected_fingerprint: Option<String>,
    },
}

fn short(value: &str, max: usize) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > max {
        bail!("Use nonempty text of at most {max} characters");
    }
    Ok(value.into())
}
fn revision(s: &Session, expected: u64) -> Result<()> {
    if s.capabilities.revision != expected {
        bail!("Inventory revision changed; copy revision from capability_inventory list");
    }
    Ok(())
}
fn relative(s: &Session, path: &str) -> Result<String> {
    let root = s.project.root.canonicalize()?;
    let allowed = read_path(&s.project, path)?;
    Ok(allowed
        .strip_prefix(&root)?
        .to_string_lossy()
        .replace('\\', "/"))
}
fn scope(s: &Session) -> String {
    hash(json!({"root":s.project.root,"include":s.project.include,"exclude":s.project.exclude,"output":s.project.output}).to_string().as_bytes())
}
fn source_paths(s: &Session, cancel: &CancellationToken) -> Result<Vec<String>> {
    let root = s.project.root.canonicalize()?;
    let output = output_path(&s.project)?;
    let mut paths = Vec::new();
    for path in candidate_paths(&s.project, None, cancel)? {
        if path == output || !discovery::in_scope(&path) {
            continue;
        }
        paths.push(
            path.strip_prefix(&root)?
                .to_string_lossy()
                .replace('\\', "/"),
        );
        if paths.len() > MAX_FILES {
            bail!(
                "Inventory supports at most {MAX_FILES} source/config files in project scope; explicitly narrow project include/exclude before scanning"
            );
        }
    }
    Ok(paths)
}
fn current_text(s: &Session, path: &str, expected: &str) -> Result<String> {
    let content = read_text(&read_path(&s.project, path)?)?;
    if hash(content.as_bytes()) != expected {
        bail!(
            "Source/document changed; read its current hash and refresh the inventory or binding"
        );
    }
    Ok(content)
}
fn cursor(inv: &Inventory) -> Option<String> {
    (inv.next_file < inv.manifest.len()).then(|| format!("{}:{}", inv.generation, inv.next_file))
}

pub fn summary(s: &Session) -> Value {
    let i = &s.capabilities;
    if !i.active {
        return json!({"active":false});
    }
    json!({"active":i.active,"revision":i.revision,"scan_complete":i.active&&cursor(i).is_none(),"next_cursor":cursor(i),"files":i.manifest.len(),"scanned_files":i.next_file,"reviewed_files":i.files.values().filter(|f|f.reviewed_hash.as_ref()==Some(&f.hash)&&f.error.is_none()).count(),"features":i.features.iter().filter(|f|f.present).count(),"unclassified":i.features.iter().filter(|f|f.present&&f.status=="candidate").count(),"linked":i.features.iter().filter(|f|f.present&&f.status=="included"&&f.documentation.is_some()).count(),"limits":{"files":MAX_FILES,"features":MAX_FEATURES},"last_audit":i.last_audit,"included":i.features.iter().filter(|f|f.present&&f.status=="included").count(),"excluded":i.features.iter().filter(|f|f.present&&f.status=="excluded").count(),"scope":"Project include/exclude and ignore rules; source/config formats plus unknown text-source extensions. Ordinary Markdown documentation, static media, lockfiles, OS metadata and build dependencies excluded; Markdown in pages/routes is source. Discovery is heuristic; file review and runtime/config evidence may still be required."})
}
fn view(s: &Session, files: bool, offset: usize, limit: usize) -> Value {
    let rows: Vec<Value> = if files {
        s.capabilities
            .files
            .iter()
            .map(|(path, file)| json!({"path":path,"state":file}))
            .collect()
    } else {
        s.capabilities.features.iter().map(|f| json!(f)).collect()
    };
    let end = offset.saturating_add(limit.clamp(1, 20)).min(rows.len());
    json!({"revision":s.capabilities.revision,"summary":summary(s),"view":if files {"files"}else{"features"},"offset":offset,"items":rows.iter().skip(offset).take(limit.clamp(1,20)).collect::<Vec<_>>(),"next_offset":(end<rows.len()).then_some(end),"total_items":rows.len()})
}
fn insert(inv: &mut Inventory, mut item: Feature) -> Result<String> {
    if let Some(old) = inv
        .features
        .iter_mut()
        .find(|f| f.path == item.path && f.anchor == item.anchor)
    {
        let changed = old.hash != item.hash;
        old.present = true;
        old.start_line = item.start_line;
        old.end_line = item.end_line;
        old.hash = item.hash;
        if item.manual {
            old.title = item.title;
            old.kind = item.kind;
            old.reason.clear();
        }
        if changed {
            old.status = "candidate".into();
            old.reason.clear();
        }
        return Ok(old.id.clone());
    }
    if inv.features.len() >= MAX_FEATURES {
        bail!(
            "Feature capacity reached; retained inventory unchanged. Narrow explicit project scope for a new task; never drop required functions"
        );
    }
    inv.sequence = inv
        .sequence
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("Inventory ID capacity reached"))?;
    item.id = format!("F{}", inv.sequence);
    let id = item.id.clone();
    inv.features.push(item);
    Ok(id)
}

fn scan(
    s: &Session,
    inv: &mut Inventory,
    requested: Option<String>,
    cancel: &CancellationToken,
) -> Result<()> {
    let paths = source_paths(s, cancel)?;
    if inv.active && inv.scope != scope(s) {
        bail!(
            "Project scope changed during this task; restore the scope or start a new user task, so required functions cannot disappear silently"
        );
    }
    if let Some(requested) = requested {
        if cursor(inv).as_deref() != Some(&requested) {
            bail!("Stale scan cursor; copy next_cursor from the current inventory summary");
        }
        if paths != inv.manifest {
            bail!("Source file set changed; restart scan without cursor");
        }
    } else if !inv.active || cursor(inv).is_none() || paths != inv.manifest {
        inv.active = true;
        inv.scope = scope(s);
        inv.generation = inv.generation.saturating_add(1);
        inv.next_file = 0;
        inv.manifest = paths.clone();
        inv.files.retain(|path, _| paths.contains(path));
        for item in &mut inv.features {
            if !paths.contains(&item.path) {
                item.present = false;
            }
        }
    }
    let end = (inv.next_file + SCAN_PAGE).min(paths.len());
    for path in &paths[inv.next_file..end] {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        let content = read_path(&s.project, path).and_then(|p| read_text(&p));
        match content {
            Err(error) => {
                inv.files.insert(
                    path.clone(),
                    FileRecord {
                        error: Some(error.to_string().chars().take(200).collect()),
                        ..Default::default()
                    },
                );
            }
            Ok(content) => {
                let digest = hash(content.as_bytes());
                if inv
                    .files
                    .get(path)
                    .is_some_and(|f| f.hash == digest && f.error.is_none())
                {
                    continue;
                }
                for item in inv.features.iter_mut().filter(|f| f.path == *path) {
                    item.present = false;
                }
                let candidates = discovery::detect(path, &content, cancel)?;
                for (kind, start, end, anchor, title) in candidates {
                    insert(
                        inv,
                        Feature {
                            id: String::new(),
                            title,
                            kind,
                            path: path.clone(),
                            start_line: start,
                            end_line: end,
                            hash: digest.clone(),
                            anchor,
                            status: "candidate".into(),
                            reason: String::new(),
                            manual: false,
                            present: true,
                            documentation: None,
                        },
                    )?;
                }
                // Manual discoveries survive changes conservatively and must
                // be re-registered at their current source range before binding.
                for item in inv
                    .features
                    .iter_mut()
                    .filter(|f| f.path == *path && f.manual && !f.present)
                {
                    item.present = true;
                    item.status = "candidate".into();
                }
                inv.files.insert(
                    path.clone(),
                    FileRecord {
                        hash: digest,
                        ..Default::default()
                    },
                );
            }
        }
    }
    inv.next_file = end;
    Ok(())
}

fn review_sources(s: &Session, path: &str, digest: &str, ids: &[String]) -> Result<()> {
    let allowed = read_path(&s.project, path)?;
    let text = read_text(&allowed)?;
    if text.is_empty() && ids.is_empty() {
        return Ok(());
    }
    if ids.is_empty() || ids.len() > 32 {
        bail!(
            "Supply 1..32 source_ids from delivered reads used to review this file (empty files accept [])"
        );
    }
    let (_, covered) = coverage::report(s, &allowed, &text, 0, 1);
    if let Some(first) = covered.iter().position(|read| !read) {
        bail!(
            "File review requires every source line delivered; {} of {} lines read, first unread line {}. Continue file_read from that line before review_file",
            covered.iter().filter(|read| **read).count(),
            covered.len(),
            first + 1
        );
    }
    for source in s.source_refs(ids)? {
        if source.hash.as_deref() != Some(digest)
            || source
                .path
                .as_deref()
                .and_then(|p| read_path(&s.project, p).ok())
                .as_ref()
                != Some(&allowed)
        {
            bail!("Review evidence must refer to this current source file");
        }
        let (Some(start), Some(end)) = (source.start_line, source.end_line) else {
            bail!("Use line-based file evidence");
        };
        if start == 0
            || end < start
            || end > covered.len()
            || !covered[start - 1..end].iter().all(|v| *v)
        {
            bail!("Review evidence was not fully delivered; read the required source range");
        }
    }
    Ok(())
}

pub fn execute(
    s: &mut Session,
    name: &str,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    // All validation and mutations use a private inventory; malformed arguments,
    // stale cursors and capacity are recoverable and never partly change it.
    match execute_inner(s, name, args, cancel) {
        Ok(result) => Ok(result),
        Err(error) if !cancel.is_cancelled() => Ok(
            json!({"applied":false,"revision":s.capabilities.revision,"reason":error.to_string().chars().take(400).collect::<String>(),"summary":summary(s),"guidance":"Correct the indicated arguments or finish current work; the inventory and plan were retained."}),
        ),
        Err(error) => Err(error),
    }
}
fn execute_inner(
    s: &mut Session,
    name: &str,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    let mut inv = s.capabilities.clone();
    let mut result = json!({});
    if name == "capability_inventory" {
        let op: InventoryOp = serde_json::from_value(args.clone())?;
        match op {
            InventoryOp::List {
                view: which,
                offset,
                limit,
                expected_revision,
            } => {
                let offset = offset.unwrap_or(0);
                if offset > 0 && expected_revision != Some(inv.revision) {
                    bail!("Paged list requires current expected_revision");
                }
                if which
                    .as_deref()
                    .is_some_and(|v| !matches!(v, "features" | "files"))
                {
                    bail!("view must be features or files");
                }
                return Ok(view(
                    s,
                    which.as_deref() == Some("files"),
                    offset,
                    limit.unwrap_or(PAGE),
                ));
            }
            InventoryOp::Scan { cursor } => scan(s, &mut inv, cursor, cancel)?,
            InventoryOp::Classify {
                expected_revision,
                id,
                decision,
                kind,
                title,
                reason,
            } => {
                revision(s, expected_revision)?;
                let item = inv
                    .features
                    .iter_mut()
                    .find(|f| f.id == id && f.present)
                    .ok_or_else(|| anyhow::anyhow!("Unknown current feature ID"))?;
                // A changed manual reference can be excluded with a reason;
                // including it requires register(id=...) to refresh its range.
                if decision == "exclude" && item.manual {
                    let current = inv
                        .files
                        .get(&item.path)
                        .ok_or_else(|| anyhow::anyhow!("Scan this file first"))?;
                    current_text(s, &item.path, &current.hash)?;
                    item.hash = current.hash.clone();
                } else {
                    current_text(s, &item.path, &item.hash)?;
                }
                match decision.as_str() {
                    "include" => {
                        let kind = kind.ok_or_else(|| anyhow::anyhow!("include requires kind"))?;
                        if !KINDS.contains(&kind.as_str()) {
                            bail!("Unknown kind");
                        }
                        item.kind = kind;
                        item.status = "included".into();
                        item.reason.clear();
                    }
                    "exclude" => {
                        item.reason = short(reason.as_deref().unwrap_or(""), 400)?;
                        item.status = "excluded".into();
                    }
                    _ => bail!("Use include or exclude"),
                }
                if let Some(title) = title {
                    item.title = short(&title, 160)?;
                }
            }
            InventoryOp::Register {
                expected_revision,
                id: existing_id,
                title,
                kind,
                path,
                start_line,
                end_line,
                expected_hash,
            } => {
                revision(s, expected_revision)?;
                if !inv.active {
                    bail!("Start capability_inventory scan first");
                }
                if !KINDS.contains(&kind.as_str()) {
                    bail!("Unknown kind");
                }
                let path = relative(s, &path)?;
                if !inv.manifest.contains(&path) {
                    bail!("File is outside the inventory manifest; scan new files first");
                }
                let content = current_text(s, &path, &expected_hash)?;
                let lines: Vec<_> = content.lines().collect();
                if start_line == 0
                    || end_line < start_line
                    || end_line > lines.len()
                    || end_line - start_line >= 200
                {
                    bail!("Choose an existing 1..200-line source range");
                }
                let title = short(&title, 160)?;
                // Several dynamic functions may share one registration line.
                // Include the function identity as well as the source anchor.
                let anchor = hash(
                    json!([kind, title, lines[start_line - 1..end_line].join("\n")])
                        .to_string()
                        .as_bytes(),
                );
                let mut feature = Feature {
                    id: String::new(),
                    title,
                    kind,
                    path: path.clone(),
                    start_line,
                    end_line,
                    hash: expected_hash,
                    anchor: format!("manual:{anchor}"),
                    status: "included".into(),
                    reason: String::new(),
                    manual: true,
                    present: true,
                    documentation: None,
                };
                let id = if let Some(id) = existing_id {
                    let old = inv
                        .features
                        .iter_mut()
                        .find(|f| f.id == id && f.manual)
                        .ok_or_else(|| {
                            anyhow::anyhow!("register id must identify an existing manual feature")
                        })?;
                    if old.path != path {
                        bail!("A refreshed manual feature must retain its original source path");
                    }
                    feature.id = id.clone();
                    *old = feature;
                    id
                } else {
                    let id = insert(&mut inv, feature)?;
                    inv.features.iter_mut().find(|f| f.id == id).unwrap().status =
                        "included".into();
                    id
                };
                if let Some(file) = inv.files.get_mut(&path) {
                    file.reviewed_hash = None;
                }
                result["id"] = json!(id);
            }
            InventoryOp::ReviewFile {
                expected_revision,
                path,
                expected_hash,
                note,
                source_ids,
            } => {
                revision(s, expected_revision)?;
                let path = relative(s, &path)?;
                current_text(s, &path, &expected_hash)?;
                review_sources(s, &path, &expected_hash, &source_ids)?;
                if inv
                    .features
                    .iter()
                    .any(|f| f.present && f.path == path && f.status == "candidate")
                {
                    bail!(
                        "Classify all file candidates and register missed functions before review_file"
                    );
                }
                let file = inv
                    .files
                    .get_mut(&path)
                    .ok_or_else(|| anyhow::anyhow!("Scan this file first"))?;
                if file.hash != expected_hash {
                    bail!("Re-scan changed source before review_file");
                }
                file.review_note = short(&note, 600)?;
                file.source_ids = source_ids;
                file.reviewed_hash = Some(expected_hash);
                file.error = None;
            }
        }
    } else {
        let op: CoverageOp = serde_json::from_value(args.clone())?;
        match op {
            CoverageOp::Audit {
                offset,
                limit,
                expected_fingerprint,
            } => {
                let audit = audit(s, cancel)?;
                let offset = offset.unwrap_or(0);
                if offset > 0 && expected_fingerprint.as_deref() != Some(&audit.fingerprint) {
                    bail!("Audit changed; restart at offset=0 or copy expected_fingerprint");
                }
                remember(s, &audit);
                return Ok(audit_page(&audit, offset, limit.unwrap_or(PAGE)));
            }
            CoverageOp::Enqueue {
                expected_fingerprint,
            } => {
                let audit = audit(s, cancel)?;
                if expected_fingerprint
                    .as_deref()
                    .is_some_and(|v| v != audit.fingerprint)
                {
                    bail!("Audit changed; read a fresh audit before enqueue");
                }
                remember(s, &audit);
                enqueue(s, &audit);
                return Ok(
                    json!({"applied":true,"revision":s.capabilities.revision,"audit":audit_page(&audit,0,PAGE),"plan":task_plan::view(&s.task,0,PAGE)}),
                );
            }
            CoverageOp::Bind {
                expected_revision,
                id,
                path,
                section,
                expected_hash,
                fields,
            } => {
                revision(s, expected_revision)?;
                let item = inv
                    .features
                    .iter_mut()
                    .find(|f| f.id == id && f.present && f.status == "included")
                    .ok_or_else(|| anyhow::anyhow!("Bind an included current feature ID"))?;
                current_text(s, &item.path, &item.hash)?;
                let allowed = read_path(&s.project, &path)?;
                if inv
                    .manifest
                    .iter()
                    .any(|p| s.project.root.join(p).canonicalize().ok().as_ref() == Some(&allowed))
                {
                    bail!("Documentation must be a separate Markdown artifact, not source code");
                }
                if !allowed
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("md"))
                {
                    bail!("Documentation bindings require a Markdown file");
                }
                let doc = current_text(s, &path, &expected_hash)?;
                let heading = documentation::resolve_heading(&doc, &section)?;
                let section = documentation::heading_path(&doc, heading.start)?;
                let body = &doc[heading.start..heading.end];
                check_fields(&fields, body)?;
                item.documentation = Some(Binding {
                    path: allowed.display().to_string(),
                    section,
                    source_hash: item.hash.clone(),
                    fields,
                });
            }
        }
    }
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    inv.revision = inv.revision.saturating_add(1);
    inv.last_audit = None;
    if serde_json::to_vec(&inv)?.len() > s.config.memory_bytes / 2 {
        bail!(
            "Inventory metadata budget reached; shorten notes/excerpts, preserve required features and continue existing work"
        );
    }
    s.capabilities = inv;
    s.completion_review.required = true;
    s.completion_review.approved = false;
    result["applied"] = json!(true);
    result["revision"] = json!(s.capabilities.revision);
    result["summary"] = summary(s);
    Ok(result)
}

fn check_fields(fields: &BTreeMap<String, String>, body: &str) -> Result<()> {
    if fields.len() != FIELDS.len() || FIELDS.iter().any(|key| !fields.contains_key(*key)) {
        bail!(
            "fields must contain purpose, trigger, inputs, outputs, behavior, data_changes, errors and permissions"
        );
    }
    for (name, quote) in fields {
        if !(12..=1000).contains(&quote.trim().chars().count()) || !body.contains(quote.as_str()) {
            bail!("Missing substantive exact excerpt for {name} inside the bound document section");
        }
        if !quote
            .lines()
            .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        {
            bail!("A heading alone is not evidence for {name}");
        }
    }
    Ok(())
}

pub fn audit_page(audit: &Audit, offset: usize, limit: usize) -> Value {
    let limit = limit.clamp(1, 20);
    let end = offset.saturating_add(limit).min(audit.issues.len());
    json!({"active":audit.active,"ready":audit.ready,"semantic_verified":false,"revision":audit.revision,"fingerprint":audit.fingerprint,"total":audit.total,"documented":audit.documented,"excluded":audit.excluded,"issue_count":audit.issues.len(),"offset":offset,"items":audit.issues.iter().skip(offset).take(limit).collect::<Vec<_>>(),"next_offset":(end<audit.issues.len()).then_some(end)})
}
fn issue(
    issues: &mut Vec<Issue>,
    kind: &str,
    id: Option<&str>,
    path: Option<&str>,
    message: impl Into<String>,
    action: impl Into<String>,
) {
    issues.push(Issue {
        kind: kind.into(),
        id: id.map(str::to_string),
        path: path.map(str::to_string),
        message: message.into(),
        next_action: action.into().chars().take(160).collect(),
    });
}

pub fn audit(s: &Session, cancel: &CancellationToken) -> Result<Audit> {
    let started = std::time::Instant::now();
    let check_budget = || -> Result<()> {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        if started.elapsed().as_secs() >= s.config.tool_timeout_secs {
            bail!(
                "Documentation audit exceeded tool_timeout_secs; inventory retained. Increase the tool time budget before retrying this full-scope audit"
            );
        }
        Ok(())
    };
    let inv = &s.capabilities;
    if !inv.active {
        return Ok(Audit::default());
    }
    let mut a = Audit {
        active: true,
        revision: inv.revision,
        ..Default::default()
    };
    let paths = source_paths(s, cancel)?;
    let mut versions = BTreeMap::new();
    if inv.scope != scope(s) {
        issue(
            &mut a.issues,
            "scope_changed",
            None,
            None,
            "Project scope changed",
            "기능 조사 범위를 원래 프로젝트 설정으로 복원하고 다시 점검합니다.",
        );
    }
    if paths != inv.manifest {
        issue(
            &mut a.issues,
            "manifest_changed",
            None,
            None,
            "Source/config file set changed",
            "capability_inventory scan으로 추가·삭제된 소스 파일을 반영합니다.",
        );
    }
    if cursor(inv).is_some() {
        issue(
            &mut a.issues,
            "scan_pending",
            None,
            None,
            "Discovery has unprocessed file pages",
            "capability_inventory scan의 next_cursor로 남은 파일 수집을 완료합니다.",
        );
    }
    if inv.manifest.is_empty() {
        issue(
            &mut a.issues,
            "empty_scope",
            None,
            None,
            "No source/config files in the selected project scope",
            "프로젝트 경로와 분석 범위를 확인해 기능 목록의 근거를 확보합니다.",
        );
    }
    for path in &inv.manifest {
        check_budget()?;
        let version = read_path(&s.project, path).and_then(|p| hash_file(&p)).ok();
        versions.insert(path.clone(), version.clone());
        let file = inv.files.get(path);
        if file.is_none() {
            continue;
        }
        let file = file.unwrap();
        if file.error.is_some() || version.as_deref() != Some(&file.hash) {
            issue(
                &mut a.issues,
                "source_changed",
                None,
                Some(path),
                "Source unavailable or changed since scanning",
                format!("변경된 {path}의 기능을 capability_inventory scan으로 다시 조사합니다."),
            );
        } else if file.reviewed_hash.as_deref() != Some(&file.hash) {
            issue(
                &mut a.issues,
                "file_unreviewed",
                None,
                Some(path),
                "Heuristic detection needs source review, including undiscovered registrations",
                format!(
                    "{path}의 진입점·설정을 확인하고 누락 기능 등록 후 review_file을 기록합니다."
                ),
            );
        }
    }
    let mut documents = BTreeMap::new();
    let mut pending: BTreeMap<&str, Vec<&Feature>> = BTreeMap::new();
    for item in inv.features.iter().filter(|f| f.present) {
        check_budget()?;
        if item.status == "excluded" {
            a.excluded += 1;
            continue;
        }
        a.total += 1;
        if item.status != "included" {
            issue(
                &mut a.issues,
                "unclassified",
                Some(&item.id),
                Some(&item.path),
                "Candidate needs inclusion or justified exclusion",
                format!(
                    "{}: {}의 기능 여부를 확인해 classify합니다.",
                    item.id, item.title
                ),
            );
            continue;
        }
        let Some(binding) = &item.documentation else {
            issue(
                &mut a.issues,
                "undocumented",
                Some(&item.id),
                Some(&item.path),
                "No document binding",
                format!(
                    "{}: {}를 문서화하고 documentation_coverage bind로 연결합니다.",
                    item.id, item.title
                ),
            );
            continue;
        };
        pending.entry(&binding.path).or_default().push(item);
    }
    // Group by artifact: read and parse each document once, then release its
    // body. Hundreds of documents must not accumulate in audit memory.
    for (path, items) in pending {
        check_budget()?;
        let doc = read_path(&s.project, path).and_then(|p| read_text(&p)).ok();
        documents.insert(path, doc.as_ref().map(|d| hash(d.as_bytes())));
        let mut sections = BTreeMap::new();
        if let Some(doc) = &doc {
            let headings = documentation::headings(doc);
            for (heading, path) in headings.iter().zip(documentation::heading_paths(&headings)) {
                sections
                    .entry(path)
                    .and_modify(|range| *range = None)
                    .or_insert(Some((heading.start, heading.end)));
            }
        }
        for item in items {
            check_budget()?;
            let binding = item.documentation.as_ref().unwrap();
            let valid = doc.as_ref().is_some_and(|doc| {
                sections
                    .get(&binding.section)
                    .and_then(|r| *r)
                    .is_some_and(|(start, end)| {
                        check_fields(&binding.fields, &doc[start..end]).is_ok()
                    })
            }) && versions.get(&item.path).and_then(|v| v.as_deref())
                == Some(&binding.source_hash)
                && item.hash == binding.source_hash;
            if valid {
                a.documented += 1;
            } else {
                issue(
                    &mut a.issues,
                    "stale_documentation",
                    Some(&item.id),
                    Some(&binding.path),
                    "Document excerpts/section or source version no longer match",
                    format!(
                        "{}: {}의 문서와 현재 소스를 대조하고 bind를 갱신합니다.",
                        item.id, item.title
                    ),
                );
            }
        }
    }
    a.ready = a.issues.is_empty();
    a.fingerprint=hash(json!({"scope":scope(s),"manifest":paths,"versions":versions,"documents":documents,"inventory":inv.features,"reviews":inv.files.iter().map(|(p,f)|(p,&f.reviewed_hash)).collect::<BTreeMap<_,_>>(),"issues":a.issues,"total":a.total,"documented":a.documented,"excluded":a.excluded}).to_string().as_bytes());
    // Rewording already-bound prose must not prolong retries for unchanged gaps.
    a.progress_fingerprint=hash(json!({"scope":scope(s),"manifest":paths,"versions":versions,"gaps":a.issues.iter().map(|i|(&i.kind,&i.id,&i.path)).collect::<Vec<_>>()}).to_string().as_bytes());
    Ok(a)
}

pub fn allows_investigation(s: &Session, args: &Value) -> bool {
    s.capabilities.active
        && args["title"].as_str().is_some_and(|title| {
            s.capabilities
                .features
                .iter()
                .any(|f| f.present && f.status == "included" && f.title == title)
        })
}

pub fn enqueue(s: &mut Session, audit: &Audit) {
    let mut seen = BTreeSet::new();
    // Resolve discovery/classification prerequisites before writing. Same tasks
    // are reused and only the available 100-item execution window is filled.
    for item in &audit.issues {
        let text = &item.next_action;
        if !seen.insert(text.clone()) {
            continue;
        }
        let existing = s.task.todos.iter().find(|t| t.text == *text);
        let operation = match existing {
            Some(t) if !t.done => continue,
            Some(t) => {
                json!({"op":"reopen","id":t.id,"reason":"기능 문서 누락 점검에서 보완이 필요합니다."})
            }
            None => json!({"op":"insert","texts":[text]}),
        };
        let mut operations = vec![operation];
        if let Some(item) = existing {
            operations.push(json!({"op":"move","id":item.id}));
        }
        let result = task_plan::execute(
            s,
            &json!({"action":"apply","expected_revision":s.task.plan_revision,"operations":operations}),
        );
        if result.as_ref().is_ok_and(|r| r["applied"] == false) {
            break;
        }
    }
    if !audit.ready {
        s.last_error=Some("documentation_coverage_pending: use capability_inventory and documentation_coverage to resolve missing discovery/review/documentation; completing to-dos alone does not satisfy this gate".into());
    }
}

/// Cache only the last observation for UI/context; completion always audits live.
pub fn remember(s: &mut Session, audit: &Audit) {
    s.capabilities.last_audit = Some(audit_page(audit, 0, 3));
}

pub fn public_view(s: &Session) -> Value {
    json!({"summary":summary(s),"items":s.capabilities.features.iter().filter(|f|f.present).take(10).map(|f|json!({"id":f.id,"title":f.title,"kind":f.kind,"status":f.status,"linked":f.documentation.is_some()})).collect::<Vec<_>>()})
}

/// Count material catalog outcomes; changing wording/revisions or listing the
/// same records does not masquerade as progress in the agent's recovery loop.
pub fn progress(s: &Session) -> [usize; 5] {
    let i = &s.capabilities;
    [
        i.files.len(),
        i.features.iter().filter(|f| f.present).count(),
        i.features
            .iter()
            .filter(|f| f.present && f.status != "candidate")
            .count(),
        i.files
            .values()
            .filter(|f| f.reviewed_hash.as_ref() == Some(&f.hash))
            .count(),
        i.features
            .iter()
            .filter(|f| f.present && f.status == "included" && f.documentation.is_some())
            .count(),
    ]
}
