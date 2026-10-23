use super::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct Heading {
    pub heading: String,
    pub start: usize,
    pub end: usize,
    pub(super) line: usize,
    pub(super) level: usize,
}

fn html_comment_state(line: &str, in_comment: bool) -> bool {
    if !in_comment && !line.contains("<!--") {
        return false;
    }
    let mut state = in_comment;
    let _ = visible_without_html_comments(line, &mut state);
    state
}

fn visible_without_html_comments(line: &str, in_comment: &mut bool) -> String {
    if !*in_comment && !line.contains("<!--") {
        return line.to_owned();
    }
    if *in_comment && !line.contains("-->") {
        return String::new();
    }
    let bytes = line.as_bytes();
    let mut runs = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'`' {
            let start = index;
            while index < bytes.len() && bytes[index] == b'`' {
                index += 1;
            }
            runs.push((start, index));
        } else {
            index += 1;
        }
    }
    let mut next_for_width = std::collections::HashMap::<usize, usize>::new();
    let mut closing = vec![None; runs.len()];
    for run_index in (0..runs.len()).rev() {
        let width = runs[run_index].1 - runs[run_index].0;
        closing[run_index] = next_for_width
            .get(&width)
            .map(|&next_index| runs[next_index].1);
        next_for_width.insert(width, run_index);
    }
    let mut visible = String::new();
    let mut offset = 0;
    let mut run_index = 0;
    while offset < line.len() {
        if *in_comment {
            let Some(end) = line[offset..].find("-->") else {
                break;
            };
            offset += end + 3;
            *in_comment = false;
            visible.push(' ');
        } else if line[offset..].starts_with("<!--") {
            visible.push(' ');
            offset += 4;
            *in_comment = true;
        } else {
            while run_index < runs.len() && runs[run_index].0 < offset {
                run_index += 1;
            }
            if run_index < runs.len() && runs[run_index].0 == offset {
                let end = closing[run_index].unwrap_or(runs[run_index].1);
                visible.push_str(&line[offset..end]);
                offset = end;
            } else {
                let ch = line[offset..].chars().next().expect("valid UTF-8 boundary");
                visible.push(ch);
                offset += ch.len_utf8();
            }
        }
    }
    visible
}

/// ATX headings outside fenced code and HTML comments. Byte offsets preserve Unicode and CRLF.
/// Fences are the citation scanner's (CodeFence), so a fence opened after a
/// list marker (`1. ```bash`) holds its `# comment` lines; read as prose,
/// its indented closer opened a fence that hid every later heading, and a
/// section replace deleted the next section.
pub(super) fn headings(doc: &str) -> Vec<Heading> {
    let mut result: Vec<Heading> = Vec::new();
    // Indices of headings whose section still runs to the end, by rising
    // level: a heading ends every open one at its level or deeper.
    let mut open: Vec<usize> = Vec::new();
    let mut fence: Option<CodeFence> = None;
    let mut in_comment = false;
    let mut offset = 0;
    for (i, line) in doc.split_inclusive('\n').enumerate() {
        let mut start = offset;
        offset += line.len();
        // A byte order mark is no text, and no part of the first section:
        // before, the first heading of a document saved by a Windows editor
        // was not seen at all.
        let line = match line.strip_prefix('\u{feff}') {
            Some(rest) if i == 0 => {
                start += '\u{feff}'.len_utf8();
                rest
            }
            _ => line,
        };
        if let Some(open) = &fence {
            match open.ended_by(line) {
                Some(FenceEnd::Closer) => {
                    fence = None;
                    continue;
                }
                Some(FenceEnd::ListItemEnd) => fence = None,
                None => continue,
            }
        }
        if !in_comment && let Some((opened, _)) = CodeFence::opened_by(line) {
            fence = Some(opened);
            continue;
        }
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let run = trimmed.bytes().take_while(|byte| *byte == b'#').count();
        if indent <= 3
            && !in_comment
            && (1..=6).contains(&run)
            && trimmed
                .as_bytes()
                .get(run)
                .is_none_or(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            while let Some(&prior) = open.last()
                && result[prior].level >= run
            {
                result[prior].end = start;
                open.pop();
            }
            open.push(result.len());
            result.push(Heading {
                heading: line.trim().to_string(),
                start,
                end: doc.len(),
                line: i + 1,
                level: run,
            });
        }
        if in_comment || indent < 4 {
            in_comment = html_comment_state(line, in_comment);
        }
    }
    result
}

pub(super) fn heading_paths(headings: &[Heading]) -> Vec<String> {
    let mut stack: Vec<(usize, &str)> = Vec::new();
    headings
        .iter()
        .map(|heading| {
            while stack
                .last()
                .is_some_and(|(level, _)| *level >= heading.level)
            {
                stack.pop();
            }
            stack.push((heading.level, &heading.heading));
            stack
                .iter()
                .map(|(_, title)| *title)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect()
}

/// How much of a document's outline a tool result or model request carries.
pub(super) enum OutlineView {
    /// One page of the complete outline, continued by heading index.
    Page { offset: usize, limit: usize },
    /// The complete outline when it has at most `max` headings. Otherwise
    /// the headings inside the one-based inclusive line ranges with their
    /// ancestors and a few neighbours, then each heading level from the top
    /// while it fits, the first level that does not fit in part. A focus
    /// covering much of the outline (a whole-document write) adds nothing,
    /// so the levels alone remain. The runtime's automatic outlines in save
    /// results use this view, because each is repeated per save and shares
    /// the result budget; document_inspect pages the complete outline.
    Around {
        lines: Vec<(usize, usize)>,
        max: usize,
    },
}

pub(super) struct Outline {
    /// Indices into the heading list, in document order.
    pub indices: Vec<usize>,
    pub heading_count: usize,
}

impl Outline {
    pub(super) fn omitted(&self) -> usize {
        self.heading_count - self.indices.len()
    }
    /// heading, level and start_line of each entry, with section_path when
    /// `paths` is given (the edit tools take it as `section`).
    pub(super) fn entries(&self, headings: &[Heading], paths: Option<&[String]>) -> Vec<Value> {
        self.indices
            .iter()
            .map(|&index| {
                let heading = &headings[index];
                let mut entry = json!({"heading":heading.heading,"level":heading.level,"start_line":heading.line});
                if let Some(paths) = paths {
                    entry["section_path"] = json!(paths[index]);
                }
                entry
            })
            .collect()
    }
}

/// Headings kept on each side of a focus range in an `Around` outline.
const OUTLINE_NEIGHBOURS: usize = 4;

pub(super) fn outline(headings: &[Heading], view: OutlineView) -> Outline {
    let total = headings.len();
    let indices = match view {
        OutlineView::Page { offset, limit } => {
            (offset.min(total)..offset.saturating_add(limit).min(total)).collect()
        }
        OutlineView::Around { max, .. } if total <= max => (0..total).collect(),
        OutlineView::Around { lines, max } => around(headings, &lines, max),
    };
    Outline {
        indices,
        heading_count: total,
    }
}

fn around(headings: &[Heading], ranges: &[(usize, usize)], max: usize) -> Vec<usize> {
    // The nearest earlier heading of a smaller level is the parent.
    let mut parents = vec![None; headings.len()];
    let mut stack: Vec<usize> = Vec::new();
    for (index, heading) in headings.iter().enumerate() {
        while stack
            .last()
            .is_some_and(|&top| headings[top].level >= heading.level)
        {
            stack.pop();
        }
        parents[index] = stack.last().copied();
        stack.push(index);
    }
    let mut near = BTreeSet::new();
    let mut neighbours = BTreeSet::new();
    for &(first, last) in ranges {
        let mut inside: Vec<usize> = (0..headings.len())
            .filter(|&index| (first..=last).contains(&headings[index].line))
            .collect();
        if inside.is_empty() {
            // Lines inside a section's body: that section.
            inside.extend(headings.iter().rposition(|heading| heading.line < first));
        }
        let (Some(&lowest), Some(&highest)) = (inside.first(), inside.last()) else {
            continue;
        };
        for index in inside {
            let mut ancestor = Some(index);
            while let Some(index) = ancestor {
                if !near.insert(index) {
                    break;
                }
                ancestor = parents[index];
            }
        }
        neighbours.extend(lowest.saturating_sub(OUTLINE_NEIGHBOURS)..lowest);
        neighbours.extend(highest + 1..(highest + 1 + OUTLINE_NEIGHBOURS).min(headings.len()));
    }
    let mut tiers: Vec<Vec<usize>> = Vec::new();
    // Nearness means little once the focus covers much of the outline.
    if !near.is_empty() && near.len() <= max / 2 {
        tiers.push(near.iter().copied().collect());
        tiers.push(neighbours.difference(&near).copied().collect());
    }
    let deepest = headings
        .iter()
        .map(|heading| heading.level)
        .max()
        .unwrap_or(0);
    for level in 1..=deepest {
        tiers.push(
            (0..headings.len())
                .filter(|&index| headings[index].level == level)
                .collect(),
        );
    }
    let mut selected = BTreeSet::new();
    for tier in tiers {
        let fresh: Vec<usize> = tier
            .into_iter()
            .filter(|index| !selected.contains(index))
            .collect();
        if selected.len() + fresh.len() <= max {
            selected.extend(fresh);
        } else {
            let room = max - selected.len();
            selected.extend(fresh.into_iter().take(room));
            break;
        }
    }
    selected.into_iter().collect()
}

pub(super) fn heading_path(doc: &str, start: usize) -> Result<String> {
    HeadingIndex::new(doc).path_at(start).map(str::to_owned)
}

/// Character bigrams of a title, ignoring case, spacing and punctuation,
/// sorted so equal pairs are adjacent.
fn title_bigrams(text: &str) -> Vec<(char, char)> {
    let chars: Vec<char> = text
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    let mut pairs: Vec<_> = chars.windows(2).map(|pair| (pair[0], pair[1])).collect();
    pairs.sort_unstable();
    pairs
}

/// The requested title's bigrams with their counts, and how many there are.
type TitleTarget = (HashMap<(char, char), usize>, usize);

fn title_target(text: &str) -> TitleTarget {
    let pairs = title_bigrams(text);
    let mut counts = HashMap::new();
    for pair in &pairs {
        *counts.entry(*pair).or_insert(0) += 1;
    }
    (counts, pairs.len())
}

/// Dice similarity of the character bigrams of two titles, ignoring case,
/// spacing and punctuation, so Korean and English wording rank alike. The
/// requested title's bigrams are counted once (`target`): matched pair by
/// pair against each heading, a 3000-character section name took 1.8 s
/// against 40,000 headings.
fn title_similarity(target: &TitleTarget, title: &str) -> f64 {
    let title = title_bigrams(title);
    if target.1 == 0 || title.is_empty() {
        return 0.0;
    }
    let shared: usize = title
        .chunk_by(|a, b| a == b)
        .map(|run| run.len().min(target.0.get(&run[0]).copied().unwrap_or(0)))
        .sum();
    (2 * shared) as f64 / (target.1 + title.len()) as f64
}

pub(super) fn bare_heading_title(heading: &str) -> &str {
    let content = heading.trim_start_matches('#').trim_start();
    let without_closing = content.trim_end_matches('#');
    if without_closing.len() < content.len()
        && without_closing
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace)
    {
        without_closing.trim_end()
    } else {
        content
    }
}

/// A title's section number (if any) and its text before the first colon.
fn short_title(title: &str) -> (Option<&str>, &str) {
    let number = super::section_number(title);
    let rest = match number {
        Some(label) => title[title.find(label).unwrap() + label.len()..]
            .trim_start_matches(['.', ')'])
            .trim_start(),
        None => title,
    };
    let core = rest.split([':', '：']).next().unwrap_or(rest).trim();
    (number, core)
}

/// Full headings, unique bare titles, or newline-separated ancestor paths
/// whose lines may be either form.
pub(super) struct HeadingIndex {
    headings: Vec<Heading>,
    paths: Vec<String>,
}

impl HeadingIndex {
    pub(super) fn new(doc: &str) -> Self {
        let headings = headings(doc);
        let paths = heading_paths(&headings);
        Self { headings, paths }
    }

    /// The section path of the heading starting at byte `start`.
    pub(super) fn path_at(&self, start: usize) -> Result<&str> {
        // Headings are in document order.
        self.headings
            .binary_search_by_key(&start, |heading| heading.start)
            .ok()
            .map(|index| self.paths[index].as_str())
            .ok_or_else(|| anyhow::anyhow!("section_not_found: heading position changed"))
    }

    pub(super) fn resolve(&self, requested: &str) -> Result<&Heading> {
        match self.resolve_exact(requested) {
            Err(error) if error.to_string().starts_with("section_not_found") => {
                match split_joined_path(requested) {
                    Some(split) => self.resolve_exact(&split).map_err(|_| error),
                    None => Err(error),
                }
            }
            result => result,
        }
    }

    fn resolve_exact(&self, requested: &str) -> Result<&Heading> {
        let requested = requested.trim();
        let headings = &self.headings;
        let paths = &self.paths;
        let requested_path = requested
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let is_path = requested_path.contains('\n');
        let matches = |(index, h): &(usize, &Heading)| {
            if requested.is_empty() {
                return false;
            }
            if is_path {
                paths[*index] == requested_path
            } else if requested.starts_with('#') {
                h.heading == requested
            } else {
                bare_heading_title(&h.heading) == requested
            }
        };
        let mut matching: Vec<_> = headings.iter().enumerate().filter(matches).collect();
        // A heading named by its short form, e.g. "## 1. 처음 설정" or "처음 설정"
        // for "## 1. 처음 설정: 모델 연결 정보 입력": compare the title before a
        // colon without its section number. Section numbers must agree when both
        // have one, and only a single match counts.
        if matching.is_empty() && !is_path {
            let (number, core) = short_title(bare_heading_title(requested));
            if core.chars().count() >= 2 {
                let short: Vec<_> = headings
                    .iter()
                    .enumerate()
                    .filter(|(_, h)| {
                        let (other_number, other_core) =
                            short_title(bare_heading_title(&h.heading));
                        other_core == core
                            && (number.is_none()
                                || other_number.is_none()
                                || number == other_number)
                    })
                    .collect();
                if short.len() == 1 {
                    matching = short;
                }
            }
        }
        // A path may mix full headings and bare titles line by line; compare bare
        // titles only when the exact path found nothing.
        if matching.is_empty() && is_path {
            let requested: Vec<_> = requested_path.split('\n').map(bare_heading_title).collect();
            matching = headings
                .iter()
                .enumerate()
                .filter(|(index, _)| {
                    paths[*index]
                        .split('\n')
                        .map(bare_heading_title)
                        .eq(requested.iter().copied())
                })
                .collect();
        }
        if matching.len() != 1 {
            const SHOWN: usize = 8;
            let candidate_indices: Vec<_> = if matching.is_empty() {
                // Only some headings fit: the closest titles help more than
                // the first ones, which a live model then distrusted.
                let target = title_target(bare_heading_title(
                    requested_path.lines().last().unwrap_or(requested),
                ));
                let mut indices: Vec<_> = (0..headings.len()).collect();
                if indices.len() > SHOWN {
                    let score: Vec<_> = headings
                        .iter()
                        .map(|h| title_similarity(&target, bare_heading_title(&h.heading)))
                        .collect();
                    indices.sort_by(|a, b| score[*b].total_cmp(&score[*a]).then(a.cmp(b)));
                    indices.truncate(SHOWN);
                }
                indices
            } else {
                matching
                    .iter()
                    .map(|(index, _)| *index)
                    .take(SHOWN)
                    .collect()
            };
            let candidates: Vec<_> = candidate_indices
            .into_iter()
            .map(|index| json!({"heading":headings[index].heading,"section_path":paths[index],"start_line":headings[index].line}))
            .collect();
            if matching.is_empty() {
                let listed = if headings.len() > SHOWN {
                    format!(
                        "The {SHOWN} closest of {} headings (partial list; document_inspect without section shows all)",
                        headings.len()
                    )
                } else {
                    "Headings".to_owned()
                };
                bail!(
                    "section_not_found: {requested:?}; use document_inspect without section for the outline. {listed}: {}",
                    json!(candidates)
                );
            }
            let listed = if matching.len() > SHOWN {
                format!("First {SHOWN} matches")
            } else {
                "Matches".to_owned()
            };
            bail!(
                "ambiguous_section: {requested:?} matches {} headings; copy section_path from the document_inspect outline to distinguish nested headings. If the full paths also repeat, use a unique text anchor for editing. {listed}: {}",
                matching.len(),
                json!(candidates)
            );
        }
        Ok(matching[0].1)
    }
}

/// Whether a path argument names the configured output: its own path, that
/// path relative to project.root, or its bare file name when no project file
/// has that name (read_path's rule for an output outside the root). An
/// existing output also matches through symlinks, such as /var and
/// /private/var.
/// Whether two paths name the same file, resolving symlinks when both exist.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (a.canonicalize(), b.canonicalize()),
            (Ok(a), Ok(b)) if a == b
        )
}

pub(super) fn names_output(s: &Session, path: &str, output: &Path) -> bool {
    let given = Path::new(path);
    // Joining an absolute path yields that path.
    let candidate = s.project.root.join(given);
    candidate == output
        || s.project
            .root
            .canonicalize()
            .is_ok_and(|root| root.join(given) == output)
        || super::output_name_at_root(&s.project, &candidate).is_some()
        || output
            .canonicalize()
            .is_ok_and(|real| candidate.canonicalize().is_ok_and(|named| named == real))
}

/// A section path whose headings arrived on one line, joined by a literal
/// "\\n" or by nothing ("# Guide## 4. Flow"), split back into lines; None
/// when that changes nothing. Live models sent both and were refused as naming
/// an unknown section although every heading exists (nine times in one run).
fn split_joined_path(requested: &str) -> Option<String> {
    let text = requested.trim();
    if !text.starts_with('#') {
        return None;
    }
    let chars: Vec<char> = text.replace("\\n", "\n").chars().collect();
    let mut split = String::with_capacity(text.len() + 8);
    for (index, &c) in chars.iter().enumerate() {
        if c == '#' && index > 0 && !matches!(chars[index - 1], '#' | '\n') {
            let run = chars[index..]
                .iter()
                .take_while(|&&next| next == '#')
                .count();
            if (1..=6).contains(&run) && chars.get(index + run) == Some(&' ') {
                split.push('\n');
            }
        }
        split.push(c);
    }
    (split != text).then_some(split)
}

/// Each section's own lines, from its heading to the next heading of any
/// level (the text before the first heading is a section too), as (first
/// line, fingerprint). The fingerprint hashes the trimmed nonblank lines, so
/// blank-line and indentation changes keep it and a section that moves
/// keeps it too.
pub(super) fn section_fingerprints(doc: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = doc.lines().collect();
    let mut starts = vec![1];
    starts.extend(
        headings(doc)
            .iter()
            .map(|heading| heading.line)
            .filter(|&line| line > 1),
    );
    starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            let end = starts
                .get(index + 1)
                .map_or(lines.len(), |next| next - 1)
                .max(start - 1);
            let text = lines[(start - 1).min(lines.len())..end.min(lines.len())]
                .iter()
                .map(|line| line.trim())
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            (start, super::hash(text.as_bytes()))
        })
        .collect()
}

pub(super) fn resolve_heading(doc: &str, requested: &str) -> Result<Heading> {
    HeadingIndex::new(doc).resolve(requested).cloned()
}

pub(super) fn execute(
    s: &mut Session,
    name: &str,
    args: &Value,
    _cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    match name {
        "document_inspect" => {
            let output = output_path(&s.project);
            let output_file = output.as_ref().ok().cloned();
            let path = match (args["path"].as_str(), &output) {
                // Before the first write the configured output has nothing
                // to resolve. Named by its path, it is reported missing as a
                // call without a path is, not as file_not_found (a live model
                // asked for it that way first).
                (Some(path), Ok(output)) if !output.exists() && names_output(s, path, output) => {
                    output.clone()
                }
                (Some(path), _) => super::read_path_suggesting(&s.project, path)?,
                (None, _) => output?,
            };
            if !path.exists() {
                // Advise creation only when the edit tool is offered: the
                // answer workflow writes the output only if the user enabled
                // document_edit.
                let guidance = if s.is_document_work() || s.active_tools.contains("document_edit") {
                    "Nothing has been written to the configured output yet. Create it with document_edit action=create before reading or auditing it."
                } else {
                    "Nothing has been written to the configured output yet."
                };
                return Ok(json!({"exists":false,"path":path,"total_lines":0,"guidance":guidance}));
            }
            let doc = read_text(&path)?;
            let digest = hash(doc.as_bytes());
            let document_offset = n(args, "offset", 0);
            let coverage_offset = n(args, "coverage_offset", 0);
            // Models page with offset alone (14 times in 12 live runs, 5 in
            // one). The hash the last page of this file returned still names
            // the current text, so the pages stay consistent.
            let remembered = s
                .last_inspected
                .as_ref()
                .filter(|(seen, hash)| *hash == digest && same_file(seen, &path))
                .map(|(_, hash)| hash.as_str());
            let expected_hash = args["expected_hash"].as_str().or(remembered);
            if (document_offset > 0 || coverage_offset > 0) && expected_hash.is_none() {
                // An offset past the outline is a line number taken for a
                // heading index: a live model sent offset 106 five times,
                // asked four times for a hash before hearing what offset
                // means. Say that first.
                if args["section"].as_str().is_none() {
                    let count = headings(&doc).len();
                    if document_offset > count {
                        return Err(outline_offset_error(document_offset, count));
                    }
                }
                bail!(
                    "document_hash_required: offset or coverage_offset > 0 requires expected_hash from the first document_inspect result; copy its hash or the returned next_cursor arguments. If that result is unavailable, call document_inspect with offset 0 and coverage_offset 0 first. offset counts outline headings (characters within section when section is given), not document lines: read a line range with file_read start_line"
                );
            }
            // Offset 0 starts a fresh read that returns the current hash, so a
            // stale or placeholder expected_hash has nothing to protect there.
            if let Some(expected) = expected_hash
                && (document_offset > 0 || coverage_offset > 0)
                && expected != digest
            {
                // Saying the document changed would be false for a value no
                // tool issued.
                if !super::is_document_hash(expected) {
                    bail!(
                        "document_revision_conflict: expected_hash is not a document hash (a SHA-256 hash is 64 hex characters; got {}); copy hash from the first document_inspect result or its next_cursor arguments exactly, or restart with offset 0 and coverage_offset 0",
                        expected.chars().count()
                    );
                }
                bail!(
                    "document_revision_conflict: document changed during paged read; restart document_inspect with offset 0 and use its new hash"
                );
            }
            s.last_inspected = Some((path.clone(), digest.clone()));
            let mut result = json!({"exists":true,"path":path,"hash":digest,"total_lines":doc.lines().count(),"bytes":doc.len()});
            // Another document read by path is not the one the edit tools
            // change. A live run inspected the project's own user manual,
            // took its outdated links for its output's text, and spent its
            // closing requests editing passages its output no longer had.
            if let Some(output) = &output_file
                && !same_file(&path, output)
            {
                result["configured_output"] = json!(false);
                result["note"] = json!(format!(
                    "This is {}, not the configured output {}. document_edit, document_edit_batch and document_audit change and check only the configured output; call document_inspect without path to read it.",
                    path.display(),
                    output.display()
                ));
            }
            if let Some(heading) = args["section"].as_str() {
                let resolved = resolve_heading(&doc, heading)?;
                let section = &doc[resolved.start..resolved.end];
                let offset = document_offset;
                let section_chars = section.chars().count();
                if offset > section_chars {
                    bail!(
                        "invalid_offset: offset {offset} is beyond {section_chars} characters in section {:?}; use content.next_offset from the prior page or restart at offset 0",
                        resolved.heading
                    );
                }
                result["section"] = json!(resolved.heading);
                result["section_path"] = json!(heading_path(&doc, resolved.start)?);
                result["section_hash"] = json!(hash(section.as_bytes()));
                result["read_offset"] = json!(n(args, "offset", 0));
                result["content"] = bounded_text(s, section, n(args, "offset", 0));
                result["start_line"] = json!(resolved.line);
                result["section_lines"] = json!(section.lines().count());
            } else {
                let headings = headings(&doc);
                let paths = heading_paths(&headings);
                let offset = n(args, "offset", 0);
                if offset > headings.len() {
                    return Err(outline_offset_error(offset, headings.len()));
                }
                let limit = n(args, "limit", 50).clamp(1, 100);
                let (coverage, read_lines) = super::coverage::report(
                    s,
                    &path,
                    &doc,
                    n(args, "coverage_offset", 0),
                    n(args, "limit", 50),
                );
                let missing_count = coverage["missing_range_count"].as_u64().unwrap_or(0);
                if coverage_offset as u64 > missing_count {
                    bail!(
                        "invalid_offset: coverage_offset {coverage_offset} exceeds {missing_count} missing ranges; restart at coverage_offset 0 or copy coverage.next_offset"
                    );
                }
                let coverage_key = |page: usize| format!("{}:{}:{page}", path.display(), digest);
                if coverage_offset > 0 {
                    let expected = args["expected_coverage_revision"]
                        .as_str()
                        .or_else(|| {
                            s.coverage_cursors
                                .get(&coverage_key(coverage_offset))
                                .map(String::as_str)
                        })
                        .ok_or_else(|| anyhow::anyhow!(
                            "document_coverage_revision_conflict: coverage page is no longer retained; copy expected_coverage_revision from its first page or restart document_inspect with coverage_offset 0"
                        ))?;
                    if coverage["revision"].as_str() != Some(expected) {
                        bail!(
                            "document_coverage_revision_conflict: delivered coverage changed during pagination; restart document_inspect with coverage_offset 0"
                        );
                    }
                }
                // Keep revisions for only this document version. Discarded
                // legacy pages must restart or supply their coverage revision,
                // including when a file later reverts to identical old bytes.
                let current_path = path.to_string_lossy();
                s.coverage_cursors.retain(|key, _| {
                    let mut parts = key.rsplitn(3, ':');
                    let _offset = parts.next();
                    let version = parts.next();
                    let cursor_path = parts.next();
                    cursor_path != Some(current_path.as_ref()) || version == Some(digest.as_str())
                });
                if coverage_offset > 0 {
                    s.coverage_cursors.remove(&coverage_key(coverage_offset));
                }
                if let (Some(next), Some(revision)) = (
                    coverage["next_offset"].as_u64(),
                    coverage["revision"].as_str(),
                ) {
                    s.coverage_cursors
                        .insert(coverage_key(next as usize), revision.into());
                }
                result["coverage"] = coverage;
                let outline = outline(&headings, OutlineView::Page { offset, limit });
                let mut entries = outline.entries(&headings, Some(&paths));
                for (entry, &index) in entries.iter_mut().zip(&outline.indices) {
                    let h = &headings[index];
                    let lines = doc[h.start..h.end].lines().count();
                    entry["lines"] = json!(lines);
                    entry["hash"] = json!(hash(&doc.as_bytes()[h.start..h.end]));
                    entry["fully_read"] = json!(
                        read_lines[h.line - 1..h.line - 1 + lines]
                            .iter()
                            .all(|&v| v)
                    );
                }
                let end = outline.indices.last().map_or(offset, |index| index + 1);
                result["outline"] = json!(entries);
                result["heading_count"] = json!(outline.heading_count);
                result["next_offset"] = json!((end < headings.len()).then_some(end));
            }
            Ok(result)
        }
        "document_audit" => {
            revalidate(s)?;
            let path = output_path(&s.project)?;
            if !path.exists() {
                bail!(
                    "document_missing: the configured output has not been written yet; create it with document_edit action=create before auditing it"
                );
            }
            let doc = read_text(&path)?;
            let document_hash = hash(doc.as_bytes());
            let (checked, issues, format_check) = audit_issues(s, &path, &doc, &document_hash)?;
            // The issue list includes delivered-evidence state as well as
            // document citations. Bind every continuation page to the
            // exact list that produced the first page.
            let revision = hash(&serde_json::to_vec(&(
                document_hash.as_str(),
                checked,
                &issues,
                &format_check,
            ))?);
            let offset = n(args, "offset", 0);
            if offset > 0 {
                // As with document_inspect: the revision the last audit page
                // returned, while it still names the current audit inputs.
                let remembered = s
                    .last_audit_revision
                    .as_deref()
                    .filter(|seen| *seen == revision);
                let expected = args["expected_revision"].as_str().or(remembered).ok_or_else(|| {
                    anyhow::anyhow!(
                        "document_audit_revision_required: offset > 0 requires expected_revision from the first result; copy its revision or restart at offset 0"
                    )
                })?;
                if expected != revision {
                    if !super::is_document_hash(expected) {
                        bail!(
                            "document_audit_revision_conflict: expected_revision is not an audit revision (64 hex characters; got {}); copy revision from the first document_audit result exactly, or restart at offset 0",
                            expected.chars().count()
                        );
                    }
                    bail!(
                        "document_audit_revision_conflict: audit inputs changed during pagination; restart document_audit at offset 0"
                    );
                }
            }
            if offset > issues.len() {
                bail!(
                    "invalid_offset: audit issue offset {offset} exceeds {} issues; restart document_audit at offset 0 or copy its prior next_offset",
                    issues.len()
                );
            }
            s.last_audit_revision = Some(revision.clone());
            let end = (offset + n(args, "limit", 30).clamp(1, 100)).min(issues.len());
            let structural_ok = issues
                .iter()
                .all(|issue| issue["kind"] == "no_machine_readable_citations");
            let mut result = json!({"hash":document_hash,"revision":revision,"total_lines":doc.lines().count(),"citations_checked":checked,"structural_ok":structural_ok,"format_check":format_check,"issue_count":issues.len(),"issue_kinds":issue_kinds(&issues),"issues":issues[offset..end],"next_offset":(end<issues.len()).then_some(end)});
            if s.is_document_work()
                && let Some(check) = test_code_check(s, &path, &doc)
            {
                result["test_code_check"] = check;
            }
            if s.is_document_work()
                && let Some(check) = citation_mismatch_check(s, &path, &doc)
            {
                result["citation_mismatch_check"] = check;
            }
            Ok(result)
        }
        _ => bail!("unsupported_tool"),
    }
}

/// An outline offset past the document's headings, which models send as a
/// line number.
fn outline_offset_error(offset: usize, headings: usize) -> anyhow::Error {
    anyhow::anyhow!(
        "invalid_offset: offset {offset} is an outline heading index, but this document has {headings} headings; it is not a document line number. Use file_read with start_line to read a line range, or copy next_offset from the previous document_inspect outline page"
    )
}

/// document_audit's issues for the output text: citation issues, format
/// issues, a missing-citations issue, the output's own path, then unread
/// citations; with the number of citations checked and the format summary.
fn audit_issues(
    s: &Session,
    path: &Path,
    doc: &str,
    digest: &str,
) -> Result<(usize, Vec<Value>, Value)> {
    let (checked, mut issues) = citation_issues(s, path, doc)?;
    let format = super::document_format::checked(doc, digest);
    let format_check = format.summary();
    // The citation scanner already catches many unclosed fences. Keep
    // one diagnostic per fence while adding AST coverage for containers.
    let mut fence_lines: BTreeSet<_> = issues
        .iter()
        .filter(|issue| issue["kind"] == "unclosed_code_fence")
        .filter_map(|issue| issue["line"].as_u64())
        .collect();
    for issue in &format.issues {
        if issue["kind"] != "unclosed_code_fence"
            || issue["line"]
                .as_u64()
                .is_none_or(|line| fence_lines.insert(line))
        {
            issues.push(issue.clone());
        }
    }
    if checked == 0 {
        issues
            .push(json!({"kind":"no_machine_readable_citations","guidance":NO_CITATIONS_GUIDANCE}));
    }
    // The output's own absolute path in its text is a run report
    // (where it was written), which belongs in the final answer.
    if let Some(line) = output_path_line(path, doc) {
        issues.push(json!({"kind":"output_path_in_document","line":line,
            "guidance":"The document states its own output path, a report of how it was produced. Remove that text; the final chat answer reports it."}));
    }
    issues.extend(unread_citations(s, doc)?);
    Ok((checked, issues, format_check))
}

/// How many issues document_audit reports for the output text.
pub(super) fn open_issue_count(s: &Session, path: &Path, doc: &str, digest: &str) -> Result<usize> {
    Ok(audit_issues(s, path, doc, digest)?.1.len())
}

/// Issue counts by kind, so a page of issues still shows the whole set.
fn issue_kinds(issues: &[Value]) -> Value {
    let mut kinds = BTreeMap::<&str, usize>::new();
    for issue in issues {
        *kinds
            .entry(issue["kind"].as_str().unwrap_or("other"))
            .or_default() += 1;
    }
    json!(kinds)
}

/// The document lines of the output's open citation and format issues,
/// in order and at most `limit`, for a read of the output to start near.
pub(super) fn open_issue_lines(
    s: &Session,
    output: &Path,
    doc: &str,
    digest: &str,
    limit: usize,
) -> Vec<u64> {
    let mut lines = BTreeSet::new();
    if let Ok((_, issues, _)) = citation_issues_and_paths(s, output, doc, false) {
        lines.extend(issues.iter().filter_map(|issue| issue["line"].as_u64()));
    }
    lines.extend(
        super::document_format::checked(doc, digest)
            .issues
            .iter()
            .filter_map(|issue| issue["line"].as_u64()),
    );
    lines.into_iter().take(limit).collect()
}

/// The first document line naming the output's absolute path, in either its
/// configured or canonical spelling (e.g. /var/... and /private/var/...).
fn output_path_line(path: &Path, doc: &str) -> Option<usize> {
    let mut spellings = vec![path.display().to_string()];
    if let Ok(canonical) = path.canonicalize() {
        spellings.push(canonical.display().to_string());
    }
    if let Some(stripped) = spellings.iter().find_map(|p| p.strip_prefix("/private")) {
        spellings.push(stripped.to_owned());
    }
    doc.lines()
        .position(|line| spellings.iter().any(|p| line.contains(p.as_str())))
        .map(|index| index + 1)
}

/// Citations in prose and Mermaid are references; other fenced code is an example.
pub(super) struct Citation {
    pub raw: String,
    pub path: String,
    pub begin: usize,
    pub end: usize,
    pub relative_link: bool,
    pub document_line: usize,
    /// Byte range of the citation in `line_text`, its document line with HTML
    /// comments removed, and whether it stands in prose rather than in a
    /// diagram.
    pub columns: (usize, usize),
    pub line_text: Arc<str>,
    pub prose: bool,
}

/// A missing cited range this long is more likely a pointer than evidence.
const BROAD_CITATION_LINES: usize = 120;

/// Cited ranges never delivered to the model as complete lines of the current
/// file version in this session, merged per file. The runtime derives this
/// from the delivered-source registry on every save and audit, so no
/// bookkeeping call attests a comparison: the model reads a listed range or
/// narrows the citation to the lines it read. Unreadable paths and invalid
/// ranges are the citation check's issues, not unread evidence.
pub(super) fn unread_citations(s: &Session, doc: &str) -> Result<Vec<Value>> {
    let output = output_path(&s.project)?;
    // Delivered complete-line ranges of each file's current version.
    // Looked up once or twice per citation: hashed, as comparing paths by
    // component took a third of the check for 80,000 citations.
    let mut versions = HashMap::<PathBuf, Option<String>>::new();
    let mut delivered = HashMap::<PathBuf, Vec<(usize, usize)>>::new();
    // Every source of a long session is checked on each request; resolve
    // each file once rather than once per source.
    let mut paths = PathResolver::new(&s.project);
    let citations = citation_spans(doc)?;
    let cited_path = |citation: &Citation| {
        if citation.relative_link {
            output
                .parent()
                .unwrap()
                .join(&citation.path)
                .to_string_lossy()
                .into_owned()
        } else {
            citation.path.clone()
        }
    };
    // Only a cited file's sources can attest a citation. Resolving and
    // hashing every file the session had read, several times a round, made
    // each round slower with every file read (200 ms per check at 6000).
    // Sources are kept under their resolved paths, so the cited files' names
    // tell which to resolve.
    let cited_names: BTreeSet<std::ffi::OsString> = citations
        .iter()
        .filter_map(|citation| paths.lookup(&cited_path(citation)))
        .filter_map(|path| path.file_name().map(ToOwned::to_owned))
        .collect();
    for source in s.sources.values() {
        if source.origin != "file" || source.evidence_truncated {
            continue;
        }
        let (Some(path), Some(start), Some(end)) =
            (source.path.as_deref(), source.start_line, source.end_line)
        else {
            continue;
        };
        if !Path::new(path)
            .file_name()
            .is_some_and(|name| cited_names.contains(name))
        {
            continue;
        }
        let Some(resolved) = paths.lookup(path) else {
            continue;
        };
        let current = versions
            .entry(resolved.clone())
            .or_insert_with(|| hash_file(&resolved).ok());
        if current.as_deref() != source.hash.as_deref() {
            continue;
        }
        // A cursor may begin or end in the middle of a line; such a line is
        // navigation context, not an attested citation.
        let start = if source.line_start_complete {
            start
        } else {
            start.saturating_add(1)
        };
        let end = if source.line_end_complete {
            end
        } else {
            end.saturating_sub(1)
        };
        if start <= end {
            delivered.entry(resolved).or_default().push((start, end));
        }
    }
    // Overlapping and adjacent reads merged, so each citation finds its
    // first delivered range by binary search: walking every range of the
    // file for every citation grew with both.
    for ranges in delivered.values_mut() {
        *ranges = merged_ranges(std::mem::take(ranges));
    }
    let mut missing = HashMap::<PathBuf, Vec<(usize, usize)>>::new();
    let mut line_counts = HashMap::<PathBuf, usize>::new();
    // The document is not evidence for itself: a self-citation would go
    // unread again after every edit, so it is never owed a read.
    let own = output.canonicalize().ok();
    // A section an existing output already had when this task began, and
    // still has unchanged, is earlier work: this task owes a read only for
    // citations in the sections it wrote or changed. A live correction
    // limited to two sections was refused 31 times for unread citations in
    // the other four. Exempting only unchanged lines let a section's wrong
    // claim pass because the model rewrote that line as it was.
    let unchanged = unchanged_sections(s, &output, doc);
    for citation in citations.iter() {
        if unchanged(citation.document_line) {
            continue;
        }
        let Some(cited) = paths.lookup(&cited_path(citation)) else {
            continue;
        };
        if own.as_ref() == Some(&cited) {
            continue;
        }
        // A reversed range such as `file.rs:10-5` must not read as an empty,
        // fully covered interval, and a range past the end of the file is
        // not unread evidence; the citation check reports both.
        let total = *line_counts
            .entry(cited.clone())
            .or_insert_with(|| super::text_line_count(&cited).unwrap_or(0));
        if citation.begin == 0 || citation.end < citation.begin || citation.end > total {
            continue;
        }
        let mut next = citation.begin;
        let gaps = missing.entry(cited.clone()).or_default();
        let ranges = delivered.get(&cited).map(Vec::as_slice).unwrap_or(&[]);
        let first = ranges.partition_point(|&(_, end)| end < next);
        for &(start, end) in &ranges[first..] {
            if start > citation.end {
                break;
            }
            if start > next {
                gaps.push((next, start - 1));
            }
            next = next.max(end.saturating_add(1));
            if next > citation.end {
                break;
            }
        }
        if next <= citation.end {
            gaps.push((next, citation.end));
        }
    }
    // Issues are listed by path.
    let mut missing: BTreeMap<PathBuf, Vec<(usize, usize)>> = missing.into_iter().collect();
    // A line longer than one page reaches the model in pieces, each one a
    // partial line that attests nothing alone; the text coverage of the
    // pieces delivered shows the whole line was read. Without it, a citation
    // of such a line stayed unread however often it was read.
    for (path, gaps) in &mut missing {
        // Citations of the same lines walk them once.
        *gaps = merged_ranges(std::mem::take(gaps));
        // Only a file with delivered text coverage of its current version
        // is read for it.
        let current = versions
            .entry(path.clone())
            .or_insert_with(|| hash_file(path).ok());
        if gaps.is_empty()
            || s.read_coverage
                .get(path.to_string_lossy().as_ref())
                .is_none_or(|coverage| current.as_deref() != Some(coverage.hash.as_str()))
        {
            continue;
        }
        let Ok(text) = read_text(path) else {
            continue;
        };
        let (_, covered) = super::coverage::report(s, path, &text, 0, 1);
        if !covered.contains(&true) {
            continue;
        }
        let mut unread = Vec::new();
        for &(start, end) in gaps.iter() {
            let mut from = None;
            for line in start..=end {
                let read = covered.get(line - 1).copied().unwrap_or(false);
                match (read, from) {
                    (false, None) => from = Some(line),
                    (true, Some(first)) => {
                        unread.push((first, line - 1));
                        from = None;
                    }
                    _ => {}
                }
            }
            if let Some(first) = from {
                unread.push((first, end));
            }
        }
        *gaps = unread;
    }
    let root = s.project.root.canonicalize()?;
    let mut issues = vec![];
    for (path, gaps) in missing {
        let merged = merged_ranges(gaps);
        let path = path.strip_prefix(&root).unwrap_or(&path).to_string_lossy();
        for (start, end) in merged {
            let mut issue =
                json!({"kind":"unread_citation","path":path,"start_line":start,"end_line":end});
            // A live run read 900 lines in pages because an opening cited a
            // whole screen component as a pointer; offer the cheaper repair.
            if end + 1 - start >= BROAD_CITATION_LINES {
                issue["note"] = json!(
                    "This range is long; a pointer to where something lives cites its entry lines."
                );
            }
            issues.push(issue);
        }
    }
    Ok(issues)
}

/// Inclusive line ranges sorted, with overlapping and adjacent ones joined.
fn merged_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut()
            && start <= last.1.saturating_add(1)
        {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}

/// Whether a document line lies in a section an existing output already had,
/// unchanged, when this task began (Session::document_baseline).
fn unchanged_sections<'a>(s: &'a Session, output: &Path, doc: &str) -> impl Fn(usize) -> bool + 'a {
    let sections = match &s.document_baseline {
        Some((path, _)) if path == output => section_fingerprints(doc),
        _ => Vec::new(),
    };
    move |line: usize| {
        let index = sections.partition_point(|(start, _)| *start <= line);
        index > 0
            && s.document_baseline
                .as_ref()
                .is_some_and(|(_, kept)| kept.contains(&sections[index - 1].1))
    }
}

/// Words too common in code to tell one cited passage from another.
const COMMON_CODE_WORDS: &[&str] = &[
    "async",
    "await",
    "bool",
    "break",
    "const",
    "continue",
    "else",
    "false",
    "function",
    "impl",
    "json",
    "let",
    "match",
    "null",
    "None",
    "Option",
    "Result",
    "return",
    "self",
    "Self",
    "Some",
    "static",
    "String",
    "struct",
    "this",
    "true",
    "undefined",
    "usize",
    "Value",
];

/// The code names a claim mentions in inline code: identifiers of four or
/// more characters, without file paths and common keywords. A qualified name
/// such as `question::run` or `s.is_document_work()` names its last part;
/// the parts before it are qualifiers, returned second. A live run's claims
/// citing `fn run` were flagged for the module name its lines never repeat,
/// so a claim is checked by its names, and a qualifier found in the cited
/// lines (a call site of `routing::run`) counts as a match too.
fn code_names(span: &str) -> (Vec<String>, Vec<String>) {
    let (mut names, mut qualifiers) = (Vec::new(), Vec::new());
    let mut seen = (HashSet::new(), HashSet::new());
    if span.contains('/') {
        return (names, qualifiers);
    }
    let mut word = String::new();
    let mut chars = span.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        word.push(c);
        if chars
            .peek()
            .is_some_and(|(_, next)| next.is_ascii_alphanumeric() || *next == '_')
        {
            continue;
        }
        let rest = &span[at + c.len_utf8()..];
        let qualifier = rest
            .strip_prefix("::")
            .or_else(|| rest.strip_prefix('.'))
            .is_some_and(|after| {
                after.starts_with(|next: char| next.is_ascii_alphabetic() || next == '_')
            });
        if word.len() >= 4
            && !word.starts_with(|first: char| first.is_ascii_digit())
            && !COMMON_CODE_WORDS.contains(&word.as_str())
        {
            let (list, seen) = if qualifier {
                (&mut qualifiers, &mut seen.1)
            } else {
                (&mut names, &mut seen.0)
            };
            if seen.insert(word.clone()) {
                list.push(word.clone());
            }
        }
        word.clear();
    }
    (names, qualifiers)
}

/// Whether a definition of `name` above the cited lines encloses them: a
/// claim may cite lines inside a function by that function's name, as in
/// "the loop inside `run_session_controlled`". Definitions are keyword forms
/// (fn, function, def, class, ...), JavaScript bindings and properties
/// (`const refresh = useCallback(`, `load: async () =>`) and method
/// shorthand (`async load() {`). A live run cited lines inside `const
/// refresh = useCallback(` as `refresh()`; only keyword forms were known,
/// so the claim was flagged and the model re-read those lines for 30
/// requests. The definition encloses the cited lines while every nonblank
/// line from it through the last cited line is indented deeper, except a
/// line that starts with `)` (a signature continued over several lines).
struct CitedFile {
    lines: Vec<String>,
    text: String,
    /// Byte offset in text of each line.
    starts: Vec<usize>,
    /// Lines defining each name checked so far, ascending.
    definitions: HashMap<String, Vec<usize>>,
}

impl CitedFile {
    fn new(text: String) -> Self {
        let lines: Vec<String> = text.lines().map(str::to_owned).collect();
        let mut starts = Vec::with_capacity(lines.len());
        let mut at = 0;
        for piece in text.split_inclusive('\n').take(lines.len()) {
            starts.push(at);
            at += piece.len();
        }
        Self {
            lines,
            text,
            starts,
            definitions: HashMap::new(),
        }
    }

    fn defined_around(&mut self, name: &str, begin: usize, end: usize) -> bool {
        if !self.definitions.contains_key(name) {
            // One search of the whole text finds the lines holding the
            // name; testing each line above each citation cost a save 5
            // seconds for 2000 claims citing a 20000-line file.
            let mut holding: Vec<usize> = self
                .text
                .match_indices(name)
                .map(|(at, _)| self.starts.partition_point(|start| *start <= at) - 1)
                .collect();
            holding.dedup();
            let mut whole = None;
            holding.retain(|&line| {
                self.lines
                    .get(line)
                    .is_some_and(|text| defines(text, name, &mut whole))
            });
            self.definitions.insert(name.to_owned(), holding);
        }
        let source = &self.lines;
        let indent = |text: &str| text.len() - text.trim_start().len();
        let last = end.min(source.len());
        let above = begin.saturating_sub(1).min(source.len());
        self.definitions[name]
            .iter()
            .rev()
            .filter(|&&at| at < above)
            .any(|&at| {
                let depth = indent(&source[at]);
                source[at + 1..last].iter().all(|line| {
                    let trimmed = line.trim_start();
                    trimmed.is_empty() || indent(line) > depth || trimmed.starts_with(')')
                })
            })
    }
}

/// The definition forms of `name` as one pattern.
fn definition_pattern(name: &str) -> Option<regex::Regex> {
    let name = regex::escape(name);
    regex::Regex::new(&format!(
        r"\b(?:fn|function|def|struct|enum|trait|class|interface|mod|impl)\s+{name}\b|\b(?:const|let|var)\s+{name}\s*(?::[^=]*)?=[^=]|(?:^|[\s,{{]){name}\s*:\s*(?:async\s+)?(?:function\b|\(|[A-Za-z_$][\w$]*\s*=>)|^\s*(?:(?:async|static|get|set|public|private|protected)\s+)*{name}\s*\([^()]*\)\s*\{{\s*$"
    ))
    .ok()
}

/// Occurrences of a name in one line checked form by form. Each check scans
/// the line, so a line holding the name more often is matched by one
/// pattern for the whole line instead.
const DEFINITION_OCCURRENCES: usize = 8;

/// Whether `text` holds a definition form of `name` (a code_names word):
/// the text before an occurrence of the name and the text after it each
/// match their half of one form. A pattern compiled per name cost a save
/// 360ms for 2000 claims citing lines inside their functions, and each
/// compiled pattern held about 180KB; `whole` is that pattern, compiled
/// only for a line with more than DEFINITION_OCCURRENCES occurrences.
fn defines(text: &str, name: &str, whole: &mut Option<Option<regex::Regex>>) -> bool {
    static FORMS: std::sync::OnceLock<Option<Vec<(regex::Regex, regex::Regex)>>> =
        std::sync::OnceLock::new();
    let Some(forms) = FORMS.get_or_init(|| {
        [
            (
                r"\b(?:fn|function|def|struct|enum|trait|class|interface|mod|impl)\s+$",
                r"^(?:\W|$)",
            ),
            (r"\b(?:const|let|var)\s+$", r"^\s*(?::[^=]*)?=[^=]"),
            (
                r"(?:^|[\s,{])$",
                r"^\s*:\s*(?:async\s+)?(?:function\b|\(|[A-Za-z_$][\w$]*\s*=>)",
            ),
            (
                r"^\s*(?:(?:async|static|get|set|public|private|protected)\s+)*$",
                r"^\s*\([^()]*\)\s*\{\s*$",
            ),
        ]
        .into_iter()
        .map(|(before, after)| {
            Some((
                regex::Regex::new(before).ok()?,
                regex::Regex::new(after).ok()?,
            ))
        })
        .collect()
    }) else {
        return false;
    };
    if name.is_empty() {
        return false;
    }
    if text.matches(name).nth(DEFINITION_OCCURRENCES).is_some() {
        return whole
            .get_or_insert_with(|| definition_pattern(name))
            .as_ref()
            .is_some_and(|pattern| pattern.is_match(text));
    }
    let mut from = 0;
    while let Some(found) = text[from..].find(name) {
        let at = from + found;
        let (before, after) = (&text[..at], &text[at + name.len()..]);
        if forms
            .iter()
            .any(|(head, tail)| head.is_match(before) && tail.is_match(after))
        {
            return true;
        }
        // Occurrences may overlap; the name starts with an ASCII character.
        from = at + text[at..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

const CITATION_MISMATCH_LIMIT: usize = 8;
const CITATION_MISMATCH_GUIDANCE: &str = "None of the code names written before these citations occurs in the lines they cite: the claim may describe other code, or the cited lines may have moved. Read the cited lines once, then correct the claim or cite the lines that hold that code; if they do support the claim, leave it as it is. This check only compares names, so a correct claim may stay flagged: do not read the same lines again for it. Advice only; it does not block the final answer.";

/// Citations whose claim names code that the cited lines never mention, such
/// as `checkpoint_complete` cited at lines that hold other code. A live
/// correction read such a range and kept the wrong claim, because reading
/// is all the unread check can ask for. The names are those in inline code
/// between the previous citation on the line and this one; citations with
/// only separators between them share the names and are matched together.
/// Only prose citations in sections this task wrote or changed are checked.
/// Advice only; None when nothing is flagged.
pub(super) fn citation_mismatch_check(s: &Session, output: &Path, doc: &str) -> Option<Value> {
    let citations = citation_spans(doc).ok()?;
    let unchanged = unchanged_sections(s, output, doc);
    let own = output.canonicalize().ok();
    let mut files = HashMap::<PathBuf, Option<CitedFile>>::new();
    let mut paths = PathResolver::new(&s.project);
    let mut items = Vec::new();
    let mut total = 0usize;
    let mut index = 0;
    while index < citations.len() {
        let line = citations[index].document_line;
        let group_end = citations[index..]
            .iter()
            .position(|citation| citation.document_line != line)
            .map_or(citations.len(), |offset| index + offset);
        let on_line = &citations[index..group_end];
        index = group_end;
        if unchanged(line) || !on_line.iter().all(|citation| citation.prose) {
            continue;
        }
        // The line as the citation columns index it: with HTML comments
        // removed, so a code span inside a comment names nothing and the
        // spans after one are not shifted against the columns.
        let text = &*on_line[0].line_text;
        // Inline code spans of the whole line, so a span is never paired
        // across a citation's own backticks.
        let mut spans = Vec::new();
        let mut open = None;
        for (at, c) in text.char_indices() {
            if c == '`' {
                match open.take() {
                    Some(start) => spans.push((start, at + 1)),
                    None => open = Some(at),
                }
            }
        }
        // (names, qualifiers, citations) per claim. A citation with no
        // code between it and the previous one shares that claim; a claim
        // naming only qualifiers (`question::run`) is not checked.
        let mut claims: Vec<(Vec<String>, Vec<String>, Vec<&Citation>)> = Vec::new();
        let mut previous_end = 0;
        for citation in on_line {
            let (start, end) = citation.columns;
            let (mut names, mut qualifiers) = (Vec::new(), Vec::new());
            let mut seen = (HashSet::new(), HashSet::new());
            // Spans are in line order and do not overlap: those between the
            // previous citation and this one are one run.
            let first = spans.partition_point(|(from, _)| *from < previous_end);
            for (from, to) in spans[first..].iter().take_while(|(_, to)| *to <= start) {
                let (span_names, span_qualifiers) = code_names(&text[*from..*to]);
                for (list, seen, found) in [
                    (&mut names, &mut seen.0, span_names),
                    (&mut qualifiers, &mut seen.1, span_qualifiers),
                ] {
                    for name in found {
                        if seen.insert(name.clone()) {
                            list.push(name);
                        }
                    }
                }
            }
            previous_end = end;
            match claims.last_mut() {
                Some((_, _, cited)) if names.is_empty() && qualifiers.is_empty() => {
                    cited.push(citation)
                }
                _ if names.is_empty() && qualifiers.is_empty() => {}
                _ => claims.push((names, qualifiers, vec![citation])),
            }
        }
        for (names, qualifiers, cited) in claims {
            if names.is_empty() {
                continue;
            }
            let mut found = false;
            let mut checked = false;
            for citation in &cited {
                let path = if citation.relative_link {
                    output
                        .parent()
                        .unwrap()
                        .join(&citation.path)
                        .to_string_lossy()
                        .into_owned()
                } else {
                    citation.path.clone()
                };
                let Some(resolved) = paths.lookup(&path) else {
                    continue;
                };
                if own.as_ref() == Some(&resolved) {
                    continue;
                }
                let Some(source) = files
                    .entry(resolved.clone())
                    .or_insert_with(|| read_text(&resolved).ok().map(CitedFile::new))
                    .as_mut()
                else {
                    continue;
                };
                if citation.begin == 0
                    || citation.end < citation.begin
                    || citation.end > source.lines.len()
                {
                    continue;
                }
                checked = true;
                let cited_text = &source.lines[citation.begin - 1..citation.end];
                found |= names.iter().chain(&qualifiers).any(|name| {
                    cited_text
                        .iter()
                        .any(|cited_line| cited_line.contains(name.as_str()))
                }) || names
                    .iter()
                    .any(|name| source.defined_around(name, citation.begin, citation.end));
            }
            if checked && !found {
                total += 1;
                if items.len() < CITATION_MISMATCH_LIMIT {
                    items.push(json!({"line":line,
                        "citations":cited.iter().map(|citation| citation.raw.clone()).collect::<Vec<_>>(),
                        "names":names.iter().take(5).collect::<Vec<_>>()}));
                }
            }
        }
    }
    (total > 0)
        .then(|| json!({"flagged":total,"items":items,"guidance":CITATION_MISMATCH_GUIDANCE}))
}

const UNCITED_CODE_LIMIT: usize = 8;

/// Fence languages for commands, output, data and diagrams rather than code.
const NON_SOURCE_FENCES: &[&str] = &[
    "mermaid",
    "text",
    "txt",
    "plain",
    "plaintext",
    "console",
    "terminal",
    "shell",
    "sh",
    "bash",
    "zsh",
    "fish",
    "powershell",
    "ps1",
    "cmd",
    "bat",
    "output",
    "log",
    "json",
    "jsonc",
    "json5",
    "toml",
    "yaml",
    "yml",
    "ini",
    "env",
    "csv",
    "markdown",
    "md",
    "http",
];

/// Sections this task wrote or changed that show code in a source-language
/// fence and cite no lines in their own text, up to the next heading of any
/// level. A live document finished with a section showing two functions it
/// had never read, then "source: `src/agent.rs` (these functions still need
/// reading)", and passed every check, since checks only look at citations.
/// A citation anywhere in the section counts, including its heading, code
/// comments (`// src/session.rs:431-442`) and footnotes it refers to: other
/// live documents cite code that way. So does one in its parent's own text,
/// below the title level, as when a `### ... src/session.rs:330-386`
/// heading leads a `#### Fields` excerpt. Advice only; None when nothing is
/// flagged.
pub(super) fn uncited_code_check(s: &Session, output: &Path, doc: &str) -> Option<Value> {
    let pattern = regex::Regex::new(CITATION_PATTERN).ok()?;
    let file_exists = |path: &str| super::read_path(&s.project, path).is_ok_and(|p| p.is_file());
    let note = regex::Regex::new(r"^ {0,3}\[\^([^\]]+)\]:").ok()?;
    let lines: Vec<&str> = doc.lines().collect();
    let cited_notes: HashSet<&str> = lines
        .iter()
        .filter_map(|line| {
            let label = note.captures(line)?;
            let rest = &line[label.get(0)?.end()..];
            line_cites(&pattern, rest, &file_exists).then(|| label.get(1).map(|l| l.as_str()))?
        })
        .collect();
    let cites = |line: &&str| {
        line_cites(&pattern, line, &file_exists)
            || (!cited_notes.is_empty() && cites_note(line, &cited_notes))
    };
    let unchanged = unchanged_sections(s, output, doc);
    let headings = headings(doc);
    let mut items = Vec::new();
    let mut total = 0usize;
    // One-based lines of a heading and its own text.
    let own = |index: usize| {
        headings[index].line
            ..headings
                .get(index + 1)
                .map_or(lines.len() + 1, |next| next.line)
    };
    // A parent's own text is checked once for all its subsections.
    let mut checked: Vec<Option<bool>> = vec![None; headings.len()];
    let mut cited = |index: usize| {
        *checked[index].get_or_insert_with(|| {
            let lines_of = own(index);
            lines[lines_of.start - 1..lines_of.end - 1]
                .iter()
                .any(cites)
        })
    };
    // Headings above the current one with rising levels: the last one
    // with a lower level is its parent.
    let mut ancestors: Vec<usize> = Vec::new();
    for (index, heading) in headings.iter().enumerate() {
        while ancestors
            .last()
            .is_some_and(|&prior| headings[prior].level >= heading.level)
        {
            ancestors.pop();
        }
        let parent = ancestors
            .last()
            .copied()
            .filter(|&parent| headings[parent].level >= 2);
        ancestors.push(index);
        if unchanged(heading.line) || cited(index) || parent.is_some_and(&mut cited) {
            continue;
        }
        let own = own(index);
        let mut fence: Option<(char, usize)> = None;
        let mut code = None;
        for number in own.skip(1) {
            let Some(line) = lines.get(number - 1) else {
                break;
            };
            let trimmed = line.trim_start_matches(' ');
            if line.len() - trimmed.len() > 3 {
                continue;
            }
            let first = trimmed.chars().next().unwrap_or(' ');
            if first != '`' && first != '~' {
                continue;
            }
            let run = trimmed.chars().take_while(|c| *c == first).count();
            if let Some((marker, width)) = fence {
                if first == marker && run >= width && trimmed[run..].trim().is_empty() {
                    fence = None;
                }
            } else if run >= 3 {
                fence = Some((first, run));
                let language = trimmed[run..]
                    .trim()
                    .trim_start_matches(['{', '.'])
                    .split(|c: char| c.is_whitespace() || matches!(c, ',' | '}'))
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !language.is_empty() && !NON_SOURCE_FENCES.contains(&language.as_str()) {
                    code = Some((number, language));
                    break;
                }
            }
        }
        if let Some((code_line, language)) = code {
            total += 1;
            if items.len() < UNCITED_CODE_LIMIT {
                items.push(json!({"line":heading.line,"heading":heading.heading,
                    "code_line":code_line,"language":language}));
            }
        }
    }
    (total > 0).then(|| json!({"flagged":total,"items":items}))
}

/// Whether the line holds `[^label]` for one of `labels`. A label holds no
/// `]`, so each `[^` names the label up to the first `]` after it. Testing
/// every label against every line took 12 s for 16000 cited notes.
fn cites_note(line: &str, labels: &HashSet<&str>) -> bool {
    let mut close = None;
    line.match_indices("[^").any(|(at, _)| {
        let from = at + 2;
        if close.is_none_or(|close| close < from) {
            close = line[from..].find(']').map(|end| from + end);
        }
        close.is_some_and(|close| labels.contains(&line[from..close]))
    })
}

struct CitationFence {
    fence: CodeFence,
    mermaid: bool,
    start_line: usize,
}

/// An open fenced code block: its marker and width, and the list item's
/// content indent when the fence began right after a list marker.
struct CodeFence {
    marker: char,
    width: usize,
    list_content_indent: Option<usize>,
}

/// How a line inside an open fence ends it.
enum FenceEnd {
    /// The line is the closing fence.
    Closer,
    /// The line is outdented below the list item the fence opened in, so
    /// the item and its fence end without a closer; the line stands outside.
    ListItemEnd,
}

impl CodeFence {
    /// The fence a line opens and its info string: a fence at most three
    /// spaces in, or one right after a list marker.
    fn opened_by(line: &str) -> Option<(CodeFence, &str)> {
        let trimmed = line.trim_start_matches(' ');
        (line.len() - trimmed.len() <= 3)
            .then(|| fence_opener(trimmed).map(|run| (run, None)))
            .flatten()
            .or_else(|| {
                list_fence_content(line).and_then(|(content_indent, content)| {
                    fence_opener(content).map(|run| (run, Some(content_indent)))
                })
            })
            .map(|((marker, width, info), list_content_indent)| {
                let fence = CodeFence {
                    marker,
                    width,
                    list_content_indent,
                };
                (fence, info)
            })
    }

    fn ended_by(&self, line: &str) -> Option<FenceEnd> {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let base_indent = self.list_content_indent.unwrap_or(0);
        // An outdented sibling item or heading ends a list item even if
        // its code fence has no explicit closer.
        if self.list_content_indent.is_some() && !line.trim().is_empty() && indent < base_indent {
            Some(FenceEnd::ListItemEnd)
        } else if indent >= base_indent
            && indent <= base_indent + 3
            && fence_run(trimmed).is_some_and(|(marker, width, tail)| {
                marker == self.marker && width >= self.width && tail.trim().is_empty()
            })
        {
            Some(FenceEnd::Closer)
        } else {
            None
        }
    }
}

fn fence_run(text: &str) -> Option<(char, usize, &str)> {
    let marker = text.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let width = text.chars().take_while(|ch| *ch == marker).count();
    (width >= 3).then_some((marker, width, &text[width..]))
}

fn fence_opener(text: &str) -> Option<(char, usize, &str)> {
    fence_run(text).filter(|(marker, _, info)| *marker != '`' || !info.contains('`'))
}

// A fence can begin immediately after a list marker, as in `1. ```chart`.
// Its continuation lines are indented by the list item's content width.
fn list_fence_content(line: &str) -> Option<(usize, &str)> {
    let indent = line.bytes().take_while(|byte| *byte == b' ').count();
    if indent > 3 {
        return None;
    }
    let content = &line[indent..];
    let marker_width = match content.as_bytes().first()? {
        b'-' | b'+' | b'*' => 1,
        byte if byte.is_ascii_digit() => {
            let digits = content
                .bytes()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if !(1..=9).contains(&digits)
                || !matches!(content.as_bytes().get(digits), Some(b'.' | b')'))
            {
                return None;
            }
            digits + 1
        }
        _ => return None,
    };
    let after_marker = &content[marker_width..];
    let spaces = after_marker
        .bytes()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count();
    if !(1..=4).contains(&spaces) {
        return None;
    }
    let content_indent = indent + marker_width + spaces;
    Some((content_indent, &line[content_indent..]))
}

fn unclosed_fence_issue(fence: &CitationFence) -> Value {
    json!({"kind":"unclosed_code_fence","line":fence.start_line,
        "guidance":"Close the fenced code block before the next section or list item. To name a code-block language in prose, use inline code around the language name."})
}

/// A machine-readable citation: path, `:` or `#L`, then a line or range.
/// Route-file names count as path text: a `+` after a slash (SvelteKit
/// `routes/+page.svelte`), a bracketed segment (Next.js `[id]`,
/// `[...slug]`) and a parenthesised directory (`(auth)/`). Without them the
/// match began inside the path and reported a missing file the model could
/// not cite any other way.
const CITATION_PATTERN: &str = r"((?:[\p{L}\p{N}_./@%-]|/\+|\[\[?[\p{L}\p{N}_.@-]+\]\]?|\([\p{L}\p{N}_.@-]+\)/)+\.[A-Za-z][A-Za-z0-9]*)(:|#L)([0-9]+)(?:[-–]L?([0-9]+))?";

/// Whether a citation match is the label of a Markdown link whose target is
/// itself a citation, `…](src/agent.rs:10-20)`: the target is the citation
/// and the label only shows it. Label text may go on after the match, but
/// not across another bracket; a URL or plain-path target keeps the label
/// as the citation.
fn link_label_of_citation(
    line: &str,
    marks: &LineMarks,
    from: usize,
    pattern: &regex::Regex,
) -> bool {
    // The label ends at the first bracket after the citation, which must
    // open the link target.
    let Some(close) = marks
        .bracket_from(from)
        .filter(|&at| line[at..].starts_with("]("))
    else {
        return false;
    };
    let target = &line[close + 2..];
    let Some(end) = target.find(')') else {
        return false;
    };
    let target = target[..end].trim();
    !target.contains("://")
        && pattern
            .find(target)
            .is_some_and(|m| m.start() == 0 && m.end() == target.len())
}

/// Whether the text before a citation match makes it part of a URL. The
/// match itself may start with the `//` of the scheme, since `/` is a path
/// character: `https://example.com/agent.rs#L25` matches from `//`.
/// Name endings of network hosts: the host of `api.example.com:443` or
/// `redis.internal:6379` matched the citation pattern, and its port was
/// reported as a line of a missing file, which held back the final answer.
/// Some also end file names (Dockerfile.dev, .env.example), so a match is a
/// host only when the project has no such file.
const HOST_SUFFIXES: &[&str] = &[
    "com",
    "net",
    "org",
    "io",
    "dev",
    "local",
    "internal",
    "intranet",
    "corp",
    "lan",
    "svc",
    "cluster",
    "cloud",
    "example",
    "invalid",
    "localdomain",
    "edu",
    "gov",
    "co",
    "ai",
    "kr",
    "jp",
    "cn",
    "uk",
    "de",
    "fr",
    "eu",
    "us",
];

/// Whether a `name.suffix:number` match has the shape of a host and port:
/// no folder, and a name ending hosts use. Callers check the project has no
/// file of that name.
fn network_address(path: &str, separator: &str) -> bool {
    separator == ":"
        && !path.contains(['/', '\\', '%'])
        && path.rsplit_once('.').is_some_and(|(name, suffix)| {
            !name.is_empty() && HOST_SUFFIXES.contains(&suffix.to_ascii_lowercase().as_str())
        })
}

/// Whether a line holds a citation, as the citation checks read one: not
/// a URL and not a host with its port (`file_exists` tells a file apart).
fn line_cites(pattern: &regex::Regex, line: &str, file_exists: &dyn Fn(&str) -> bool) -> bool {
    let mut marks = None;
    pattern.captures_iter(line).any(|c| {
        let whole = c.get(0).unwrap();
        !marks
            .get_or_insert_with(|| LineMarks::new(line))
            .url_prefix(line, whole.start(), whole.as_str())
            && (!network_address(&c[1], &c[2]) || file_exists(&c[1]))
    })
}

/// A cited path as written, with %XX escapes decoded: a path holding a
/// space is cited as `docs/my%20notes.md:3`, since a space ends a citation.
fn decode_citation_path(path: &str) -> String {
    if !path.contains('%') {
        return path.to_owned();
    }
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escape = (bytes[i] == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        if let Some(byte) = escape {
            decoded.push(byte);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| path.to_owned())
}

/// The project file a cited path ends when the words written before it
/// join it with spaces (`src/my file.rs:3` cites `file.rs`), as written.
fn spaced_path(s: &Session, before: &str, cited: &str) -> Option<String> {
    if !before.ends_with(' ') {
        return None;
    }
    // Only the last words are joined: splitting the whole text before each
    // missing citation was quadratic on a long line.
    let mut words: Vec<&str> = before.rsplitn(6, ' ').collect();
    words.reverse();
    // words ends with the empty word after the final space.
    (2..=words.len().min(5)).find_map(|count| {
        let joined = words[words.len() - count..].join(" ");
        let joined = joined.trim_start_matches(|ch: char| "([`\"'*_".contains(ch));
        if joined.trim().is_empty() || joined.starts_with(' ') {
            return None;
        }
        let candidate = format!("{joined}{cited}");
        super::read_path(&s.project, &candidate)
            .is_ok_and(|path| path.is_file())
            .then_some(candidate)
    })
}

/// A project path as a citation writes it: spaces (and a literal %) escaped.
fn encode_citation_path(path: &str) -> String {
    path.replace('%', "%25").replace(' ', "%20")
}

/// Where a line's word separators, link brackets and `://` are, found once
/// per line. Each citation looked back to the previous separator and ahead
/// to the next bracket: a 256 KB line with 20,000 citations took 2.8 s to
/// audit, 16 s with no space between them.
struct LineMarks {
    /// Byte offset just past each word separator.
    words: Vec<usize>,
    /// Byte offset of each `://`.
    schemes: Vec<usize>,
    /// Byte offset of each `[` and `]`.
    brackets: Vec<usize>,
}

impl LineMarks {
    fn new(line: &str) -> Self {
        let mut marks = Self {
            words: Vec::new(),
            schemes: line.match_indices("://").map(|(at, _)| at).collect(),
            brackets: Vec::new(),
        };
        for (at, ch) in line.char_indices() {
            if ch.is_whitespace() || ['`', '(', '"'].contains(&ch) {
                marks.words.push(at + ch.len_utf8());
            }
            if matches!(ch, '[' | ']') {
                marks.brackets.push(at);
            }
        }
        marks
    }

    /// Whether the text of `line` before `start` makes the citation
    /// matched there part of a URL: its word holds a `://`, or ends in `:`
    /// right before a match starting with the `//` of a scheme.
    fn url_prefix(&self, line: &str, start: usize, matched: &str) -> bool {
        let separators = self.words.partition_point(|&end| end <= start);
        let head = separators.checked_sub(1).map_or(0, |last| self.words[last]);
        let schemes = self.schemes.partition_point(|&at| at < head);
        self.schemes
            .get(schemes)
            .is_some_and(|&at| at + "://".len() <= start)
            || (line[head..start].ends_with(':') && matched.starts_with("//"))
    }

    /// The first `[` or `]` at or after `from`.
    fn bracket_from(&self, from: usize) -> Option<usize> {
        let index = self.brackets.partition_point(|&at| at < from);
        self.brackets.get(index).copied()
    }
}

/// Per zero-based `str::lines` line, whether a CommonMark parse puts it in
/// a code block, indented or fenced, inside any list or quote.
fn code_block_lines(doc: &str) -> Vec<bool> {
    // The parser also ends a line at a lone CR, which `str::lines` keeps.
    let normalized;
    let doc = if doc.contains('\r') {
        normalized = doc.replace("\r\n", "\n").replace('\r', " ");
        normalized.as_str()
    } else {
        doc
    };
    let mut code = vec![false; doc.lines().count()];
    let arena = comrak::Arena::new();
    let mut options = comrak::Options::default();
    options.extension.table = true;
    options.extension.footnotes = true;
    let root = comrak::parse_document(&arena, doc, &options);
    for node in root.descendants() {
        let data = node.data.borrow();
        if matches!(data.value, comrak::nodes::NodeValue::CodeBlock(_)) {
            let lines = data.sourcepos.start.line.saturating_sub(1)..data.sourcepos.end.line;
            for line in code.iter_mut().take(lines.end).skip(lines.start) {
                *line = true;
            }
        }
    }
    code
}

/// A document's citations and the issues found scanning them.
pub(super) type CitationScan = (Vec<Citation>, Vec<Value>);

/// Scans kept at most: the latest version's. A scan holds every citation
/// with its line (about 14 MB for a 5.6 MB document citing on most lines),
/// and the checks of a save or an issue count all scan one version.
const SCAN_MEMO_ENTRIES: usize = 1;

/// scan_document_citations of `doc`, kept for the latest versions by hash.
/// One saved version was scanned by every check of its save (citations,
/// test code, mismatches, unread ranges) and again by the run's issue count:
/// seven scans of a 5.6 MB document took 0.6 s per save.
fn scan_citations(doc: &str) -> Result<Arc<CitationScan>> {
    type Memo = std::collections::VecDeque<([u8; 32], Arc<CitationScan>)>;
    static MEMO: std::sync::OnceLock<std::sync::Mutex<Memo>> = std::sync::OnceLock::new();
    let memo = || {
        MEMO.get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    let key: [u8; 32] = Sha256::digest(doc.as_bytes()).into();
    if let Some((_, found)) = memo().iter().find(|(known, _)| *known == key) {
        return Ok(found.clone());
    }
    let scan = Arc::new(scan_document_citations(doc)?);
    let mut memo = memo();
    if !memo.iter().any(|(known, _)| *known == key) {
        if memo.len() >= SCAN_MEMO_ENTRIES {
            memo.pop_front();
        }
        memo.push_back((key, scan.clone()));
    }
    Ok(scan)
}

fn scan_document_citations(doc: &str) -> Result<CitationScan> {
    let pattern = regex::Regex::new(CITATION_PATTERN)?;
    let continuation = regex::Regex::new(r"^\s*,\s*([0-9]+)(?:[-–]L?([0-9]+))?")?;
    let mut spans = vec![];
    let mut issues = vec![];
    let mut fence: Option<CitationFence> = None;
    let mut in_comment = false;
    let mut code_lines: Option<Vec<bool>> = None;
    for (document_line, line) in doc.lines().enumerate() {
        if let Some(open) = &fence {
            match open.fence.ended_by(line) {
                Some(FenceEnd::Closer) => {
                    fence = None;
                    continue;
                }
                Some(FenceEnd::ListItemEnd) => {
                    issues.push(unclosed_fence_issue(open));
                    fence = None;
                }
                None => {}
            }
        }
        if fence.as_ref().is_some_and(|open| !open.mermaid) {
            continue;
        }
        if fence.is_none() {
            if !in_comment && let Some((opened, info)) = CodeFence::opened_by(line) {
                fence = Some(CitationFence {
                    fence: opened,
                    mermaid: info.trim().eq_ignore_ascii_case("mermaid"),
                    start_line: document_line + 1,
                });
                continue;
            }
            // Four spaces in is often list content, a nested `    - item` or
            // a `10.` item's continuation paragraph. Skipped as code, their
            // unread and missing citations passed every check, so only a code
            // block of the parsed document is skipped.
            let indent = line.len() - line.trim_start_matches(' ').len();
            if indent >= 4
                && code_lines
                    .get_or_insert_with(|| code_block_lines(doc))
                    .get(document_line)
                    .is_some_and(|code| *code)
            {
                if in_comment {
                    let _ = visible_without_html_comments(line, &mut in_comment);
                }
                continue;
            }
        }
        let visible = if fence.is_some() {
            line.to_owned()
        } else {
            visible_without_html_comments(line, &mut in_comment)
        };
        // The text the columns of this line's citations index, shared by them.
        let mut line_text: Option<Arc<str>> = None;
        let mut marks = None;
        for c in pattern.captures_iter(&visible) {
            let whole = c.get(0).unwrap();
            let marks = marks.get_or_insert_with(|| LineMarks::new(&visible));
            if marks.url_prefix(&visible, whole.start(), whole.as_str()) {
                continue;
            }
            // A Markdown link whose label and target are both citations,
            // `[main.rs:55-106](src/main.rs:55-106)`: the label is display
            // text. Checked as a citation it reported a missing main.rs on
            // every such link, and a live run spent twelve requests rewriting
            // labels whose targets already resolved.
            if link_label_of_citation(&visible, marks, whole.end(), &pattern) {
                continue;
            }
            let begin = c[3].parse().unwrap_or(0);
            let end = c
                .get(4)
                .map(|v| v.as_str().parse().unwrap_or(0))
                .unwrap_or(begin);
            spans.push(Citation {
                raw: c[0].into(),
                path: decode_citation_path(&c[1]),
                begin,
                end,
                relative_link: &c[2] == "#L",
                document_line: document_line + 1,
                columns: (whole.start(), whole.end()),
                line_text: line_text
                    .get_or_insert_with(|| Arc::from(visible.as_str()))
                    .clone(),
                prose: fence.is_none(),
            });
            // Repeat the path internally for grouped citations such as a.js:3, 8-10.
            let mut tail_start = whole.end();
            let mut tail = &visible[tail_start..];
            while let Some(extra) = continuation.captures(tail) {
                let begin = extra[1].parse().unwrap_or(0);
                let end = extra
                    .get(2)
                    .map(|m| m.as_str().parse().unwrap_or(0))
                    .unwrap_or(begin);
                let matched = extra.get(0).unwrap();
                spans.push(Citation {
                    raw: format!("{}{}{}-{}", &c[1], &c[2], begin, end),
                    path: decode_citation_path(&c[1]),
                    begin,
                    end,
                    relative_link: &c[2] == "#L",
                    document_line: document_line + 1,
                    columns: (tail_start + matched.start(), tail_start + matched.end()),
                    line_text: line_text
                        .get_or_insert_with(|| Arc::from(visible.as_str()))
                        .clone(),
                    prose: fence.is_none(),
                });
                tail_start += matched.end();
                tail = &tail[matched.end()..];
            }
        }
    }
    if let Some(open) = fence {
        issues.push(unclosed_fence_issue(&open));
    }
    Ok((spans, issues))
}

/// The citations of `doc`, shared with the scan that found them.
pub(super) struct CitationSpans(Arc<CitationScan>);

impl std::ops::Deref for CitationSpans {
    type Target = [Citation];
    fn deref(&self) -> &[Citation] {
        &self.0.0
    }
}

pub(super) fn citation_spans(doc: &str) -> Result<CitationSpans> {
    Ok(CitationSpans(scan_citations(doc)?))
}

const TEST_CODE_FLAG_LIMIT: usize = 12;
const TEST_CODE_GUIDANCE: &str = "These citations point into test code. A test shows how a behavior is checked, not how the product behaves: cite the product code that implements the claim, or say in the text that the passage describes a test.";

/// Citations into test code: files a test layout names (a tests, test or
/// __tests__ directory; .test., .spec., _test, _spec, test_, tests or ...Test
/// names) and Rust `#[cfg(test)]` modules. A live document cited a test's
/// loop as how the product repeats a step. Advice only; None when nothing is
/// flagged.
pub(super) fn test_code_check(s: &Session, output: &Path, doc: &str) -> Option<Value> {
    let root = s.project.root.canonicalize().ok()?;
    let citations = citation_spans(doc).ok()?;
    let mut modules = HashMap::<PathBuf, Vec<(usize, usize)>>::new();
    let mut paths = PathResolver::new(&s.project);
    let mut items = Vec::new();
    let mut total = 0usize;
    for citation in citations.iter() {
        let path = if citation.relative_link {
            output
                .parent()
                .unwrap()
                .join(&citation.path)
                .to_string_lossy()
                .into_owned()
        } else {
            citation.path.clone()
        };
        let Some(resolved) = paths.lookup(&path) else {
            continue;
        };
        let Ok(relative) = resolved.strip_prefix(&root) else {
            continue;
        };
        let mut in_test_module = || {
            resolved
                .extension()
                .is_some_and(|extension| extension == "rs")
                && modules
                    .entry(resolved.clone())
                    .or_insert_with(|| {
                        read_text(&resolved)
                            .map(|text| rust_test_modules(&text))
                            .unwrap_or_default()
                    })
                    .iter()
                    .any(|(start, end)| citation.begin <= *end && citation.end >= *start)
        };
        if test_file(relative) || in_test_module() {
            total += 1;
            if items.len() < TEST_CODE_FLAG_LIMIT {
                let range = if citation.begin == citation.end {
                    citation.begin.to_string()
                } else {
                    format!("{}-{}", citation.begin, citation.end)
                };
                items.push(json!({"line":citation.document_line,"citation":format!("{}:{range}", citation.path)}));
            }
        }
    }
    (total > 0).then(|| json!({"flagged":total,"items":items,"guidance":TEST_CODE_GUIDANCE}))
}

/// A file whose directory or name marks it as tests.
pub(super) fn test_file(relative: &Path) -> bool {
    let parts: Vec<String> = relative
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect();
    let Some((name, directories)) = parts.split_last() else {
        return false;
    };
    if directories.iter().any(|directory| {
        matches!(
            directory.to_lowercase().as_str(),
            "tests" | "test" | "__tests__"
        )
    }) {
        return true;
    }
    let lower = name.to_lowercase();
    let stem = name.split('.').next().unwrap_or(name);
    let stem_lower = stem.to_lowercase();
    lower.contains(".test.")
        || lower.contains(".spec.")
        || stem_lower.starts_with("test_")
        || matches!(stem_lower.as_str(), "test" | "tests")
        || ["_test", "_tests", "_spec"]
            .iter()
            .any(|suffix| stem_lower.ends_with(suffix))
        || (stem.len() > 5 && (stem.ends_with("Test") || stem.ends_with("Tests")))
}

/// One-based line ranges of Rust `#[cfg(test)]` modules: the attribute, the
/// `mod name {` line after it and everything up to the closing brace at that
/// line's indentation (rustfmt layout), or the end of the file.
pub(super) fn rust_test_modules(text: &str) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let attribute = lines[index].trim();
        if attribute == "#[cfg(test)]" || attribute.starts_with("#[cfg(all(test") {
            let mut next = index + 1;
            while next < lines.len() && {
                let line = lines[next].trim_start();
                line.is_empty() || line.starts_with("#[") || line.starts_with("//")
            } {
                next += 1;
            }
            if let Some(line) = lines.get(next) {
                let trimmed = line.trim_start();
                let declaration = trimmed
                    .trim_start_matches("pub(crate) ")
                    .trim_start_matches("pub(super) ")
                    .trim_start_matches("pub ");
                if declaration.starts_with("mod ") && trimmed.trim_end().ends_with('{') {
                    let close = format!("{}}}", &line[..line.len() - trimmed.len()]);
                    let end = (next + 1..lines.len())
                        .find(|&i| lines[i].trim_end() == close)
                        .unwrap_or(lines.len() - 1);
                    ranges.push((index + 1, end + 1));
                    index = end + 1;
                    continue;
                }
            }
        }
        index += 1;
    }
    ranges
}

fn citation_issues(s: &Session, output: &Path, doc: &str) -> Result<(usize, Vec<Value>)> {
    citation_issues_and_paths(s, output, doc, true).map(|(checked, issues, _)| (checked, issues))
}

/// (document line, cited path, project file it names): the citation_path
/// issues that come with a suggested project path.
type FixablePaths = Vec<(usize, String, String)>;

/// citation_issues, with the fixable citation paths behind their fixes.
/// Without `with_fixes` no ready edit is built: callers that only count
/// issues or complete the paths themselves skipped a cost that grows with
/// the document times its fixable lines.
fn citation_issues_and_paths(
    s: &Session,
    output: &Path,
    doc: &str,
    with_fixes: bool,
) -> Result<(usize, Vec<Value>, FixablePaths)> {
    let scan = scan_citations(doc)?;
    let (spans, mut issues) = (&scan.0, scan.1.clone());
    let mut versions = HashMap::new();
    let mut paths = PathResolver::new(&s.project);
    let mut suggestions = BTreeMap::<String, Option<String>>::new();
    let mut project_files = super::FilesByName::default();
    // The project file each broken #L link target means, looked up once.
    let mut link_files = BTreeMap::<String, Option<String>>::new();
    // (document line, cited path, suggested project path)
    let mut fixable = Vec::new();
    for Citation {
        raw,
        path: cited,
        begin,
        end,
        relative_link,
        document_line,
        columns,
        line_text,
        ..
    } in spans
    {
        let path = if *relative_link {
            output
                .parent()
                .unwrap()
                .join(cited)
                .to_string_lossy()
                .into_owned()
        } else {
            cited.clone()
        };
        let check = versions.entry(path.clone()).or_insert_with(|| {
            paths
                .resolve(&path)
                .and_then(|p| super::text_line_count(&p))
                .map_err(|e| e.to_string())
        });
        match check {
            Ok(lines) if *begin > 0 && end >= begin && end <= lines => {}
            // Name what is wrong with the range: a live closing re-read an
            // 8-line file cited as 64-107 for 12 requests without fixing it.
            Ok(lines) => {
                let error = if *lines == 0 {
                    format!(
                        "{cited} is empty; cite the file that holds the claim, or remove the citation"
                    )
                } else if *begin == 0 {
                    "line numbers start at 1".to_owned()
                } else if end < begin {
                    "the range ends before it starts".to_owned()
                } else {
                    format!(
                        "{cited} has {lines} lines; cite lines within 1-{lines} that hold the claim, or remove the citation"
                    )
                };
                issues.push(json!({"kind":"citation_range","citation":raw,"line":document_line,"error":error}))
            }
            // A host and port, not a file the project lacks.
            Err(error)
                if !*relative_link
                    && error.starts_with("file_not_found")
                    && network_address(cited, ":") => {}
            Err(error) => {
                let guidance = unreadable_citation_guidance(error);
                // A path missing only its folder (agent.rs for
                // src/agent.rs) names one file; a live run left 17 such
                // citations unfixed because the issue gave neither the
                // document line nor the corrected path.
                let suggested = (!*relative_link && error.starts_with("file_not_found"))
                    .then(|| {
                        suggestions
                            .entry(cited.clone())
                            .or_insert_with(|| {
                                super::file_ending_with(s, cited, &mut project_files)
                            })
                            .clone()
                    })
                    .flatten();
                let error = if *relative_link {
                    let file = link_files
                        .entry(cited.clone())
                        .or_insert_with(|| project_file_for_link(s, cited));
                    link_target_error(output, cited, file.as_deref(), *begin, *end, error)
                } else {
                    error.clone()
                };
                let missing_file = !*relative_link && error.starts_with("file_not_found");
                let mut issue = json!({"kind":"citation_path","citation":raw,"line":document_line,"error":error});
                // The citation's line range as written, after its path.
                let range = &raw[raw.find([':', '#']).unwrap_or(raw.len())..];
                if let Some(guidance) = guidance {
                    issue["guidance"] = json!(guidance);
                } else if missing_file
                    && suggested.is_none()
                    && let Some(spaced) = spaced_path(s, &line_text[..columns.0], cited)
                {
                    issue["guidance"] = json!(format!(
                        "{cited} is the end of the project file {spaced:?}: a citation ends at a space, so write each space of its path as %20: {}{range}",
                        encode_citation_path(&spaced)
                    ));
                } else if missing_file && suggested.is_none() {
                    issue["guidance"] = json!(FORMAT_EXAMPLE_GUIDANCE);
                }
                if let Some(file) = suggested {
                    let file = encode_citation_path(&file);
                    issue["suggested"] = json!(format!("{file}{range}"));
                    fixable.push((*document_line, cited.clone(), file));
                }
                issues.push(issue);
            }
        }
    }
    if !with_fixes {
        return Ok((spans.len(), issues, fixable));
    }
    // One fix per line: the first citation_path issue on that line carries
    // the edit that rewrites every fixable citation on it.
    let mut fixes = citation_fixes(doc, &fixable);
    let mut fixed_lines = BTreeSet::new();
    for issue in &mut issues {
        if issue["kind"] != "citation_path" || issue.get("suggested").is_none() {
            continue;
        }
        let Some(line) = issue["line"].as_u64() else {
            continue;
        };
        if let Some(fix) = fixes.remove(&(line as usize)) {
            issue["fix"] = fix;
            fixed_lines.insert(line);
        } else if fixed_lines.contains(&line) {
            // Not a second edit: it would no longer match after the first.
            issue["covered_by_line_fix"] = json!(true);
        }
    }
    Ok((spans.len(), issues, fixable))
}

/// Widening steps tried before a fix falls back to its whole line.
const FIX_WIDEN_STEPS: usize = 40;

/// Lines tried for a ready fix, as many as one document_edit_batch applies.
/// Each try scans and rewrites the whole document: an audit of a 1.3MB
/// document with 6000 folder-less citations took 40 seconds. Later issues
/// keep their suggested path, and the next save completes every one.
const FIX_LINES_LIMIT: usize = 32;

/// Ready replace_text edits, one per document line, that rewrite the line's
/// unresolved citation paths to their suggested project paths. old_text is
/// the citation span widened one character on each side until it occurs
/// once in the document, so a short citation inside a longer one
/// (agent.rs:25 in src/agent.rs:2563) is not matched; a live model failed
/// nine batches choosing such excerpts. Each fix is applied in document
/// order before it is kept, so the fixes apply together in one
/// document_edit_batch.
fn citation_fixes(doc: &str, fixable: &[(usize, String, String)]) -> BTreeMap<usize, Value> {
    let Ok(pattern) = regex::Regex::new(CITATION_PATTERN) else {
        return BTreeMap::new();
    };
    let mut by_line = BTreeMap::<usize, BTreeMap<&str, &str>>::new();
    for (line, cited, file) in fixable {
        by_line
            .entry(*line)
            .or_default()
            .insert(cited.as_str(), file.as_str());
    }
    let lines: Vec<&str> = doc.lines().collect();
    let mut current = doc.to_owned();
    let mut fixes = BTreeMap::new();
    for (line_number, paths) in by_line.into_iter().take(FIX_LINES_LIMIT) {
        let Some(line) = line_number
            .checked_sub(1)
            .and_then(|index| lines.get(index))
        else {
            continue;
        };
        let (rewrites, span) = path_rewrites(&pattern, line, &paths);
        let Some((mut start, mut end)) = span else {
            continue;
        };
        let mut steps = 0;
        while current.matches(&line[start..end]).take(2).count() != 1 {
            if steps == FIX_WIDEN_STEPS || (start == 0 && end == line.len()) {
                (start, end) = (0, line.len());
                break;
            }
            start -= line[..start].chars().next_back().map_or(0, char::len_utf8);
            end += line[end..].chars().next().map_or(0, char::len_utf8);
            steps += 1;
        }
        let text = rewritten(line, start, end, &rewrites);
        let fix = json!({"action":"replace_text","old_text":&line[start..end],"text":text});
        if let Ok(next) = super::apply_document_edit_operation(&current, &fix) {
            current = next;
            fixes.insert(line_number, fix);
        }
    }
    fixes
}

/// The document with each citation path that names exactly one project
/// file by its end (App.jsx:565 for frontend/src/App.jsx) rewritten to that
/// file's project path, and the citations completed ({line, cited,
/// saved_as}); None when there is none. A save writes these, so such a
/// citation needs no repair request: a live run spent 24 requests rewriting
/// 25 of them, eight listed per save, and was closed as stalled. Lines are
/// rewritten in place, so an identical line elsewhere (another live
/// document repeated one inside an unclosed code block) does not stop it,
/// as it stops a replace_text fix.
pub(super) fn complete_citation_paths(
    s: &Session,
    output: &Path,
    doc: &str,
) -> Option<(String, Vec<Value>)> {
    let (_, issues, fixable) = citation_issues_and_paths(s, output, doc, false).ok()?;
    let pattern = regex::Regex::new(CITATION_PATTERN).ok()?;
    let mut by_line = BTreeMap::<usize, BTreeMap<&str, &str>>::new();
    for (line, cited, file) in &fixable {
        by_line
            .entry(*line)
            .or_default()
            .insert(cited.as_str(), file.as_str());
    }
    let mut completed_lines = BTreeSet::new();
    let mut current = String::with_capacity(doc.len());
    for (index, piece) in doc.split_inclusive('\n').enumerate() {
        let line = piece.trim_end_matches(['\n', '\r']);
        let (rewrites, span) = by_line
            .get(&(index + 1))
            .map(|paths| path_rewrites(&pattern, line, paths))
            .unwrap_or_default();
        if span.is_some() {
            current.push_str(&rewritten(line, 0, line.len(), &rewrites));
            current.push_str(&piece[line.len()..]);
            completed_lines.insert(index as u64 + 1);
        } else {
            current.push_str(piece);
        }
    }
    if completed_lines.is_empty() {
        return None;
    }
    let completed = issues
        .iter()
        .filter(|issue| {
            issue.get("suggested").is_some()
                && issue["line"]
                    .as_u64()
                    .is_some_and(|line| completed_lines.contains(&line))
        })
        .map(|issue| {
            json!({"line":issue["line"],"cited":issue["citation"],"saved_as":issue["suggested"]})
        })
        .collect();
    Some((current, completed))
}

/// A cited path's byte range in its line and the project file to write.
type PathRewrite<'a> = (usize, usize, &'a str);

/// The paths to rewrite in one document line, as (path start, path end,
/// project file), for each citation whose path `paths` maps and that is not
/// part of a URL; and the span from the first such citation to the end of
/// the last.
fn path_rewrites<'a>(
    pattern: &regex::Regex,
    line: &str,
    paths: &BTreeMap<&str, &'a str>,
) -> (Vec<PathRewrite<'a>>, Option<(usize, usize)>) {
    let mut rewrites = Vec::new();
    let mut span: Option<(usize, usize)> = None;
    let mut marks = None;
    for c in pattern.captures_iter(line) {
        let whole = c.get(0).unwrap();
        let path = c.get(1).unwrap();
        let Some(file) = paths.get(decode_citation_path(path.as_str()).as_str()) else {
            continue;
        };
        if marks
            .get_or_insert_with(|| LineMarks::new(line))
            .url_prefix(line, whole.start(), whole.as_str())
        {
            continue;
        }
        // A match inside a longer path, agent.rs in src\agent.rs:1-2, does
        // not name the file it ends with: completing it saved
        // src\src/agent.rs.
        if line[..whole.start()]
            .chars()
            .next_back()
            .is_some_and(continues_path)
        {
            continue;
        }
        rewrites.push((path.start(), path.end(), *file));
        span = Some(span.map_or((whole.start(), whole.end()), |(start, end)| {
            (start.min(whole.start()), end.max(whole.end()))
        }));
    }
    (rewrites, span)
}

/// Whether a character before a citation match makes the match the end of
/// a longer path.
fn continues_path(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '\\' | '_' | '.' | '/' | '@' | '-' | '+')
}

/// line[start..end] with each rewrite's path replaced by its project file.
fn rewritten(line: &str, start: usize, end: usize, rewrites: &[PathRewrite]) -> String {
    let mut text = String::new();
    let mut at = start;
    for (path_start, path_end, file) in rewrites {
        text.push_str(&line[at..*path_start]);
        text.push_str(file);
        at = *path_end;
    }
    text.push_str(&line[at..end]);
    text
}

/// A cited file the tools refuse to read fails every audit until the
/// citation goes. The bare read error gave a model nothing to act on.
/// A missing cited file with no project file to suggest may be no citation
/// at all: a live document explaining the citation format wrote the example
/// `file.rs:10, 20-30`, then re-read that line eleven times and finally
/// turned the example into a citation of unrelated lines to pass the check.
const FORMAT_EXAMPLE_GUIDANCE: &str = "No project file has this name. If this is an example of the citation format rather than a citation, write it so it is not path:line (for example <path>:<start>-<end>) or move it into a fenced code block, which the check skips; do not turn an example into a citation. Otherwise cite the project file that holds the claim as path:start-end.";

fn unreadable_citation_guidance(error: &str) -> Option<&'static str> {
    if error.starts_with("unsupported_large_file") {
        Some(
            "This file is over the 16 MiB read limit, so no tool can read, search or verify it and the audit keeps failing: remove this citation and support the claim with a readable source file, or state in the text that the file could not be inspected.",
        )
    } else if error.starts_with("unsupported_binary_file")
        || error.starts_with("unsupported_non_utf8_file")
    {
        Some(
            "This file is not UTF-8 text, so no tool can read or verify it and the audit keeps failing: remove this citation and cite a readable text source, or state in the text that the file could not be inspected.",
        )
    } else {
        None
    }
}

/// A `path#Lx-Ly` link target resolves from the output document's folder,
/// as Markdown links do, but read_path's error says relative paths use
/// project.root. A live run copied a docs/ page's `../src/...#L` links into
/// an output outside the project, rewrote one target six ways in 19
/// requests, and recorded the check itself as broken. Name the folder, where
/// the target landed, and the project-relative citation to write instead.
fn link_target_error(
    output: &Path,
    target: &str,
    file: Option<&str>,
    begin: usize,
    end: usize,
    error: &str,
) -> String {
    let code = error.split(':').next().unwrap_or("file_access_error");
    let folder = output.parent().unwrap_or(output);
    let mut resolved = PathBuf::new();
    for part in folder.join(target).components() {
        match part {
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::CurDir => {}
            part => resolved.push(part),
        }
    }
    let range = if end > begin {
        format!("{begin}-{end}")
    } else {
        begin.to_string()
    };
    let instead = match file {
        Some(relative) => format!("cite it as {relative}:{range}"),
        None => "cite the project file as path:start-end relative to project.root".to_owned(),
    };
    format!(
        "{code}: a #L link target resolves from the output document's folder {}, not from project.root, so {target} points to {}, which is not a readable project file. {instead} (as text, or as the link text) instead of a #L link target",
        folder.display(),
        resolved.display()
    )
}

/// The project file a link target was most likely meant to name: the target
/// without its leading `./` and `../` steps, read from project.root, or the
/// part after the project root when the target spells out the root's path.
fn project_file_for_link(s: &Session, target: &str) -> Option<String> {
    let rest: PathBuf = Path::new(target)
        .components()
        .skip_while(|part| {
            matches!(
                part,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
        .collect();
    let root = s.project.root.canonicalize().ok()?;
    let below_root = Path::new("/")
        .join(&rest)
        .strip_prefix(&root)
        .ok()
        .map(Path::to_path_buf);
    [Some(rest), below_root]
        .into_iter()
        .flatten()
        .filter(|candidate| !candidate.as_os_str().is_empty())
        .find_map(|candidate| {
            let found = read_path(&s.project, &candidate.to_string_lossy()).ok()?;
            let relative = found.strip_prefix(&root).ok()?;
            (found.is_file() && !relative.as_os_str().is_empty())
                .then(|| relative.to_string_lossy().into_owned())
        })
}

/// A source document without a machine-readable citation cannot be
/// finished: say so on every save and audit, not only when the final answer
/// is refused.
const NO_CITATIONS_GUIDANCE: &str = "The document cites no project source as relative/path.ext:start-end. Cite the sources of its claims next to them (for example backend/src/server.js:12-30); the final answer is refused while the document cites no project source (closing mode accepts a second final answer and reports it). When memory_read is offered, it returns the path and lines of each source saved with a memory.";

pub(super) fn citation_check(s: &Session, output: &Path, doc: &str) -> Result<Value> {
    let (checked, issues) = citation_issues(s, output, doc)?;
    // The write already succeeded: a failed unread check is reported beside
    // it, not as a failure of the save.
    let (unread, unread_error) = match unread_citations(s, doc) {
        Ok(unread) => (unread, None),
        Err(error) => (vec![], Some(error.to_string())),
    };
    let listed: Vec<_> = issues.iter().take(8).collect();
    let mut check = json!({"citations_checked":checked,"issue_count":issues.len(),"issues":listed,
        "unread_citation_count":unread.len(),"unread_citations":unread.iter().take(8).collect::<Vec<_>>()});
    if let Some(error) = unread_error {
        check["unread_citation_error"] = json!(error);
    }
    // Every save result stays in context: a clean save carries no guidance,
    // and the unread-citation rule is the prompt's. Only a ready path fix
    // needs saying how to send it.
    if listed.iter().any(|issue| issue.get("fix").is_some()) {
        check["guidance"] = json!(
            "Send the fixes of the listed citation_path issues together in one document_edit_batch; an issue marked covered_by_line_fix needs no edit of its own."
        );
    }
    if checked == 0 {
        check["citations_required"] = json!(NO_CITATIONS_GUIDANCE);
    }
    Ok(check)
}

#[cfg(test)]
mod outline_tests {
    use super::*;

    /// "# A" on line 1; section i has "## S{i}" on line 3+8i and
    /// "### S{i}.{j}" on line 5+8i+2j, each followed by a body line.
    fn nested() -> String {
        let mut doc = String::from("# A\nbody\n");
        for i in 0..10 {
            doc.push_str(&format!("## S{i}\nbody\n"));
            for j in 0..3 {
                doc.push_str(&format!("### S{i}.{j}\nbody\n"));
            }
        }
        doc
    }

    fn titles(doc: &str, view: OutlineView) -> (Vec<String>, usize) {
        let headings = headings(doc);
        let outline = outline(&headings, view);
        let titles = outline
            .entries(&headings, None)
            .iter()
            .map(|entry| entry["heading"].as_str().unwrap().to_owned())
            .collect();
        (titles, outline.omitted())
    }

    #[test]
    fn a_page_is_a_slice_of_the_complete_outline() {
        let doc = nested();
        let (page, omitted) = titles(
            &doc,
            OutlineView::Page {
                offset: 40,
                limit: 5,
            },
        );
        assert_eq!(page, ["### S9.2"]);
        assert_eq!(omitted, 40);
        let (end, _) = titles(
            &doc,
            OutlineView::Page {
                offset: 41,
                limit: 5,
            },
        );
        assert!(end.is_empty());
    }

    #[test]
    fn a_short_outline_is_complete() {
        let (all, omitted) = titles(
            &nested(),
            OutlineView::Around {
                lines: vec![(47, 47)],
                max: 41,
            },
        );
        assert_eq!(all.len(), 41);
        assert_eq!(omitted, 0);
    }

    #[test]
    fn a_long_outline_keeps_the_focus_its_ancestors_and_neighbours_then_the_top_levels() {
        let doc = nested();
        // Line 47 is "### S5.1"; line 48 is its body, which focuses the
        // section that contains it.
        for lines in [(47, 47), (48, 48)] {
            let (near, omitted) = titles(
                &doc,
                OutlineView::Around {
                    lines: vec![lines],
                    max: 12,
                },
            );
            assert_eq!(
                near,
                [
                    "# A", "## S0", "## S1", "### S4.1", "### S4.2", "## S5", "### S5.0",
                    "### S5.1", "### S5.2", "## S6", "### S6.0", "### S6.1"
                ],
                "{lines:?}"
            );
            assert_eq!(omitted, 29);
        }
    }

    #[test]
    fn a_focus_over_most_of_the_outline_leaves_the_levels_alone() {
        let (levels, omitted) = titles(
            &nested(),
            OutlineView::Around {
                lines: vec![(1, 82)],
                max: 12,
            },
        );
        assert_eq!(levels.len(), 12, "{levels:?}");
        assert!((0..10).all(|i| levels.contains(&format!("## S{i}"))));
        assert!(levels.contains(&"### S0.0".to_owned()));
        assert!(!levels.contains(&"### S0.1".to_owned()));
        assert_eq!(omitted, 29);
    }

    #[test]
    fn several_ranges_each_keep_their_neighbourhood() {
        // "### S1.0" is on line 13 and "### S8.2" on line 73.
        let (near, omitted) = titles(
            &nested(),
            OutlineView::Around {
                lines: vec![(13, 13), (73, 73)],
                max: 20,
            },
        );
        assert_eq!(near.len(), 20, "{near:?}");
        for title in [
            "### S1.0", "## S1", "### S8.2", "## S8", "### S8.1", "### S9.0",
        ] {
            assert!(near.contains(&title.to_owned()), "{title} in {near:?}");
        }
        assert_eq!(omitted, 21);
    }
}

#[cfg(test)]
mod definition_tests {
    use super::{
        DEFINITION_OCCURRENCES, HashSet, cites_note, code_names, defines, definition_pattern,
    };

    /// The pattern defines checks form by form: one regex per name.
    fn reference(name: &str) -> regex::Regex {
        definition_pattern(name).unwrap()
    }

    /// Lines built from definition fragments around the name agree with
    /// the per-name pattern, including overlapping and embedded names.
    #[test]
    fn split_forms_match_the_per_name_pattern() {
        let heads = [
            "",
            " ",
            "  ",
            "\t",
            "fn ",
            "pub fn ",
            "function ",
            "def ",
            "struct ",
            "impl ",
            "mod  ",
            "xfn ",
            "const ",
            "let ",
            "var ",
            "export const ",
            "{",
            ",",
            "a,",
            "(",
            "async ",
            "static async ",
            "public get ",
            "  private ",
            "load",
            "_",
            "é ",
            "ü",
            "const loadx",
            "fn\u{a0}",
            "class\u{3000}",
        ];
        let tails = [
            "",
            " ",
            "()",
            "() {",
            "(a, b) {  ",
            "(a) { x }",
            "(a(b)) {",
            " = 1",
            "= 1",
            "==",
            " =",
            ": T = v",
            ": T == v",
            ":x=y",
            ": function",
            ": functionx",
            ": function(",
            ": (",
            ": async () =>",
            ": async x =>",
            ": x => 1",
            ": $a  =>",
            ": 1 =>",
            "x",
            "_",
            "é",
            "\u{301}",
            "-",
            "::run",
            ".load",
            ": async function",
            "load: () =>",
            "ad() {",
            "\u{a0}= 2",
        ];
        let mut checked = 0;
        for name in ["load", "loadload", "aaaa", "run_x"] {
            let reference = reference(name);
            for head in heads {
                for middle in ["", "load", "x", " "] {
                    for tail in tails {
                        let text = format!("{head}{name}{middle}{tail}");
                        assert_eq!(
                            defines(&text, name, &mut None),
                            reference.is_match(&text),
                            "{text:?} defines {name:?}"
                        );
                        let doubled = format!("{head}{name}{tail}, {head}{name}{tail}");
                        assert_eq!(
                            defines(&doubled, name, &mut None),
                            reference.is_match(&doubled),
                            "{doubled:?} defines {name:?}"
                        );
                        checked += 2;
                    }
                }
            }
        }
        assert!(checked > 10_000);
    }

    /// A line holding the name many times is matched in one pass: checking
    /// each occurrence scanned the rest of the line again.
    #[test]
    fn many_occurrences_in_one_line_stay_linear() {
        let mut whole = None;
        let line = "const load: a, ".repeat(100_000);
        let started = std::time::Instant::now();
        assert!(!defines(&line, "load", &mut whole));
        let defined = format!("{line}const load = 1");
        assert!(defines(&defined, "load", &mut whole));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let few = "const load: a, ".repeat(DEFINITION_OCCURRENCES);
        assert_eq!(
            defines(&few, "load", &mut None),
            reference("load").is_match(&few)
        );
    }

    /// The note lookup agrees with testing each `[^label]` as a substring.
    #[test]
    fn cited_note_lookup_matches_the_substring_test() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as usize
        };
        let pieces = ["[^", "]", "a", "b", "[", "^", " ", "한"];
        let pool = ["a", "b", "ab", "[^a", "a b", "^", "한a"];
        for _ in 0..20_000 {
            let line: String = (0..next(14)).map(|_| pieces[next(pieces.len())]).collect();
            let labels: HashSet<&str> = (0..next(4)).map(|_| pool[next(pool.len())]).collect();
            let expected = labels
                .iter()
                .any(|label| line.contains(&format!("[^{label}]")));
            assert_eq!(cites_note(&line, &labels), expected, "{line:?} {labels:?}");
        }
    }

    #[test]
    fn code_names_keep_first_occurrences_in_order() {
        let (names, qualifiers) = code_names("beta alpha beta gamma::delta alpha gamma::x");
        assert_eq!(names, ["beta", "alpha", "delta"]);
        assert_eq!(qualifiers, ["gamma"]);
    }
}

#[cfg(test)]
mod scan_memo_tests {
    use super::*;

    /// Tool review 2026-10-23: one saved version was scanned for citations
    /// by every check of its save and again by the run's issue count. The
    /// scan is kept per version: the same text gives the scan made for it,
    /// with what scanning it again finds, and changed text a new scan.
    #[test]
    fn a_scanned_version_is_scanned_once() {
        let doc = "# T\n\nSee src/memo_a.rs:1-2, 4 and memo_b.rs#L3.\n\n```\nsrc/x.rs:9\n";
        let first = scan_citations(doc).unwrap();
        let fresh = scan_document_citations(doc).unwrap();
        let raw = |citations: &[Citation]| {
            citations
                .iter()
                .map(|c| {
                    (
                        c.raw.clone(),
                        c.path.clone(),
                        c.begin,
                        c.end,
                        c.document_line,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(raw(&first.0), raw(&fresh.0));
        assert_eq!(first.1, fresh.1);
        assert_eq!(first.0.len(), 3, "{:?}", raw(&first.0));
        assert!(!first.1.is_empty());
        // Other tests share the memo; one pair of calls in a row sees the
        // same scan unless another version came between them.
        assert!(
            (0..10)
                .any(|_| Arc::ptr_eq(&scan_citations(doc).unwrap(), &scan_citations(doc).unwrap()))
        );
        let changed = doc.replace("1-2", "5-6");
        let scan = citation_spans(&changed).unwrap();
        assert_eq!((scan[0].begin, scan[0].end), (5, 6));
    }
}
