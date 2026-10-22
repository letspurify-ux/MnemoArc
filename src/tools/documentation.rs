use super::*;

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
    let mut fence: Option<CodeFence> = None;
    let mut in_comment = false;
    let mut offset = 0;
    for (i, line) in doc.split_inclusive('\n').enumerate() {
        let start = offset;
        offset += line.len();
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
            for prior in result.iter_mut().rev() {
                if prior.end == doc.len() && prior.level >= run {
                    prior.end = start;
                }
            }
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
    let headings = headings(doc);
    let paths = heading_paths(&headings);
    headings
        .iter()
        .position(|heading| heading.start == start)
        .map(|index| paths[index].clone())
        .ok_or_else(|| anyhow::anyhow!("section_not_found: heading position changed"))
}

/// Dice similarity of the character bigrams of two titles, ignoring case,
/// spacing and punctuation, so Korean and English wording rank alike.
fn title_similarity(a: &str, b: &str) -> f64 {
    let bigrams = |text: &str| {
        let chars: Vec<char> = text
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        chars
            .windows(2)
            .map(|pair| (pair[0], pair[1]))
            .collect::<Vec<_>>()
    };
    let (a, mut b) = (bigrams(a), bigrams(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let total = a.len() + b.len();
    let shared = a
        .iter()
        .filter(|pair| {
            b.iter()
                .position(|other| other == *pair)
                .map(|index| b.swap_remove(index))
                .is_some()
        })
        .count();
    (2 * shared) as f64 / total as f64
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
                let target = bare_heading_title(requested_path.lines().last().unwrap_or(requested));
                let mut indices: Vec<_> = (0..headings.len()).collect();
                if indices.len() > SHOWN {
                    let score: Vec<_> = headings
                        .iter()
                        .map(|h| title_similarity(target, bare_heading_title(&h.heading)))
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
                (Some(path), _) => read_path(&s.project, path)?,
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
            let (checked, issues, format_check) = audit_issues(s, &path, &doc)?;
            // The issue list includes delivered-evidence state as well as
            // document citations. Bind every continuation page to the
            // exact list that produced the first page.
            let document_hash = hash(doc.as_bytes());
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
fn audit_issues(s: &Session, path: &Path, doc: &str) -> Result<(usize, Vec<Value>, Value)> {
    let (checked, mut issues) = citation_issues(s, path, doc)?;
    let format = super::document_format::check(doc);
    let format_check = format.summary();
    // The citation scanner already catches many unclosed fences. Keep
    // one diagnostic per fence while adding AST coverage for containers.
    let mut fence_lines: BTreeSet<_> = issues
        .iter()
        .filter(|issue| issue["kind"] == "unclosed_code_fence")
        .filter_map(|issue| issue["line"].as_u64())
        .collect();
    for issue in format.issues {
        if issue["kind"] != "unclosed_code_fence"
            || issue["line"]
                .as_u64()
                .is_none_or(|line| fence_lines.insert(line))
        {
            issues.push(issue);
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
pub(super) fn open_issue_count(s: &Session, path: &Path, doc: &str) -> Result<usize> {
    Ok(audit_issues(s, path, doc)?.1.len())
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
pub(super) fn open_issue_lines(s: &Session, output: &Path, doc: &str, limit: usize) -> Vec<u64> {
    let mut lines = BTreeSet::new();
    if let Ok((_, issues)) = citation_issues(s, output, doc) {
        lines.extend(issues.iter().filter_map(|issue| issue["line"].as_u64()));
    }
    lines.extend(
        super::document_format::check(doc)
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
    /// Byte range of the citation in its document line (with HTML comments
    /// removed), and whether it stands in prose rather than in a diagram.
    pub columns: (usize, usize),
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
    let mut versions = BTreeMap::<PathBuf, Option<String>>::new();
    let mut delivered = BTreeMap::<PathBuf, Vec<(usize, usize)>>::new();
    // Every source of a long session is checked on each request; resolve
    // each file once rather than once per source.
    let mut paths = PathResolver::new(&s.project);
    for source in s.sources.values() {
        if source.origin != "file" || source.evidence_truncated {
            continue;
        }
        let (Some(path), Some(start), Some(end)) =
            (source.path.as_deref(), source.start_line, source.end_line)
        else {
            continue;
        };
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
    for ranges in delivered.values_mut() {
        ranges.sort_unstable();
    }
    let mut missing = BTreeMap::<PathBuf, Vec<(usize, usize)>>::new();
    let mut line_counts = BTreeMap::<PathBuf, usize>::new();
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
    for citation in citation_spans(doc)? {
        if unchanged(citation.document_line) {
            continue;
        }
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
        let Some(cited) = paths.lookup(&path) else {
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
        for &(start, end) in delivered.get(&cited).map(Vec::as_slice).unwrap_or(&[]) {
            if end < next {
                continue;
            }
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
    let root = s.project.root.canonicalize()?;
    let mut issues = vec![];
    for (path, mut gaps) in missing {
        gaps.sort_unstable();
        let mut merged: Vec<(usize, usize)> = vec![];
        for (start, end) in gaps {
            if let Some(last) = merged.last_mut()
                && start <= last.1.saturating_add(1)
            {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
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
            let list = if qualifier {
                &mut qualifiers
            } else {
                &mut names
            };
            if !list.contains(&word) {
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
fn defined_around(source: &[String], name: &str, begin: usize, end: usize) -> bool {
    let name = regex::escape(name);
    let Ok(definition) = regex::Regex::new(&format!(
        r"\b(?:fn|function|def|struct|enum|trait|class|interface|mod|impl)\s+{name}\b|\b(?:const|let|var)\s+{name}\s*(?::[^=]*)?=[^=]|(?:^|[\s,{{]){name}\s*:\s*(?:async\s+)?(?:function\b|\(|[A-Za-z_$][\w$]*\s*=>)|^\s*(?:(?:async|static|get|set|public|private|protected)\s+)*{name}\s*\([^()]*\)\s*\{{\s*$"
    )) else {
        return false;
    };
    let indent = |text: &str| text.len() - text.trim_start().len();
    let last = end.min(source.len());
    source[..begin.saturating_sub(1).min(source.len())]
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, text)| definition.is_match(text))
        .any(|(at, text)| {
            let depth = indent(text);
            source[at + 1..last].iter().all(|line| {
                let trimmed = line.trim_start();
                trimmed.is_empty() || indent(line) > depth || trimmed.starts_with(')')
            })
        })
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
    let lines: Vec<&str> = doc.lines().collect();
    let own = output.canonicalize().ok();
    let mut files = BTreeMap::<PathBuf, Option<Vec<String>>>::new();
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
        let Some(text) = lines.get(line - 1) else {
            continue;
        };
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
            for (from, to) in spans
                .iter()
                .filter(|(from, to)| *from >= previous_end && *to <= start)
            {
                let (span_names, span_qualifiers) = code_names(&text[*from..*to]);
                for (list, found) in [(&mut names, span_names), (&mut qualifiers, span_qualifiers)]
                {
                    for name in found {
                        if !list.contains(&name) {
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
                    .or_insert_with(|| {
                        read_text(&resolved)
                            .ok()
                            .map(|text| text.lines().map(str::to_owned).collect())
                    })
                    .as_ref()
                else {
                    continue;
                };
                if citation.begin == 0
                    || citation.end < citation.begin
                    || citation.end > source.len()
                {
                    continue;
                }
                checked = true;
                let cited_text = &source[citation.begin - 1..citation.end];
                found |= names.iter().chain(&qualifiers).any(|name| {
                    cited_text
                        .iter()
                        .any(|cited_line| cited_line.contains(name.as_str()))
                }) || names
                    .iter()
                    .any(|name| defined_around(source, name, citation.begin, citation.end));
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
    let note = regex::Regex::new(r"^ {0,3}\[\^([^\]]+)\]:").ok()?;
    let lines: Vec<&str> = doc.lines().collect();
    let cited_notes: Vec<String> = lines
        .iter()
        .filter_map(|line| {
            let label = note.captures(line)?;
            let rest = &line[label.get(0)?.end()..];
            pattern.is_match(rest).then(|| format!("[^{}]", &label[1]))
        })
        .collect();
    let cites = |line: &&str| {
        pattern.is_match(line)
            || cited_notes
                .iter()
                .any(|label| line.contains(label.as_str()))
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
    let cited = |index: usize| {
        let lines_of = own(index);
        lines[lines_of.start - 1..lines_of.end - 1]
            .iter()
            .any(cites)
    };
    for (index, heading) in headings.iter().enumerate() {
        let parent = headings[..index]
            .iter()
            .rposition(|prior| prior.level < heading.level)
            .filter(|&parent| headings[parent].level >= 2);
        if unchanged(heading.line) || cited(index) || parent.is_some_and(cited) {
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
const CITATION_PATTERN: &str = r"((?:[\p{L}\p{N}_./@-]|/\+|\[\[?[\p{L}\p{N}_.@-]+\]\]?|\([\p{L}\p{N}_.@-]+\)/)+\.[A-Za-z][A-Za-z0-9]*)(:|#L)([0-9]+)(?:[-–]L?([0-9]+))?";

/// Whether a citation match is the label of a Markdown link whose target is
/// itself a citation, `…](src/agent.rs:10-20)`: the target is the citation
/// and the label only shows it. Label text may go on after the match, but
/// not across another bracket; a URL or plain-path target keeps the label
/// as the citation.
fn link_label_of_citation(line: &str, from: usize, pattern: &regex::Regex) -> bool {
    let rest = &line[from..];
    let Some(close) = rest.find("](") else {
        return false;
    };
    if rest[..close].contains(['[', ']']) {
        return false;
    }
    let target = &rest[close + 2..];
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
fn url_prefix(before: &str, matched: &str) -> bool {
    let head = before
        .rsplit(|ch: char| ch.is_whitespace() || ['`', '(', '"'].contains(&ch))
        .next()
        .unwrap_or("");
    head.contains("://") || (head.ends_with(':') && matched.starts_with("//"))
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

fn scan_citations(doc: &str) -> Result<(Vec<Citation>, Vec<Value>)> {
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
        for c in pattern.captures_iter(&visible) {
            let whole = c.get(0).unwrap();
            if url_prefix(&visible[..whole.start()], whole.as_str()) {
                continue;
            }
            // A Markdown link whose label and target are both citations,
            // `[main.rs:55-106](src/main.rs:55-106)`: the label is display
            // text. Checked as a citation it reported a missing main.rs on
            // every such link, and a live run spent twelve requests rewriting
            // labels whose targets already resolved.
            if link_label_of_citation(&visible, whole.end(), &pattern) {
                continue;
            }
            let begin = c[3].parse().unwrap_or(0);
            let end = c
                .get(4)
                .map(|v| v.as_str().parse().unwrap_or(0))
                .unwrap_or(begin);
            spans.push(Citation {
                raw: c[0].into(),
                path: c[1].into(),
                begin,
                end,
                relative_link: &c[2] == "#L",
                document_line: document_line + 1,
                columns: (whole.start(), whole.end()),
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
                    path: c[1].into(),
                    begin,
                    end,
                    relative_link: &c[2] == "#L",
                    document_line: document_line + 1,
                    columns: (tail_start + matched.start(), tail_start + matched.end()),
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

pub(super) fn citation_spans(doc: &str) -> Result<Vec<Citation>> {
    Ok(scan_citations(doc)?.0)
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
    let mut modules = BTreeMap::<PathBuf, Vec<(usize, usize)>>::new();
    let mut paths = PathResolver::new(&s.project);
    let mut items = Vec::new();
    let mut total = 0usize;
    for citation in citations {
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
    citation_issues_and_paths(s, output, doc).map(|(checked, issues, _)| (checked, issues))
}

/// (document line, cited path, project file it names): the citation_path
/// issues that come with a suggested project path.
type FixablePaths = Vec<(usize, String, String)>;

/// citation_issues, with the fixable citation paths behind their fixes.
fn citation_issues_and_paths(
    s: &Session,
    output: &Path,
    doc: &str,
) -> Result<(usize, Vec<Value>, FixablePaths)> {
    let (spans, mut issues) = scan_citations(doc)?;
    let mut versions = std::collections::BTreeMap::new();
    let mut paths = PathResolver::new(&s.project);
    let mut suggestions = BTreeMap::<String, Option<String>>::new();
    // (document line, cited path, suggested project path)
    let mut fixable = Vec::new();
    for Citation {
        raw,
        path: cited,
        begin,
        end,
        relative_link,
        document_line,
        ..
    } in &spans
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
                            .or_insert_with(|| super::file_ending_with(s, cited))
                            .clone()
                    })
                    .flatten();
                let error = if *relative_link {
                    link_target_error(s, output, cited, *begin, *end, error)
                } else {
                    error.clone()
                };
                let missing_file = !*relative_link && error.starts_with("file_not_found");
                let mut issue = json!({"kind":"citation_path","citation":raw,"line":document_line,"error":error});
                if let Some(guidance) = guidance {
                    issue["guidance"] = json!(guidance);
                } else if missing_file && suggested.is_none() {
                    issue["guidance"] = json!(FORMAT_EXAMPLE_GUIDANCE);
                }
                if let Some(file) = suggested {
                    issue["suggested"] = json!(format!("{file}{}", &raw[cited.len()..]));
                    fixable.push((*document_line, cited.clone(), file));
                }
                issues.push(issue);
            }
        }
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
    for (line_number, paths) in by_line {
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
        while current.matches(&line[start..end]).count() != 1 {
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
    let (_, issues, fixable) = citation_issues_and_paths(s, output, doc).ok()?;
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
    for c in pattern.captures_iter(line) {
        let whole = c.get(0).unwrap();
        let path = c.get(1).unwrap();
        let Some(file) = paths.get(path.as_str()) else {
            continue;
        };
        if url_prefix(&line[..whole.start()], whole.as_str()) {
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
    s: &Session,
    output: &Path,
    target: &str,
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
    let instead = match project_file_for_link(s, target) {
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
