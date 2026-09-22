use super::{MAX_FILE_BYTES, excluded, hash, output_path, read_text};
use crate::{config::Project, session::Session};
use anyhow::{Result, bail};
use caseless::Caseless;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::Permissions,
    io::Write,
    path::{Component, Path, PathBuf},
};
use tokio_util::sync::CancellationToken;
use unicode_normalization::UnicodeNormalization;

const MAX_PATCH_STAGED_BYTES: usize = 128 * 1024 * 1024;

fn normalized_alias_key(path: &Path) -> String {
    folded_alias_key(&path.to_string_lossy())
}

fn folded_alias_key(value: &str) -> String {
    value.nfd().default_case_fold().nfd().collect()
}

fn excluded_case_alias(project: &Project, relative: &Path) -> Result<bool> {
    let default_excludes = [
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
    if relative.components().any(|part| {
        default_excludes.iter().any(|name| {
            part.as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(name)
        })
    }) {
        return Ok(true);
    }
    for pattern in &project.exclude {
        let normalized_pattern: String = pattern.nfd().collect();
        let normalized_path: String = relative.to_string_lossy().nfd().collect();
        if globset::GlobBuilder::new(&normalized_pattern)
            .case_insensitive(true)
            .build()?
            .compile_matcher()
            .is_match(&normalized_path)
        {
            return Ok(true);
        }
        let folded_pattern = folded_alias_key(pattern);
        let folded_path = folded_alias_key(&relative.to_string_lossy());
        if let Ok(folded_glob) = globset::Glob::new(&folded_pattern) {
            if folded_glob.compile_matcher().is_match(&folded_path) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn normalize_existing_prefix(path: &Path) -> Result<PathBuf> {
    let mut ancestor = path;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| anyhow::anyhow!("invalid_file_path"))?;
    }
    let canonical = ancestor.canonicalize()?;
    if ancestor == path {
        Ok(canonical)
    } else {
        Ok(canonical.join(path.strip_prefix(ancestor)?))
    }
}

fn project_path(s: &Session, raw: &str) -> Result<PathBuf> {
    let relative = Path::new(raw);
    if raw.is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!("invalid_file_path: use a project-relative path without . or .. components");
    }
    let root = s.project.root.canonicalize()?;
    if excluded(&s.project, relative)? || excluded_case_alias(&s.project, relative)? {
        bail!("path_excluded: {raw}");
    }
    let candidate = root.join(relative);
    let mut current = root.clone();
    for part in relative.components() {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                bail!("file_symlink_not_allowed: {}", current.display())
            }
            Ok(meta) if current != candidate && !meta.is_dir() => {
                bail!("file_parent_not_directory: {}", current.display())
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    let path = normalize_existing_prefix(&candidate)?;
    if !path.starts_with(&root) {
        bail!("path_outside_project");
    }
    if excluded(&s.project, path.strip_prefix(&root)?)?
        || excluded_case_alias(&s.project, path.strip_prefix(&root)?)?
    {
        bail!("path_excluded: {raw}");
    }
    let normalized_output = normalize_existing_prefix(&output_path(&s.project)?)?;
    if path == normalized_output
        || normalized_alias_key(&path) == normalized_alias_key(&normalized_output)
    {
        bail!("configured_output_requires_document_edit");
    }
    Ok(path)
}

fn content(value: &str) -> Result<()> {
    if value.len() > MAX_FILE_BYTES {
        bail!("unsupported_large_file: maximum 16MiB");
    }
    if value.as_bytes().contains(&0) {
        bail!("unsupported_binary_file");
    }
    Ok(())
}

fn required<'a>(operation: &'a Value, key: &str) -> Result<&'a str> {
    operation
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing_or_invalid_argument: {key}"))
}

fn exact_replace(old: &str, needle: &str, replacement: &str, all: bool) -> Result<String> {
    if needle.is_empty() {
        bail!("empty_old_text");
    }
    let mut matches = 0;
    let mut cursor = 0;
    while let Some(offset) = old[cursor..].find(needle) {
        matches += 1;
        let start = cursor + offset;
        cursor = start + old[start..].chars().next().unwrap().len_utf8();
    }
    if matches == 0 {
        bail!("text_not_found");
    }
    if matches > 1 && !all {
        bail!("ambiguous_text: {matches} matches; provide a longer old_text or replace_all=true");
    }
    let result = if all {
        old.replace(needle, replacement)
    } else {
        old.replacen(needle, replacement, 1)
    };
    content(&result)?;
    Ok(result)
}

fn load(
    path: &Path,
    state: &mut BTreeMap<PathBuf, Option<String>>,
    originals: &mut BTreeMap<PathBuf, Option<String>>,
    permissions: &mut BTreeMap<PathBuf, Option<Permissions>>,
) -> Result<Option<String>> {
    if let Some(value) = state.get(path) {
        return Ok(value.clone());
    }
    let original = if path.exists() {
        Some(read_text(path)?)
    } else {
        None
    };
    permissions.insert(
        path.to_path_buf(),
        if original.is_some() {
            Some(std::fs::metadata(path)?.permissions())
        } else {
            None
        },
    );
    state.insert(path.to_path_buf(), original.clone());
    originals.insert(path.to_path_buf(), original.clone());
    Ok(original)
}

fn check_hash(expected: &str, actual: &str) -> Result<()> {
    if expected != hash(actual.as_bytes()) {
        bail!("file_revision_conflict: read the current file and retry");
    }
    Ok(())
}

fn check_operation_hash(
    op: &Value,
    path: &Path,
    actual: &str,
    checked: &BTreeSet<PathBuf>,
) -> Result<()> {
    match op.get("expected_hash") {
        Some(Value::String(expected)) => check_hash(expected, actual),
        Some(_) => bail!("invalid_argument_type: expected_hash"),
        None if checked.contains(path) => Ok(()),
        None => bail!(
            "file_hash_required: first operation on an existing file requires expected_hash from file_read"
        ),
    }
}

fn operation(
    s: &Session,
    op: &Value,
    state: &mut BTreeMap<PathBuf, Option<String>>,
    originals: &mut BTreeMap<PathBuf, Option<String>>,
    permissions: &mut BTreeMap<PathBuf, Option<Permissions>>,
    checked: &mut BTreeSet<PathBuf>,
    replaced_paths: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let obj = op
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("invalid_operation: expected object"))?;
    let action = required(op, "action")?;
    let path = project_path(s, required(op, "path")?)?;
    let allowed: &[&str] = match action {
        "add" => &["action", "path", "content"],
        "update" => &[
            "action",
            "path",
            "expected_hash",
            "old_text",
            "new_text",
            "replace_all",
        ],
        "replace" => &["action", "path", "expected_hash", "content"],
        "move" => &["action", "path", "to_path", "expected_hash"],
        "delete" => &["action", "path", "expected_hash"],
        _ => bail!("invalid_operation_action: {action}"),
    };
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            bail!("invalid_operation_field: {key} for {action}");
        }
    }
    match action {
        "add" => {
            let text = required(op, "content")?;
            content(text)?;
            if load(&path, state, originals, permissions)?.is_some() {
                bail!("file_exists: {}", path.display());
            }
            if originals[&path].is_some() {
                replaced_paths.insert(path.clone());
            }
            permissions.insert(path.clone(), None);
            state.insert(path.clone(), Some(text.to_owned()));
        }
        "update" => {
            let old = load(&path, state, originals, permissions)?
                .ok_or_else(|| anyhow::anyhow!("file_not_found: {}", path.display()))?;
            check_operation_hash(op, &path, &old, checked)?;
            let all = match op.get("replace_all") {
                None => false,
                Some(Value::Bool(value)) => *value,
                _ => bail!("invalid_argument_type: replace_all"),
            };
            let next = exact_replace(
                &old,
                required(op, "old_text")?,
                required(op, "new_text")?,
                all,
            )?;
            state.insert(path.clone(), Some(next));
        }
        "replace" => {
            let old = load(&path, state, originals, permissions)?
                .ok_or_else(|| anyhow::anyhow!("file_not_found: {}", path.display()))?;
            check_operation_hash(op, &path, &old, checked)?;
            let next = required(op, "content")?;
            content(next)?;
            state.insert(path.clone(), Some(next.to_owned()));
        }
        "move" => {
            let old = load(&path, state, originals, permissions)?
                .ok_or_else(|| anyhow::anyhow!("file_not_found: {}", path.display()))?;
            check_operation_hash(op, &path, &old, checked)?;
            let destination = project_path(s, required(op, "to_path")?)?;
            if destination == path || load(&destination, state, originals, permissions)?.is_some() {
                bail!("file_exists: {}", destination.display());
            }
            if originals[&destination].is_some() {
                replaced_paths.insert(destination.clone());
            }
            let moved_permissions = permissions.get(&path).cloned().unwrap_or(None);
            permissions.insert(path.clone(), None);
            permissions.insert(destination.clone(), moved_permissions);
            state.insert(path.clone(), None);
            state.insert(destination.clone(), Some(old));
            checked.insert(destination);
        }
        "delete" => {
            let old = load(&path, state, originals, permissions)?
                .ok_or_else(|| anyhow::anyhow!("file_not_found: {}", path.display()))?;
            check_operation_hash(op, &path, &old, checked)?;
            permissions.insert(path.clone(), None);
            state.insert(path.clone(), None);
        }
        _ => unreachable!(),
    }
    checked.insert(path);
    Ok(())
}

fn write_path(
    path: &Path,
    text: &str,
    existed: bool,
    permissions: Option<&Permissions>,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid_file_path"))?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(text.as_bytes())?;
    if let Some(permissions) = permissions {
        temp.as_file().set_permissions(permissions.clone())?;
    }
    temp.as_file().sync_all()?;
    if existed {
        temp.persist(path)?;
    } else {
        temp.persist_noclobber(path)?;
    }
    Ok(())
}

fn commit(
    s: &Session,
    state: BTreeMap<PathBuf, Option<String>>,
    originals: BTreeMap<PathBuf, Option<String>>,
    permissions: BTreeMap<PathBuf, Option<Permissions>>,
    replaced_paths: BTreeSet<PathBuf>,
    cancel: &CancellationToken,
) -> Result<Value> {
    let root = s.project.root.canonicalize()?;
    let files: Vec<Value> = state
        .iter()
        .map(|(path, after)| {
            json!({
                "path":path.strip_prefix(&root).unwrap().to_string_lossy(),
                "exists":after.is_some(),
                "hash":after.as_ref().map(|text| hash(text.as_bytes())),
                "changed":originals[path] != *after || replaced_paths.contains(path)
            })
        })
        .collect();
    let original_permissions: BTreeMap<_, _> = originals
        .iter()
        .map(|(path, text)| {
            Ok((
                path.clone(),
                if text.is_some() {
                    Some(std::fs::metadata(path)?.permissions())
                } else {
                    None
                },
            ))
        })
        .collect::<Result<_>>()?;
    let changed: Vec<_> = state
        .into_iter()
        .filter(|(path, after)| originals[path] != *after || replaced_paths.contains(path))
        .collect();
    for (path, _) in &changed {
        let checked = project_path(
            s,
            path.strip_prefix(s.project.root.canonicalize()?)?
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid_file_path"))?,
        )?;
        if checked != *path {
            bail!("file_path_changed");
        }
        let current = if path.exists() {
            Some(read_text(path)?)
        } else {
            None
        };
        if current != originals[path] {
            bail!(
                "file_revision_conflict: {} changed during patch",
                path.display()
            );
        }
    }
    let mut completed: Vec<PathBuf> = Vec::new();
    for (path, after) in &changed {
        if cancel.is_cancelled() && completed.is_empty() {
            bail!("cancelled");
        }
        if cancel.is_cancelled() {
            let failures = rollback(&completed, &originals, &original_permissions);
            if failures.is_empty() {
                bail!("cancelled: completed file changes were rolled back");
            }
            bail!(
                "file_patch_rollback_failed: cancellation left uncertain file changes; rollback_errors={failures:?}"
            );
        }
        let result = match after {
            Some(text) => write_path(
                path,
                text,
                originals[path].is_some(),
                permissions[path].as_ref(),
            ),
            None => std::fs::remove_file(path).map_err(Into::into),
        };
        if let Err(error) = result {
            let rollback_errors = rollback(&completed, &originals, &original_permissions);
            if rollback_errors.is_empty() {
                bail!("file_patch_write_failed: {error}; completed file changes were rolled back");
            }
            bail!(
                "file_patch_rollback_failed: write error={error}; rollback_errors={rollback_errors:?}"
            );
        }
        completed.push(path.clone());
    }
    let mut result = json!({"files":files,"changed_files":changed.len()});
    if files.len() == 1 {
        result["hash"] = files[0]["hash"].clone();
    }
    Ok(result)
}

fn rollback(
    completed: &[PathBuf],
    originals: &BTreeMap<PathBuf, Option<String>>,
    original_permissions: &BTreeMap<PathBuf, Option<Permissions>>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for restored in completed.iter().rev() {
        let result = match &originals[restored] {
            Some(text) => write_path(
                restored,
                text,
                restored.exists(),
                original_permissions[restored].as_ref(),
            ),
            None => {
                if restored.exists() {
                    std::fs::remove_file(restored).map_err(Into::into)
                } else {
                    Ok(())
                }
            }
        };
        if let Err(cause) = result {
            errors.push(format!("{}: {cause}", restored.display()));
        }
    }
    errors
}

pub(super) fn execute(
    s: &mut Session,
    name: &str,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    let operations = match name {
        "file_edit" => vec![
            json!({"action":"update","path":required(args,"path")?,"old_text":required(args,"old_text")?,"new_text":required(args,"new_text")?,"expected_hash":required(args,"expected_hash")?,"replace_all":args.get("replace_all").cloned().unwrap_or(json!(false))}),
        ],
        "file_write" => {
            let path = project_path(s, required(args, "path")?)?;
            let exists = path.exists();
            let content = required(args, "content")?;
            if exists {
                let expected = required(args, "expected_hash")?;
                let old = read_text(&path)?;
                check_hash(expected, &old)?;
                vec![
                    json!({"action":"replace","path":required(args,"path")?,"content":content,"expected_hash":expected}),
                ]
            } else {
                if args.get("expected_hash").is_some() {
                    bail!("file_not_found: expected_hash supplied for new file");
                }
                vec![json!({"action":"add","path":required(args,"path")?,"content":content})]
            }
        }
        "file_patch" => {
            let operations = args["operations"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("invalid_argument_type: operations"))?;
            if operations.is_empty() || operations.len() > 32 {
                bail!("invalid_operation_count: use 1..32 operations");
            }
            operations.clone()
        }
        _ => bail!("unsupported_tool"),
    };
    let mut state = BTreeMap::new();
    let mut originals = BTreeMap::new();
    let mut permissions = BTreeMap::new();
    let mut checked = BTreeSet::new();
    let mut replaced_paths = BTreeSet::new();
    for (index, op) in operations.iter().enumerate() {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        operation(
            s,
            op,
            &mut state,
            &mut originals,
            &mut permissions,
            &mut checked,
            &mut replaced_paths,
        )
        .map_err(|error| {
            anyhow::anyhow!("{error}; operation_index={index}; no changes persisted")
        })?;
        let staged_bytes: usize = state
            .values()
            .chain(originals.values())
            .filter_map(Option::as_ref)
            .map(String::len)
            .sum();
        if staged_bytes > MAX_PATCH_STAGED_BYTES {
            bail!(
                "file_patch_capacity: staged text exceeds 128MiB; split the patch into smaller calls; no changes persisted"
            );
        }
    }
    let mut result = commit(s, state, originals, permissions, replaced_paths, cancel)?;
    result["operation_count"] = json!(operations.len());
    Ok(result)
}
