//! The project's files with what the writer read and cited of each. The
//! reviewer otherwise sees only the evidence the writer collected, so a
//! narrow exploration hides every area it never opened: a live "documentation
//! logic" document read one module of a 200-file project, and both reviews
//! approved it.
//!
//! The map has one size for any project. A scan is bounded in files and time
//! and reused between the reviews of a run; the rendering expands directories
//! within a fixed entry budget, first along the paths to what was read, then
//! the shallowest and largest, and summarizes every other directory by count.
//!
//! Code files the writer read in part, and unopened code files beside the
//! ones it read, list their largest top-level declarations it did not read.
//! A path alone did not tell a live reviewer that App.jsx held the project
//! page and the detail panel of the UI a manual described.
use super::*;
use crate::tools::structure;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Entries (file names and directory summaries) one map may show.
const MAX_MAP_ENTRIES: usize = 400;
/// File names one expanded directory shows; the rest are counted.
const MAX_LISTED_PER_DIRECTORY: usize = 40;
/// File names a scan keeps per directory, so a listing can still choose.
const MAX_KEPT_PER_DIRECTORY: usize = 200;
/// A monorepo is scanned partially rather than walked for long.
const MAX_SCANNED_FILES: usize = 200_000;
const MAX_SCAN_TIME: Duration = Duration::from_millis(1500);
/// Read ranges one file mark lists; more are counted.
const MAX_LISTED_RANGES: usize = 3;
/// A file is mostly unread when more than half of it, and at least this
/// many lines, never reached the writer. A live UI manual read 120 of
/// App.jsx's 1921 lines; the map said "read 120 lines", so the file looked
/// covered and the manual missed the sidebar, top bar and detail panel.
const MOSTLY_UNREAD_LINES: usize = 100;
/// Code files one map outlines, and the declarations one outline lists.
const MAX_OUTLINED_FILES: usize = 12;
const MAX_LISTED_DECLARATIONS: usize = 6;
/// Shorter declarations (one-line constants) are counted, not listed.
const MIN_LISTED_LINES: usize = 3;
/// Time the outlines of one map may take; a file past it is not outlined.
const MAX_OUTLINE_TIME: Duration = Duration::from_millis(1000);
/// A scan is reused by a review's pages and by later reviews of the run.
const SCAN_REUSE: Duration = Duration::from_secs(300);
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "ico", "webp", "bmp", "pdf", "ppt", "pptx", "doc", "docx", "xls",
    "xlsx", "zip", "gz", "tgz", "bz2", "7z", "jar", "woff", "woff2", "ttf", "otf", "eot", "mp3",
    "mp4", "mov", "wav", "exe", "dll", "so", "dylib", "class", "o", "a",
];

pub(super) const SCOPE_CRITERION_ID: &str = "S1";
pub(super) const SCOPE_CRITERION: &str = "The saved document covers the parts of this project that the request asks about. Judge this with runtime_project_map. First find the unopened files and directories whose names share the request's subject, including English names for the subject of a request written in another language; the unread declarations the map lists whose names belong to that subject (for a UI manual, the screens, pages, panels and dialogs a user sees); and the files marked mostly unread that the document cites for that subject, whose unread lines are unopened areas too. Then decide for each whether the request asks about it. If such an area is clearly part of the request, this is unmet, and next_action names up to three of those files, declarations or directories, with their line ranges where the map gives them, to read and the document section they extend. When met, the reason names the closest unopened areas and why the request does not need them. Never ask for tests, unrelated areas or completeness beyond the request.";

const NOTE: &str = "Recorded by the runtime, not the model. directories maps each expanded directory (paths relative to project_root, binary files left out) to its files, marked with the lines this session delivered to the writer (read N of M lines with the read ranges; mostly unread when more than half of the file, and at least 100 lines, never reached the writer) and whether the document cites them, and to its subdirectories: name/ is expanded under its own key, name/(N files, M opened) is summarized by count, +K more counts names not shown. Code files list their largest top-level declarations this session did not read as name first-last line, after the marks of a file read in part (unread: ...) and, for an unopened file beside read ones, after its line count (N lines: ...); these names come from a syntax outline, not from reading the code. An unmarked file was never opened, so its content is unknown: judge files, directories and listed declarations only by their paths and names, and never claim what their code does.";
const TRUNCATED_NOTE: &str = "The scan stopped early: counts are lower bounds, and a top-level directory marked (not scanned) was never walked.";

/// The project's files from one bounded walk, never persisted.
#[derive(Debug)]
pub(crate) struct Scan {
    key: String,
    taken: Instant,
    directories: BTreeMap<String, Directory>,
    files: usize,
    truncated: bool,
}

#[derive(Debug, Default)]
struct Directory {
    names: Vec<String>,
    direct: usize,
    total: usize,
    children: BTreeSet<String>,
    /// Known only from the root listing of a scan that stopped early.
    unscanned: bool,
}

impl Scan {
    fn add(&mut self, parts: &[String]) {
        let Some((name, directories)) = parts.split_last() else {
            return;
        };
        let mut parent = String::new();
        for directory in directories {
            let path = join(&parent, directory);
            self.directories
                .entry(parent)
                .or_default()
                .children
                .insert(path.clone());
            self.directories.entry(path.clone()).or_default();
            parent = path;
        }
        let directory = self.directories.entry(parent).or_default();
        directory.direct += 1;
        if directory.names.len() < MAX_KEPT_PER_DIRECTORY {
            directory.names.push(name.clone());
        }
        self.files += 1;
    }

    fn count_totals(&mut self) {
        let mut paths: Vec<String> = self.directories.keys().cloned().collect();
        paths.sort_by_key(|path| std::cmp::Reverse(depth(path)));
        for path in paths {
            let below: usize = self.directories[&path]
                .children
                .iter()
                .map(|child| self.directories.get(child).map_or(0, |child| child.total))
                .sum();
            let directory = self.directories.get_mut(&path).unwrap();
            directory.total = directory.direct + below;
        }
    }
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn depth(path: &str) -> usize {
    if path.is_empty() {
        0
    } else {
        path.matches('/').count() + 1
    }
}

fn parent_of(path: &str) -> String {
    path.rsplit_once('/')
        .map_or_else(String::new, |(parent, _)| parent.to_owned())
}

fn parts(path: &Path) -> Vec<String> {
    path.iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect()
}

fn binary(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            BINARY_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

fn scan_key(p: &Project) -> String {
    format!(
        "{}|{:?}|{:?}|{}",
        p.root.display(),
        p.include,
        p.exclude,
        p.output.display()
    )
}

/// Take a scan for the next review unless a recent one of the same project
/// settings exists. Reviews call this where they begin or page; verdict
/// checks never walk the project.
pub(super) fn refresh(s: &mut Session) {
    if !s.is_document_work() {
        s.completion_review.project_scan = None;
        return;
    }
    let key = scan_key(&s.project);
    let recent = s
        .completion_review
        .project_scan
        .as_ref()
        .is_some_and(|scan| scan.key == key && scan.taken.elapsed() < SCAN_REUSE);
    if !recent {
        s.completion_review.project_scan =
            scan(&s.project, key, MAX_SCANNED_FILES, MAX_SCAN_TIME).map(Arc::new);
    }
}

fn scan(p: &Project, key: String, max_files: usize, max_time: Duration) -> Option<Scan> {
    let root = p.root.canonicalize().ok()?;
    let output = output_path(p)
        .ok()
        .and_then(|path| path.canonicalize().ok());
    let taken = Instant::now();
    let mut scan = Scan {
        key,
        taken,
        directories: BTreeMap::from([(String::new(), Directory::default())]),
        files: 0,
        truncated: false,
    };
    let walk = ignore::WalkBuilder::new(&root)
        .hidden(false)
        .follow_links(false)
        .filter_entry(|entry| {
            !entry.file_type().is_some_and(|kind| kind.is_dir())
                || !SKIPPED_DIRECTORIES.contains(&entry.file_name().to_string_lossy().as_ref())
        })
        .build();
    for (visited, entry) in walk.enumerate() {
        if scan.files >= max_files || (visited % 256 == 0 && taken.elapsed() > max_time) {
            scan.truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file())
            || entry.path().to_str().is_none()
            || output.as_deref() == Some(entry.path())
        {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(&root) else {
            continue;
        };
        if binary(relative) || excluded(p, relative).unwrap_or(true) {
            continue;
        }
        scan.add(&parts(relative));
    }
    // A scan that stopped early still names every top-level area, so an
    // area the walk never reached is not taken for absent.
    if scan.truncated
        && let Ok(entries) = std::fs::read_dir(&root)
    {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type().is_ok_and(|kind| kind.is_dir())
                || SKIPPED_DIRECTORIES.contains(&name.as_str())
                || excluded(p, Path::new(&name)).unwrap_or(true)
                || scan.directories.contains_key(&name)
            {
                continue;
            }
            scan.directories
                .get_mut("")
                .unwrap()
                .children
                .insert(name.clone());
            scan.directories.insert(
                name,
                Directory {
                    unscanned: true,
                    ..Default::default()
                },
            );
        }
    }
    scan.count_totals();
    Some(scan)
}

/// What this session read of a file and whether the document cites it.
#[derive(Default)]
struct Mark {
    /// Line numbers delivered to the writer.
    read: BTreeSet<usize>,
    /// The file's current line count, when it could be read.
    total: Option<usize>,
    cited: bool,
}

/// Top-level declarations of a code file this session did not read.
#[derive(Debug, Default)]
struct Outline {
    /// The file's line count.
    total: usize,
    /// The largest unread declarations as (name, first line, last line), in
    /// line order.
    listed: Vec<(String, usize, usize)>,
    /// Unread declarations not listed.
    more: usize,
}

impl Outline {
    fn text(&self) -> String {
        let mut parts: Vec<String> = self
            .listed
            .iter()
            .map(|(name, start, end)| format!("{name} {start}-{end}"))
            .collect();
        if self.more > 0 {
            parts.push(format!("+{} more", self.more));
        }
        parts.join(", ")
    }

    /// An unopened file's label.
    fn label(&self, name: &str) -> String {
        let lines = match self.total {
            1 => "1 line".to_owned(),
            total => format!("{total} lines"),
        };
        if self.listed.is_empty() {
            format!("{name}({lines})")
        } else {
            format!("{name}({lines}: {})", self.text())
        }
    }
}

impl Mark {
    fn label(&self, name: &str, outline: Option<&Outline>) -> String {
        let cited = if self.cited { ", cited" } else { "" };
        if self.read.is_empty() {
            return if self.cited {
                format!("{name}[cited, not read]")
            } else {
                name.to_owned()
            };
        }
        let Some(total) = self.total else {
            return format!("{name}[read {} lines{cited}]", self.read.len());
        };
        let read: Vec<usize> = self.read.range(1..=total).copied().collect();
        if read.len() >= total {
            return format!("{name}[read all {total} lines{cited}]");
        }
        let ranges = ranges(&read);
        let shown = if ranges.len() <= MAX_LISTED_RANGES {
            ranges.join(", ")
        } else {
            format!("in {} ranges", ranges.len())
        };
        let unread = total - read.len();
        let mostly = if unread * 2 > total && unread >= MOSTLY_UNREAD_LINES {
            ", mostly unread"
        } else {
            ""
        };
        let unread = outline
            .filter(|outline| !outline.listed.is_empty())
            .map_or_else(String::new, |outline| {
                format!("; unread: {}", outline.text())
            });
        format!(
            "{name}[read {} of {total} lines ({shown}){mostly}{cited}{unread}]",
            read.len()
        )
    }
}

/// The top-level declarations of a code file that `read` covers less than
/// half of, outside Rust test modules; None for a file without a syntax
/// outline or one that cannot be parsed before the deadline.
fn outline(path: &Path, read: &BTreeSet<usize>, deadline: Instant) -> Option<Outline> {
    structure::language(path).ok()?;
    let source = read_text(path).ok()?;
    let total = source.lines().count();
    let tests = if path.extension().is_some_and(|extension| extension == "rs") {
        documentation::rust_test_modules(&source)
    } else {
        Vec::new()
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let file = structure::SyntaxFile::parse(path.to_path_buf(), source, &cancel, deadline).ok()?;
    let (symbols, _) = file
        .symbols(&json!({"max_depth":0}), None, &cancel, deadline)
        .ok()?;
    let mut unread: Vec<(String, usize, usize)> = symbols
        .iter()
        .filter_map(|symbol| {
            let name: String = symbol["name"].as_str()?.chars().take(60).collect();
            let start = symbol["start_line"].as_u64()? as usize;
            let end = (symbol["end_line"].as_u64()? as usize).max(start);
            let seen = read.range(start..=end).count();
            let in_tests = tests
                .iter()
                .any(|(first, last)| (*first..=*last).contains(&start));
            (seen * 2 < end - start + 1 && !in_tests).then_some((name, start, end))
        })
        .collect();
    let count = unread.len();
    unread.retain(|(_, start, end)| end - start + 1 >= MIN_LISTED_LINES);
    unread.sort_by_key(|(_, start, end)| (std::cmp::Reverse(end - start), *start));
    unread.truncate(MAX_LISTED_DECLARATIONS);
    unread.sort_by_key(|(_, start, _)| *start);
    Some(Outline {
        total,
        more: count - unread.len(),
        listed: unread,
    })
}

/// A directory's unmarked file names the map lists, in listing order.
fn listed_unmarked<'a>(
    directory: &'a Directory,
    marked: &'a [String],
) -> impl Iterator<Item = &'a String> {
    directory
        .names
        .iter()
        .filter(move |name| !marked.contains(name))
        .take(MAX_LISTED_PER_DIRECTORY.saturating_sub(marked.len()))
}

/// The marked files' names in each directory that holds one.
fn marked_by_directory(marks: &BTreeMap<String, Mark>) -> BTreeMap<String, Vec<String>> {
    let mut directories = BTreeMap::<String, Vec<String>>::new();
    for path in marks.keys() {
        directories
            .entry(parent_of(path))
            .or_default()
            .push(path.rsplit('/').next().unwrap_or(path).to_owned());
    }
    directories
}

/// Outlines of the code files a reviewer most needs to see into: files read
/// in part with at least MOSTLY_UNREAD_LINES unread lines, most unread
/// first, then the largest unopened files listed beside them. Test files are
/// left out.
fn outlines(root: &Path, scan: &Scan, marks: &BTreeMap<String, Mark>) -> BTreeMap<String, Outline> {
    let mut partial: Vec<(usize, &String)> = marks
        .iter()
        .filter_map(|(path, mark)| {
            let total = mark.total?;
            let unread = total.saturating_sub(mark.read.range(1..=total).count());
            (unread >= MOSTLY_UNREAD_LINES).then_some((unread, path))
        })
        .collect();
    partial.sort_by_key(|(unread, path)| (std::cmp::Reverse(*unread), *path));
    let mut unopened: Vec<(u64, String)> = Vec::new();
    for (directory, marked) in marked_by_directory(marks) {
        let Some(listing) = scan.directories.get(&directory) else {
            continue;
        };
        for name in listed_unmarked(listing, &marked) {
            let path = join(&directory, name);
            let full = root.join(&path);
            if structure::language(&full).is_ok()
                && !documentation::test_file(Path::new(&path))
                && let Ok(metadata) = std::fs::metadata(&full)
            {
                unopened.push((metadata.len(), path));
            }
        }
    }
    unopened.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let deadline = Instant::now() + MAX_OUTLINE_TIME;
    let empty = BTreeSet::new();
    partial
        .into_iter()
        .map(|(_, path)| (path.clone(), &marks[path].read))
        .filter(|(path, _)| !documentation::test_file(Path::new(path)))
        .chain(unopened.into_iter().map(|(_, path)| (path, &empty)))
        .take(MAX_OUTLINED_FILES)
        .filter_map(|(path, read)| {
            let outline = outline(&root.join(&path), read, deadline)?;
            Some((path, outline))
        })
        .collect()
}

/// Sorted line numbers as contiguous "start-end" ranges.
fn ranges(lines: &[usize]) -> Vec<String> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &line in lines {
        match ranges.last_mut() {
            Some((_, end)) if *end + 1 == line => *end = line,
            _ => ranges.push((line, line)),
        }
    }
    ranges
        .into_iter()
        .map(|(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect()
}

fn marks(s: &Session) -> BTreeMap<String, Mark> {
    let Ok(root) = s.project.root.canonicalize() else {
        return BTreeMap::new();
    };
    let relative = |path: &str| {
        let resolved = read_path(&s.project, path).ok()?;
        Some(parts(resolved.strip_prefix(&root).ok()?).join("/"))
    };
    let mut lines = BTreeMap::<String, BTreeSet<usize>>::new();
    for source in s.sources.values().filter(|source| source.origin == "file") {
        if let (Some(path), Some(start), Some(end)) =
            (source.path.as_deref(), source.start_line, source.end_line)
            && let Some(path) = relative(path)
        {
            lines.entry(path).or_default().extend(start..=end);
        }
    }
    let mut marks: BTreeMap<String, Mark> = lines
        .into_iter()
        .map(|(path, read)| {
            let total = read_text(&root.join(&path))
                .ok()
                .map(|text| text.lines().count());
            (
                path,
                Mark {
                    read,
                    total,
                    cited: false,
                },
            )
        })
        .collect();
    if let Ok(output) = output_path(&s.project)
        && let Ok(doc) = read_text(&output)
        && let Ok(citations) = documentation::citation_spans(&doc)
    {
        for citation in citations {
            let path = if citation.relative_link {
                output
                    .parent()
                    .unwrap()
                    .join(&citation.path)
                    .to_string_lossy()
                    .into_owned()
            } else {
                citation.path
            };
            if let Some(path) = relative(&path) {
                marks.entry(path).or_default().cited = true;
            }
        }
    }
    marks
}

/// Map evidence for the latest scan of a source document's project, or
/// None outside document work or when the root could not be scanned.
pub(super) fn evidence(s: &Session) -> Option<Value> {
    if !s.is_document_work() {
        return None;
    }
    let scan = s.completion_review.project_scan.as_ref()?;
    let marks = marks(s);
    let outlines = s
        .project
        .root
        .canonicalize()
        .map(|root| outlines(&root, scan, &marks))
        .unwrap_or_default();
    Some(render(scan, &marks, &outlines, MAX_MAP_ENTRIES))
}

fn render(
    scan: &Scan,
    marks: &BTreeMap<String, Mark>,
    outlines: &BTreeMap<String, Outline>,
    budget: usize,
) -> Value {
    let mut opened = BTreeMap::<String, usize>::new();
    for path in marks.keys() {
        let mut directory = parent_of(path);
        loop {
            *opened.entry(directory.clone()).or_default() += 1;
            if directory.is_empty() {
                break;
            }
            directory = parent_of(&directory);
        }
    }
    // A directory's marked files, including any beyond the names a scan kept.
    let marked_in = |directory: &str| -> Vec<String> {
        marks
            .keys()
            .filter(|path| parent_of(path) == directory)
            .map(|path| path.rsplit('/').next().unwrap_or(path).to_owned())
            .collect()
    };
    let cost = |directory: &Directory| {
        directory.direct.min(MAX_LISTED_PER_DIRECTORY) + directory.children.len() + 1
    };
    // The root and the paths to what was read are expanded first, shallow
    // before deep, while any budget is left. Every other directory is
    // expanded a whole level at a time, so all areas of one depth are shown
    // alike; a level that does not fit stays summarized by count.
    let mut expanded = BTreeSet::from([String::new()]);
    let mut left = budget.saturating_sub(cost(&scan.directories[""]));
    let mut along: Vec<&String> = scan
        .directories
        .keys()
        .filter(|path| !path.is_empty() && opened.contains_key(*path))
        .collect();
    along.sort_by_key(|path| depth(path));
    for path in along {
        let directory = &scan.directories[path];
        if left == 0 || directory.unscanned || !expanded.contains(&parent_of(path)) {
            continue;
        }
        left = left.saturating_sub(cost(directory));
        expanded.insert(path.clone());
    }
    let deepest = scan
        .directories
        .keys()
        .map(|path| depth(path))
        .max()
        .unwrap_or(0);
    for level in 1..=deepest {
        let candidates: Vec<&String> = scan
            .directories
            .iter()
            .filter(|(path, directory)| {
                depth(path) == level
                    && !directory.unscanned
                    && !expanded.contains(*path)
                    && expanded.contains(&parent_of(path))
            })
            .map(|(path, _)| path)
            .collect();
        let needed: usize = candidates
            .iter()
            .map(|path| cost(&scan.directories[*path]))
            .sum();
        if needed > left {
            break;
        }
        left -= needed;
        expanded.extend(candidates.into_iter().cloned());
    }
    let mut directories = serde_json::Map::new();
    for path in &expanded {
        let directory = &scan.directories[path];
        let mut marked = marked_in(path);
        let mut names: Vec<String> = listed_unmarked(directory, &marked).cloned().collect();
        names.append(&mut marked);
        names.sort();
        let mut entries: Vec<String> = names
            .iter()
            .map(|name| {
                let file = join(path, name);
                match (marks.get(&file), outlines.get(&file)) {
                    (Some(mark), outline) => mark.label(name, outline),
                    (None, Some(outline)) => outline.label(name),
                    (None, None) => name.clone(),
                }
            })
            .collect();
        let more = directory.direct.saturating_sub(names.len());
        if more > 0 {
            entries.push(format!("+{more} more"));
        }
        for child in &directory.children {
            let name = child.rsplit('/').next().unwrap_or(child);
            let below = &scan.directories[child];
            entries.push(if expanded.contains(child) {
                format!("{name}/")
            } else if below.unscanned {
                format!("{name}/(not scanned)")
            } else {
                match opened.get(child) {
                    Some(count) => format!("{name}/({} files, {count} opened)", below.total),
                    None => format!("{name}/({} files)", below.total),
                }
            });
        }
        let key = if path.is_empty() {
            "./".to_owned()
        } else {
            format!("{path}/")
        };
        directories.insert(key, json!(entries.join(" ")));
    }
    let mut map = json!({"kind":"runtime_project_map","files":scan.files,
        "opened":marks.len(),"directories":directories,"note":NOTE});
    if scan.truncated {
        map["truncated"] = json!(true);
        map["truncated_note"] = json!(TRUNCATED_NOTE);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic(files: &[&str]) -> Scan {
        let mut scan = Scan {
            key: String::new(),
            taken: Instant::now(),
            directories: BTreeMap::from([(String::new(), Directory::default())]),
            files: 0,
            truncated: false,
        };
        for file in files {
            scan.add(&file.split('/').map(str::to_owned).collect::<Vec<_>>());
        }
        scan.count_totals();
        scan
    }

    #[test]
    fn a_large_project_map_keeps_its_size_and_the_paths_to_what_was_read() {
        // 60,000 files: the rendering must stay within its entry budget, keep
        // every top-level area visible and expand down to the file read.
        let mut files = Vec::new();
        for area in 0..30 {
            for module in 0..40 {
                for file in 0..50 {
                    files.push(format!("area{area:02}/module{module:02}/file{file:02}.rs"));
                }
            }
        }
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let scan = synthetic(&refs);
        assert_eq!(scan.files, 60_000);
        let marks = BTreeMap::from([(
            "area29/module39/file49.rs".to_owned(),
            Mark {
                read: (1..=12).collect(),
                total: None,
                cited: true,
            },
        )]);
        let map = render(&scan, &marks, &BTreeMap::new(), MAX_MAP_ENTRIES);
        let directories = map["directories"].as_object().unwrap();
        let shown: usize = directories
            .values()
            .map(|entries| entries.as_str().unwrap().split(' ').count())
            .sum();
        assert!(shown <= MAX_MAP_ENTRIES + directories.len(), "{shown}");
        let root = directories["./"].as_str().unwrap();
        assert!(root.contains("area00/(2000 files)"), "{root}");
        assert!(root.contains("area29/"), "{root}");
        assert!(
            directories["area29/"]
                .as_str()
                .unwrap()
                .contains("module39/"),
            "{map}"
        );
        let module = directories["area29/module39/"].as_str().unwrap();
        assert!(
            module.contains("file49.rs[read 12 lines, cited]"),
            "{module}"
        );
        assert!(module.contains("+10 more"), "{module}");
        assert_eq!(map["opened"], 1);
        assert!(map.get("truncated").is_none());
    }

    #[test]
    fn a_file_mark_shows_how_much_of_the_file_was_read() {
        let mark = |read: &[std::ops::RangeInclusive<usize>], total, cited| Mark {
            read: read.iter().cloned().flatten().collect(),
            total,
            cited,
        };
        // The live UI manual's main screen file: 120 of 1921 lines.
        assert_eq!(
            mark(&[1004..=1123], Some(1921), true).label("App.jsx", None),
            "App.jsx[read 120 of 1921 lines (1004-1123), mostly unread, cited]"
        );
        assert_eq!(
            mark(&[1..=349], Some(349), true).label("Chat.jsx", None),
            "Chat.jsx[read all 349 lines, cited]"
        );
        // Lines read from a longer earlier version still mean the whole file.
        assert_eq!(
            mark(&[1..=60], Some(50), false).label("short.rs", None),
            "short.rs[read all 50 lines]"
        );
        // Most of the file was read.
        assert_eq!(
            mark(&[1..=300], Some(400), true).label("mostly.rs", None),
            "mostly.rs[read 300 of 400 lines (1-300), cited]"
        );
        // Most of a small file is unread, but too few lines to matter.
        assert_eq!(
            mark(&[1..=10], Some(80), false).label("small.rs", None),
            "small.rs[read 10 of 80 lines (1-10)]"
        );
        assert_eq!(
            mark(&[1..=10, 20..=20, 30..=40, 50..=60], Some(900), false).label("many.rs", None),
            "many.rs[read 33 of 900 lines (in 4 ranges), mostly unread]"
        );
        assert_eq!(
            mark(&[1..=10, 20..=20, 30..=40], Some(900), false).label("three.rs", None),
            "three.rs[read 22 of 900 lines (1-10, 20, 30-40), mostly unread]"
        );
        assert_eq!(
            mark(&[], None, true).label("cited.rs", None),
            "cited.rs[cited, not read]"
        );
    }

    #[test]
    fn an_outline_lists_the_largest_unread_declarations_outside_test_modules() {
        let dir = tempfile::tempdir().unwrap();
        let function = |name: &str, lines: usize| {
            let body: String = (1..lines - 1)
                .map(|n| format!("    let v{n} = {n};\n"))
                .collect();
            format!("fn {name}() {{\n{body}}}\n")
        };
        let mut text: String = (1..=8).map(|n| function(&format!("f{n}"), n + 2)).collect();
        // A one-line constant is counted, not listed.
        text.push_str("const LIMIT: usize = 3;\n");
        text.push_str("#[cfg(test)]\nmod tests {\n");
        text.push_str(&function("checks", 40));
        text.push_str("}\n");
        let path = dir.path().join("lib.rs");
        std::fs::write(&path, &text).unwrap();
        let deadline = Instant::now() + MAX_OUTLINE_TIME;
        // f1..f8 span 3..10 lines from line 1; f8 (lines 43-52) is half read.
        let read: BTreeSet<usize> = (43..=47).collect();
        let listed = outline(&path, &read, deadline).unwrap();
        assert_eq!(listed.total, text.lines().count());
        // The six largest unread functions in line order; f8 is half read
        // and so not unread, and the test module is never listed.
        assert_eq!(
            listed.text(),
            "f2 4-7, f3 8-12, f4 13-18, f5 19-25, f6 26-33, f7 34-42, +2 more"
        );
        assert!(outline(&dir.path().join("notes.md"), &read, deadline).is_none());
    }

    #[test]
    fn a_scan_that_stops_early_still_names_every_top_level_area() {
        let dir = tempfile::tempdir().unwrap();
        for area in ["alpha", "beta", "gamma"] {
            std::fs::create_dir_all(dir.path().join(area)).unwrap();
            for file in 0..5 {
                std::fs::write(dir.path().join(area).join(format!("f{file}.rs")), "x\n").unwrap();
            }
        }
        let project = Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        };
        let scan = scan(&project, String::new(), 3, MAX_SCAN_TIME).unwrap();
        assert!(scan.truncated);
        assert_eq!(scan.files, 3);
        let map = render(&scan, &BTreeMap::new(), &BTreeMap::new(), MAX_MAP_ENTRIES);
        let root = map["directories"]["./"].as_str().unwrap();
        for area in ["alpha", "beta", "gamma"] {
            assert!(root.contains(&format!("{area}/")), "{root}");
        }
        assert!(root.contains("(not scanned)"), "{root}");
        assert_eq!(map["truncated"], true);
    }
}
