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
use super::*;
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
/// A scan is reused by a review's pages and by later reviews of the run.
const SCAN_REUSE: Duration = Duration::from_secs(300);
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "ico", "webp", "bmp", "pdf", "ppt", "pptx", "doc", "docx", "xls",
    "xlsx", "zip", "gz", "tgz", "bz2", "7z", "jar", "woff", "woff2", "ttf", "otf", "eot", "mp3",
    "mp4", "mov", "wav", "exe", "dll", "so", "dylib", "class", "o", "a",
];

pub(super) const SCOPE_CRITERION_ID: &str = "S1";
pub(super) const SCOPE_CRITERION: &str = "The saved document covers the parts of this project that the request asks about. Judge this with runtime_project_map. First find the unopened files and directories whose names share the request's subject, including English names for the subject of a request written in another language; then decide for each whether the request asks about it. If such an area is clearly part of the request, this is unmet, and next_action names up to three of those files or directories to read and the document section they extend. When met, the reason names the closest unopened areas and why the request does not need them. Never ask for tests, unrelated areas or completeness beyond the request.";

const NOTE: &str = "Recorded by the runtime, not the model. directories maps each expanded directory (paths relative to project_root, binary files left out) to its files, marked with the lines this session delivered to the writer and whether the document cites them, and to its subdirectories: name/ is expanded under its own key, name/(N files, M opened) is summarized by count, +K more counts names not shown. An unmarked file was never opened, so its content is unknown: judge files and directories only by their paths and never claim what they contain.";
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
    lines: Option<usize>,
    cited: bool,
}

impl Mark {
    fn label(&self, name: &str) -> String {
        match (self.lines, self.cited) {
            (Some(lines), true) => format!("{name}[read {lines} lines, cited]"),
            (Some(lines), false) => format!("{name}[read {lines} lines]"),
            (None, true) => format!("{name}[cited, not read]"),
            (None, false) => name.to_owned(),
        }
    }
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
        .map(|(path, lines)| {
            (
                path,
                Mark {
                    lines: Some(lines.len()),
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
    Some(render(scan, &marks, MAX_MAP_ENTRIES))
}

fn render(scan: &Scan, marks: &BTreeMap<String, Mark>, budget: usize) -> Value {
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
        let mut names: Vec<String> = directory
            .names
            .iter()
            .filter(|name| !marked.contains(name))
            .take(MAX_LISTED_PER_DIRECTORY.saturating_sub(marked.len()))
            .cloned()
            .collect();
        names.append(&mut marked);
        names.sort();
        let mut entries: Vec<String> = names
            .iter()
            .map(|name| {
                marks
                    .get(&join(path, name))
                    .map_or_else(|| name.clone(), |mark| mark.label(name))
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
                lines: Some(12),
                cited: true,
            },
        )]);
        let map = render(&scan, &marks, MAX_MAP_ENTRIES);
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
        let map = render(&scan, &BTreeMap::new(), MAX_MAP_ENTRIES);
        let root = map["directories"]["./"].as_str().unwrap();
        for area in ["alpha", "beta", "gamma"] {
            assert!(root.contains(&format!("{area}/")), "{root}");
        }
        assert!(root.contains("(not scanned)"), "{root}");
        assert_eq!(map["truncated"], true);
    }
}
