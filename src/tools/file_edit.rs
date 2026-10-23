use super::{
    DirectoryPath, MAX_FILE_BYTES, excluded, hash, output_name_at_root, output_path, read_text,
};
use crate::{config::Project, session::Session};
use anyhow::{Result, bail};
use caseless::Caseless;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::Permissions,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
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

/// A project include/exclude pattern, compiled: case-insensitive, or (with
/// `folded`) as written over case-folded text, where an invalid one is None.
/// Compiling a glob costs far more than matching it, and every path a tool
/// resolves is checked against every pattern: one memory revalidation over
/// 2000 source files compiled 24000 globs and took 0.9 s. The patterns come
/// from the project settings, so few are seen; a full cache starts over.
fn project_glob(pattern: String, folded: bool) -> Result<Option<Arc<globset::GlobSet>>> {
    type Compiled = std::result::Result<Option<Arc<globset::GlobSet>>, String>;
    static CACHE: OnceLock<Mutex<HashMap<(String, bool), Compiled>>> = OnceLock::new();
    const ENTRIES: usize = 256;
    let cache = CACHE.get_or_init(Default::default);
    let key = (pattern, folded);
    if let Some(compiled) = cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
    {
        return compiled.clone().map_err(anyhow::Error::msg);
    }
    let compiled: Compiled = if folded {
        Ok(globset::Glob::new(&key.0)
            .ok()
            .and_then(|glob| super::glob_matcher(&glob).ok())
            .map(Arc::new))
    } else {
        globset::GlobBuilder::new(&key.0)
            .case_insensitive(true)
            .build()
            .and_then(|glob| super::glob_matcher(&glob))
            .map(|glob| Some(Arc::new(glob)))
            .map_err(|error| error.to_string())
    };
    let mut cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
    if cache.len() >= ENTRIES {
        cache.clear();
    }
    cache.insert(key, compiled.clone());
    compiled.map_err(anyhow::Error::msg)
}

pub(super) fn excluded_case_alias(project: &Project, relative: &Path) -> Result<bool> {
    ProjectRules::new(project).excluded(relative)
}

/// A project pattern compiled both ways project_glob compiles it; an error
/// is kept and reported only when the check reaches the pattern.
type PatternGlobs = (
    std::result::Result<Option<Arc<globset::GlobSet>>, String>,
    Option<Arc<globset::GlobSet>>,
);

/// A project's include/exclude rules, normalized and looked up once. A walk
/// checks every file it finds against every pattern: normalizing and case
/// folding each pattern again per file, and locking the glob cache twice per
/// pattern, made a 20,000-file listing with sixteen patterns take three
/// times as long as one without patterns.
pub(super) struct ProjectRules<'a> {
    project: &'a Project,
    include: Vec<PatternGlobs>,
    exclude: Vec<PatternGlobs>,
}

impl<'a> ProjectRules<'a> {
    pub(super) fn new(project: &'a Project) -> Self {
        let compile = |patterns: &[String]| -> Vec<PatternGlobs> {
            patterns
                .iter()
                .map(|pattern| {
                    (
                        project_glob(pattern.nfd().collect(), false).map_err(|e| e.to_string()),
                        project_glob(folded_alias_key(pattern), true).ok().flatten(),
                    )
                })
                .collect()
        };
        Self {
            project,
            include: compile(&project.include),
            exclude: compile(&project.exclude),
        }
    }

    pub(super) fn excluded(&self, relative: &Path) -> Result<bool> {
        // Version-control metadata is excluded as a file too: a `.git` file
        // points a worktree or submodule at its repository.
        let version_control = [".git", ".hg", ".svn"];
        // Dependency and build output directories. A regular file with such a
        // name (a Bazel `BUILD`, a root `build` script) is not inside one.
        let directories = [
            "target",
            "node_modules",
            "vendor",
            "dist",
            "build",
            ".venv",
            "__pycache__",
        ];
        let named = |part: Component, names: &[&str]| {
            let part = part.as_os_str().to_string_lossy();
            names.iter().any(|name| part.eq_ignore_ascii_case(name))
        };
        let last = relative.components().count().saturating_sub(1);
        if relative.components().enumerate().any(|(index, part)| {
            named(part, &version_control)
                || (named(part, &directories)
                    && (index < last || self.project.root.join(relative).is_dir()))
        }) {
            return Ok(true);
        }
        if self.include.is_empty() && self.exclude.is_empty() {
            return Ok(false);
        }
        let normalized_path: String = relative.to_string_lossy().nfd().collect();
        let folded_path = folded_alias_key(&relative.to_string_lossy());
        let matches = |(normalized, folded): &PatternGlobs| -> Result<bool> {
            Ok(normalized
                .clone()
                .map_err(anyhow::Error::msg)?
                .is_some_and(|glob| glob.is_match(&normalized_path))
                || folded
                    .as_ref()
                    .is_some_and(|glob| glob.is_match(&folded_path)))
        };
        if !self.include.is_empty() {
            let mut included = false;
            for pattern in &self.include {
                if matches(pattern)? {
                    included = true;
                    break;
                }
            }
            if !included {
                return Ok(true);
            }
        }
        for pattern in &self.exclude {
            if matches(pattern)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
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
    if excluded(&s.project, relative)? {
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
                bail!(
                    "file_parent_not_directory: {} is a file, so {raw} cannot be created beneath it; choose a path whose parent is a directory",
                    current.display()
                )
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    let path = normalize_existing_prefix(&candidate)?;
    if !path.starts_with(&root) {
        bail!(
            "path_outside_project: {raw} resolves to {}, outside project root {}",
            path.display(),
            root.display()
        );
    }
    if excluded(&s.project, path.strip_prefix(&root)?)? {
        bail!("path_excluded: {raw}");
    }
    let normalized_output = normalize_existing_prefix(&output_path(&s.project)?)?;
    // file_read resolves the output's bare name at the root to the output, so
    // writing that name must not create a stray copy beside it.
    if path == normalized_output
        || normalized_alias_key(&path) == normalized_alias_key(&normalized_output)
        || output_name_at_root(&s.project, &path).is_some()
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
        bail!(
            "unsupported_binary_file: the text to write contains NUL characters; send plain text without \\u0000"
        );
    }
    Ok(())
}

fn required<'a>(operation: &'a Value, key: &str) -> Result<&'a str> {
    operation
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| match operation.get(key) {
            None => match operation["action"].as_str() {
                Some(action) if key != "action" => {
                    anyhow::anyhow!("missing_argument: {key} is required for action={action}")
                }
                _ => anyhow::anyhow!("missing_argument: {key} is required"),
            },
            Some(value) => anyhow::anyhow!(
                "invalid_argument_type: {key} must be string, got {}",
                super::arguments::value_type(value)
            ),
        })
}

fn exact_replace(old: &str, needle: &str, replacement: &str, all: bool) -> Result<String> {
    if needle.is_empty() {
        bail!(
            "empty_old_text: old_text must not be empty; copy the exact text to replace from file_read"
        );
    }
    // A file with CRLF line breaks holds a passage that models send with LF
    // breaks; spelled with CRLF it is found, and the new text's breaks follow
    // the file's.
    let crlf: (String, String);
    let (needle, replacement) = match (!old.contains(needle)
        && needle.contains('\n')
        && !needle.contains('\r')
        && old.contains("\r\n"))
    .then(|| needle.replace('\n', "\r\n"))
    .filter(|spelled| old.contains(spelled.as_str()))
    {
        Some(spelled) => {
            crlf = (spelled, super::crlf_line_breaks(replacement));
            (crlf.0.as_str(), crlf.1.as_str())
        }
        None => (needle, replacement),
    };
    let Some(first) = old.find(needle) else {
        bail!(
            "text_not_found: old_text does not occur in the current file; copy it exactly (including indentation and line breaks) from a fresh file_read"
        );
    };
    // A second match may overlap the first (`aa` in `aaa`), so it is looked
    // for one character after the first. Counting overlapping matches by
    // searching again after each one took 28 s for a 2000-character
    // old_text in a 4 MiB file of one repeated character.
    let next_start = first + old[first..].chars().next().unwrap().len_utf8();
    if !all && old[next_start..].contains(needle) {
        bail!(
            "ambiguous_text: {} matches; provide a longer old_text or replace_all=true",
            old.matches(needle).count().max(2)
        );
    }
    // The size of the result is known before building it: replacing every
    // `a` of a 4 MiB file with 200 characters allocated 800 MB only to be
    // refused as too large.
    let count = if all { old.matches(needle).count() } else { 1 };
    let size =
        (old.len() - count * needle.len()).saturating_add(count.saturating_mul(replacement.len()));
    if size > MAX_FILE_BYTES {
        bail!("unsupported_large_file: maximum 16MiB");
    }
    let mut result = if all {
        old.replace(needle, replacement)
    } else {
        old.replacen(needle, replacement, 1)
    };
    // A file whose lines all end in CRLF keeps doing so (see
    // apply_document_edit_at): the new text's LF breaks follow the file's.
    // An edit that names CRs itself changes exactly the bytes it names.
    if !needle.contains('\r')
        && !replacement.contains('\r')
        && super::crlf_lines(old)
        && !super::crlf_lines(&result)
    {
        result = super::crlf_line_breaks(&result);
        if result.len() > MAX_FILE_BYTES {
            bail!("unsupported_large_file: maximum 16MiB");
        }
    }
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
        bail!(
            "file_revision_conflict: expected_hash {:?} does not match the current file; read the file again with file_read and copy its hash field exactly",
            expected.chars().take(80).collect::<String>()
        );
    }
    Ok(())
}

/// What the operations of a patch checked so far.
#[derive(Default)]
struct HashChecks {
    /// Files an earlier operation of the patch checked.
    checked: BTreeSet<PathBuf>,
    /// Hashes of files as the patch found them, each computed once: models
    /// repeat the file_read hash on every operation, and hashing the
    /// original and the edited text again per operation made 32 operations
    /// on a 15 MB file take 2 s.
    original_hashes: BTreeMap<PathBuf, String>,
}

impl HashChecks {
    fn original_hash(
        &mut self,
        path: &Path,
        originals: &BTreeMap<PathBuf, Option<String>>,
    ) -> Option<&str> {
        let text = originals.get(path)?.as_ref()?;
        Some(
            self.original_hashes
                .entry(path.to_path_buf())
                .or_insert_with(|| hash(text.as_bytes())),
        )
    }
}

fn check_operation_hash(
    op: &Value,
    path: &Path,
    actual: &str,
    originals: &BTreeMap<PathBuf, Option<String>>,
    checks: &mut HashChecks,
) -> Result<()> {
    match op.get("expected_hash") {
        // An earlier operation in this patch checked the file. Models repeat
        // the file_read hash on each operation (document_edit_batch accepts
        // that too); re-reading returns that same hash, so refusing it loops.
        Some(Value::String(expected)) if checks.checked.contains(path) => {
            if checks.original_hash(path, originals) == Some(expected.as_str())
                || *expected == hash(actual.as_bytes())
            {
                return Ok(());
            }
            bail!(
                "file_revision_conflict: expected_hash {:?} matches neither {} as file_read returns it nor its version after the earlier operations in this patch; omit expected_hash on later operations of the same file",
                expected.chars().take(80).collect::<String>(),
                path.display()
            )
        }
        // The first operation on a file sees it as the patch found it.
        Some(Value::String(expected))
            if originals.get(path).and_then(Option::as_deref) == Some(actual) =>
        {
            if checks.original_hash(path, originals) == Some(expected.as_str()) {
                return Ok(());
            }
            check_hash(expected, actual)
        }
        Some(Value::String(expected)) => check_hash(expected, actual),
        Some(_) => bail!("invalid_argument_type: expected_hash"),
        None if checks.checked.contains(path) => Ok(()),
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
    checks: &mut HashChecks,
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
            let owner = match key.as_str() {
                "old_text" | "new_text" | "replace_all" => {
                    "; old_text/new_text belong to action=update, which edits part of an existing file"
                }
                "content" => {
                    "; content belongs to action=add (new file) or replace (whole existing file)"
                }
                "to_path" => "; to_path belongs to action=move",
                "expected_hash" => {
                    "; action=add creates a new file, so it has no hash; use update or replace to change an existing file"
                }
                _ => "",
            };
            bail!(
                "invalid_operation_field: {key} is not accepted by action={action}, which takes {}{owner}",
                allowed[1..].join(", ")
            );
        }
    }
    match action {
        "add" => {
            let text = required(op, "content")?;
            content(text)?;
            if load(&path, state, originals, permissions)?.is_some() {
                bail!(
                    "file_exists: {} already exists and action=add only creates new files; to change it use update (old_text/new_text) or replace (content), each with expected_hash from file_read",
                    path.display()
                );
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
            check_operation_hash(op, &path, &old, originals, checks)?;
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
            check_operation_hash(op, &path, &old, originals, checks)?;
            let next = required(op, "content")?;
            content(next)?;
            state.insert(path.clone(), Some(next.to_owned()));
        }
        "move" => {
            let old = load(&path, state, originals, permissions)?
                .ok_or_else(|| anyhow::anyhow!("file_not_found: {}", path.display()))?;
            check_operation_hash(op, &path, &old, originals, checks)?;
            let destination = project_path(s, required(op, "to_path")?)?;
            if destination == path || load(&destination, state, originals, permissions)?.is_some() {
                bail!(
                    "file_exists: to_path {} {}; move only to a path that does not exist yet",
                    destination.display(),
                    if destination == path {
                        "is the same as path"
                    } else {
                        "already exists"
                    }
                );
            }
            if originals[&destination].is_some() {
                replaced_paths.insert(destination.clone());
            }
            let moved_permissions = permissions.get(&path).cloned().unwrap_or(None);
            permissions.insert(path.clone(), None);
            permissions.insert(destination.clone(), moved_permissions);
            state.insert(path.clone(), None);
            state.insert(destination.clone(), Some(old));
            checks.checked.insert(destination);
        }
        "delete" => {
            let old = load(&path, state, originals, permissions)?
                .ok_or_else(|| anyhow::anyhow!("file_not_found: {}", path.display()))?;
            check_operation_hash(op, &path, &old, originals, checks)?;
            permissions.insert(path.clone(), None);
            state.insert(path.clone(), None);
        }
        _ => unreachable!(),
    }
    checks.checked.insert(path);
    Ok(())
}

fn write_path(
    path: &Path,
    text: &str,
    existed: bool,
    permissions: Option<&Permissions>,
    created_dirs: &mut Vec<PathBuf>,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid_file_path"))?;
    let missing: Vec<_> = parent
        .ancestors()
        .take_while(|ancestor| !ancestor.exists())
        .collect();
    for directory in missing.into_iter().rev() {
        match std::fs::create_dir(directory) {
            Ok(()) => created_dirs.push(directory.to_path_buf()),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists
                    && std::fs::symlink_metadata(directory)?.file_type().is_dir() => {}
            Err(error) => return Err(error.into()),
        }
    }
    let mut temp = super::new_temp_file(parent)?;
    temp.write_all(text.as_bytes())?;
    if let Some(permissions) = permissions {
        temp.as_file().set_permissions(permissions.clone())?;
    }
    temp.as_file().sync_all()?;
    // A failed persist hands the temporary file back inside its error. Kept
    // there, it outlived the rollback, whose removal of the directories this
    // write created then failed as not empty: an uncertain-write report for
    // a patch that had been undone.
    let persisted = if existed {
        temp.persist(path)
    } else {
        temp.persist_noclobber(path)
    };
    persisted.map_err(|error| error.error)?;
    Ok(())
}

fn commit(
    s: &mut Session,
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
                // The writes are already committed; a root that moved since must not
                // turn their report into a panic and an unknown write outcome.
                "path":path.strip_prefix(&root).unwrap_or(path).to_string_lossy(),
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
    let mut created_dirs = Vec::new();
    for (path, after) in &changed {
        if cancel.is_cancelled() && completed.is_empty() {
            bail!("cancelled");
        }
        if cancel.is_cancelled() {
            let failures = rollback(
                &completed,
                &originals,
                &original_permissions,
                &mut created_dirs,
            );
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
                &mut created_dirs,
            ),
            None => std::fs::remove_file(path).map_err(Into::into),
        };
        if let Err(error) = result {
            let rollback_errors = rollback(
                &completed,
                &originals,
                &original_permissions,
                &mut created_dirs,
            );
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
    created_dirs: &mut Vec<PathBuf>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for restored in completed.iter().rev() {
        let result = match &originals[restored] {
            Some(text) => write_path(
                restored,
                text,
                restored.exists(),
                original_permissions[restored].as_ref(),
                created_dirs,
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
    for directory in created_dirs.iter().rev() {
        if let Err(cause) = std::fs::remove_dir(directory)
            && cause.kind() != std::io::ErrorKind::NotFound
        {
            errors.push(format!("{}: {cause}", directory.display()));
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
            // A directory has no hash to copy from file_read; name it before
            // asking for one.
            if path.is_dir() {
                return Err(DirectoryPath(path).into());
            }
            let exists = path.exists();
            let content = required(args, "content")?;
            if exists {
                let expected = args["expected_hash"].as_str().ok_or_else(|| {
                    anyhow::anyhow!(
                        "missing_argument: expected_hash is required because {} already exists; copy hash from file_read, or omit it only when creating a new file; no changes persisted",
                        required(args, "path").unwrap_or_default()
                    )
                })?;
                let old = read_text(&path)?;
                check_hash(expected, &old)?;
                vec![
                    json!({"action":"replace","path":required(args,"path")?,"content":content,"expected_hash":expected}),
                ]
            } else {
                if args.get("expected_hash").is_some() {
                    bail!(
                        "file_not_found: expected_hash was supplied but the file does not exist; omit expected_hash to create it, or correct the path"
                    );
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
    let mut checks = HashChecks::default();
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
            &mut checks,
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

#[cfg(test)]
mod glob_tests {
    use super::*;

    // Every resolved and every listed path is checked against every project
    // pattern; each check compiled each pattern again (a 2000-file listing
    // took a second). A pattern is compiled once and its matcher shared.
    #[test]
    fn project_patterns_are_compiled_once_and_match_as_before() {
        let first = project_glob("**/*.RS".into(), false).unwrap().unwrap();
        let again = project_glob("**/*.RS".into(), false).unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert!(first.is_match("src/main.rs"));
        assert!(project_glob("[".into(), false).is_err());
        assert!(project_glob("[".into(), false).is_err());
        assert!(project_glob("[".into(), true).unwrap().is_none());
        let project = Project {
            include: vec!["src/**".into()],
            exclude: vec!["**/*.lock".into(), "[".into()],
            ..Default::default()
        };
        assert!(excluded_case_alias(&project, Path::new("docs/a.md")).unwrap());
        assert!(excluded_case_alias(&project, Path::new("SRC/Cargo.lock")).unwrap());
        assert!(excluded_case_alias(&project, Path::new("src/a.rs")).is_err());
    }
}

#[cfg(test)]
mod write_tests {
    use super::*;

    // A failed persist kept the temporary file inside its error, so the
    // rollback that ran while the error was held could not remove the
    // directories the write had created.
    #[test]
    fn a_failed_write_leaves_no_temporary_file_behind_its_error() {
        let dir = tempfile::tempdir().unwrap();
        // Renaming a file over a non-empty directory fails.
        let target = dir.path().join("taken");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("inner"), "x").unwrap();
        let mut created = Vec::new();
        let error = write_path(&target, "text", true, None, &mut created).unwrap_err();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, ["taken"], "{error}");
        assert!(created.is_empty());
    }
}
