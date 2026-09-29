//! Workspace syntax navigation. Indexes are snapshots, never source evidence.
mod relations;

use super::structure::{SyntaxFile, check_budget};
use super::*;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const MAX_FILES: usize = 2_048;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_SYMBOLS: usize = 100_000;

struct IndexedFile {
    syntax: SyntaxFile,
    symbols: Vec<Value>,
}

struct Workspace {
    files: Vec<IndexedFile>,
    root: PathBuf,
    digest: String,
    matched_files: usize,
    skipped_files: usize,
}

impl Workspace {
    fn load(
        s: &Session,
        args: &Value,
        target: Option<&Path>,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<Self> {
        check_budget(cancel, deadline)?;
        let root = s.project.root.canonicalize()?;
        let exact = if target.is_none() {
            args["path"].as_str()
        } else {
            None
        };
        let mut paths = if let Some(path) = exact {
            if path_glob(args)?.is_some() {
                bail!("conflicting_path_filters: pass path or path_glob, not both");
            }
            let path = read_path(&s.project, path)?;
            if path.is_dir() {
                candidate_paths_scoped(&s.project, None, cancel, Some(&path), Some(deadline))?
            } else {
                vec![path]
            }
        } else {
            candidate_paths_scoped(&s.project, path_glob(args)?, cancel, None, Some(deadline))?
        };
        if let Some(target) = target
            && !paths.iter().any(|p| p == target)
        {
            paths.push(target.to_owned());
            paths.sort();
        }
        let matched_files = paths.len();
        let mut files = Vec::new();
        let mut skipped_files = 0;
        let mut bytes = 0usize;
        let mut symbols_count = 0usize;
        let mut fingerprint = Sha256::new();
        for path in paths {
            check_budget(cancel, deadline)?;
            fingerprint.update(path.as_os_str().as_encoded_bytes());
            fingerprint.update([0]);
            if structure::language(&path).is_err() {
                skipped_files += 1;
                continue;
            }
            let Some(path_text) = path.to_str() else {
                // Tool paths are UTF-8. A lossy conversion could reopen a
                // different file or fail the entire workspace search.
                skipped_files += 1;
                continue;
            };
            // Recheck access on every call; an old syntax cache never grants access.
            let path = read_path(&s.project, path_text)?;
            let Some(source) = search_text(&path)? else {
                skipped_files += 1;
                continue;
            };
            bytes += source.len();
            if files.len() >= MAX_FILES || bytes > MAX_BYTES {
                bail!(
                    "navigation_too_broad: limit path_glob; at most {MAX_FILES} supported files and 32MiB per syntax snapshot"
                );
            }
            let syntax = SyntaxFile::parse(path, source, cancel, deadline)?;
            fingerprint.update(syntax.digest.as_bytes());
            // Search needs only filtered declarations. Extracting an unfiltered
            // outline first can reject a narrow query in a large generated file.
            let filters = if target.is_none() {
                args.clone()
            } else {
                json!({})
            };
            let (symbols, _) = syntax.symbols(&filters, None, cancel, deadline)?;
            symbols_count += symbols.len();
            if symbols_count > MAX_SYMBOLS {
                bail!("navigation_too_broad: limit path_glob; too many declarations");
            }
            files.push(IndexedFile { syntax, symbols });
        }
        Ok(Self {
            files,
            root,
            digest: format!("{:x}", fingerprint.finalize()),
            matched_files,
            skipped_files,
        })
    }

    fn relative(&self, path: &Path) -> String {
        let relative = path
            .strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy();
        if std::path::MAIN_SEPARATOR == '\\' {
            relative.replace('\\', "/")
        } else {
            relative.into_owned()
        }
    }

    fn symbol(&self, file: usize, symbol: &Value) -> Value {
        let path = self.relative(&self.files[file].syntax.path);
        let mut result = symbol.clone();
        result
            .as_object_mut()
            .unwrap()
            .retain(|key, _| !key.starts_with("signature") && key != "kind");
        result["path"] = json!(path);
        result["hash"] = json!(self.files[file].syntax.digest);
        result["location"] = json!(format!(
            "{path}:{}-{}",
            symbol["start_line"], symbol["end_line"]
        ));
        result
    }

    fn metadata(&self) -> Value {
        let errors: Vec<_> = self
            .files
            .iter()
            .filter(|f| f.syntax.tree.root_node().has_error())
            .map(|f| self.relative(&f.syntax.path))
            .collect();
        json!({"engine":"tree-sitter","matched_files":self.matched_files,"scanned_files":self.files.len(),
            "skipped_files":self.skipped_files,"parse_error_count":errors.len(),"parse_error_files":errors.iter().take(12).collect::<Vec<_>>(),
            "navigation_only":true,"semantic_verified":false})
    }
}

fn request_fingerprint(workspace: &Workspace, name: &str, args: &Value) -> String {
    let mut filters = args.clone();
    for key in ["cursor", "limit"] {
        filters.as_object_mut().unwrap().remove(key);
    }
    // Legacy and preferred glob spellings denote the same request.
    if let Some(glob) = filters.as_object_mut().unwrap().remove("pattern") {
        filters["path_glob"] = glob;
    }
    hash(
        serde_json::to_vec(&(name, &workspace.digest, filters))
            .unwrap()
            .as_slice(),
    )
}

fn page(
    mut metadata: Value,
    args: &Value,
    fingerprint: &str,
    field: &str,
    rows: Vec<Value>,
) -> Result<Value> {
    let start = page_cursor(args, fingerprint)?;
    if start > rows.len() {
        bail!(INVALID_CURSOR);
    }
    let end = start
        .saturating_add(n(args, "limit", 20).clamp(1, 100))
        .min(rows.len());
    metadata["hash"] = json!(fingerprint);
    metadata["total_results"] = json!(rows.len());
    metadata["page_start"] = json!(start);
    metadata[field] = json!(&rows[start..end]);
    metadata["next_cursor"] = json!((end < rows.len()).then(|| format!("{fingerprint}:{end}")));
    Ok(metadata)
}

pub(super) fn search(s: &mut Session, args: &Value, cancel: &CancellationToken) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(s.config.tool_timeout_secs);
    let workspace = Workspace::load(s, args, None, cancel, deadline)?;
    let fingerprint = request_fingerprint(&workspace, "symbol_search", args);
    let mut rows = Vec::new();
    for (index, file) in workspace.files.iter().enumerate() {
        check_budget(cancel, deadline)?;
        rows.extend(
            file.symbols
                .iter()
                .map(|symbol| workspace.symbol(index, symbol)),
        );
    }
    let mut metadata = workspace.metadata();
    metadata["heuristic"] = json!(false);
    metadata["limitations"] = json!(
        "Syntax declarations only; no type inference or reference resolution. Unsupported files and parse errors can hide declarations. Read symbol bodies for behavior."
    );
    if rows.is_empty() {
        metadata["empty_reason"] = json!(if workspace.matched_files == 0 {
            "no_matching_files"
        } else if workspace.files.is_empty() {
            "no_supported_files"
        } else {
            "no_matching_symbols"
        });
    }
    let mut result = page(metadata, args, &fingerprint, "symbols", rows)?;
    // Preserve declaration excerpts/source IDs for existing consumers. Like an
    // outline, this is navigation; it does not attest a complete source line.
    for symbol in result["symbols"].as_array_mut().unwrap() {
        let file = workspace
            .files
            .iter()
            .find(|f| workspace.relative(&f.syntax.path) == symbol["path"].as_str().unwrap())
            .unwrap();
        let line = symbol["name_line"].as_u64().unwrap() as usize;
        let source_line = file.syntax.source.lines().nth(line - 1).unwrap_or("");
        let declaration: String = source_line.chars().take(500).collect();
        symbol["line"] = json!(line);
        symbol["declaration"] = json!(declaration);
        symbol["source"] = json!(observe_hashed_quality(
            s,
            &file.syntax.path,
            file.syntax.digest.clone(),
            line,
            line,
            &declaration,
            EvidenceQuality {
                line_start_complete: true,
                line_end_complete: declaration.len() == source_line.len(),
                evidence_truncated: true
            }
        ));
    }
    Ok(result)
}

pub(super) fn relations(s: &Session, args: &Value, cancel: &CancellationToken) -> Result<Value> {
    relations::execute(s, args, cancel)
}

pub(super) fn continuation(name: &str, args: &Value, cursor: &str) -> Value {
    let mut next = args.clone();
    next["tool"] = json!(name);
    next["cursor"] = json!(cursor);
    next
}

/// Paginate intact entries when the model's result budget is smaller than the
/// requested page. Never silently discard rows or manufacture read evidence.
pub(super) fn limit_page(
    call: &crate::llm::ToolCall,
    result: &Value,
    limit: usize,
    model: &str,
) -> Option<Value> {
    let args: Value = serde_json::from_str(&call.arguments).ok()?;
    let data = &result["data"];
    let field = if call.name == "symbol_search" {
        "symbols"
    } else {
        "relations"
    };
    let rows = data[field].as_array()?;
    let start = data["page_start"].as_u64()? as usize;
    let total = data["total_results"].as_u64()? as usize;
    let fingerprint = data["hash"].as_str()?;
    let candidate = |count| {
        let mut result = result.clone();
        result["data"][field] = json!(&rows[..count]);
        let cursor = (start + count < total).then(|| format!("{fingerprint}:{}", start + count));
        result["data"]["next_cursor"] = json!(cursor);
        result["next_cursor"] = cursor.map_or(Value::Null, |cursor| {
            continuation(&call.name, &args, &cursor)
        });
        result["truncated"] = json!(true);
        result
    };
    let (mut low, mut high) = (0, rows.len());
    while low < high {
        let mid = (low + high).div_ceil(2);
        if result_tokens(call, &candidate(mid), model) <= limit {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    (low > 0).then(|| candidate(low))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_deadline_stops_directory_discovery_even_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project {
            root: dir.path().into(),
            ..Default::default()
        };
        let error = candidate_paths_scoped(
            &project,
            None,
            &CancellationToken::new(),
            None,
            Some(Instant::now()),
        )
        .unwrap_err();
        assert!(
            error.to_string().starts_with("cancelled_or_timeout"),
            "{error}"
        );
    }
}
