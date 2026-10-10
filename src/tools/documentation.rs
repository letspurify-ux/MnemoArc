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
pub(super) fn headings(doc: &str) -> Vec<Heading> {
    let mut result: Vec<Heading> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut in_comment = false;
    let mut offset = 0;
    for (i, line) in doc.split_inclusive('\n').enumerate() {
        let was_fenced = fence.is_some();
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let first = trimmed.chars().next().unwrap_or(' ');
        let run = trimmed.chars().take_while(|c| *c == first).count();
        if indent <= 3 {
            if let Some((ch, size)) = fence {
                if first == ch && run >= size && trimmed[run..].trim().is_empty() {
                    fence = None;
                }
            } else if !in_comment && (first == '`' || first == '~') && run >= 3 {
                fence = Some((first, run));
            } else if !in_comment
                && first == '#'
                && (1..=6).contains(&run)
                && trimmed
                    .as_bytes()
                    .get(run)
                    .is_none_or(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
            {
                for prior in result.iter_mut().rev() {
                    if prior.end == doc.len() && prior.level >= run {
                        prior.end = offset;
                    }
                }
                result.push(Heading {
                    heading: line.trim().to_string(),
                    start: offset,
                    end: doc.len(),
                    line: i + 1,
                    level: run,
                });
            }
        }
        if !was_fenced && fence.is_none() && (in_comment || indent < 4) {
            in_comment = html_comment_state(line, in_comment);
        }
        offset += line.len();
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
                return Ok(json!({"exists":false,"path":path,"total_lines":0,
                    "guidance":"Nothing has been written to the configured output yet. Create it with document_edit action=create (or document_edit_batch) before reading or auditing it."}));
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
                bail!(
                    "document_hash_required: offset or coverage_offset > 0 requires expected_hash from the first document_inspect result; copy its hash or the returned next_cursor arguments. If that result is unavailable, call document_inspect with offset 0 and coverage_offset 0 first"
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
                    bail!(
                        "invalid_offset: offset {offset} is an outline heading index, but this document has {} headings; it is not a document line number. Use file_read with start_line to read a line range, or copy next_offset from the previous document_inspect outline page",
                        headings.len()
                    );
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
            let (checked, mut issues) = citation_issues(s, &path, &doc)?;
            let format = super::document_format::check(&doc);
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
                issues.push(json!({"kind":"no_machine_readable_citations","guidance":NO_CITATIONS_GUIDANCE}));
            }
            // The output's own absolute path in its text is a run report
            // (where it was written), which belongs in the final answer.
            if let Some(line) = output_path_line(&path, &doc) {
                issues.push(json!({"kind":"output_path_in_document","line":line,
                    "guidance":"The document states its own output path, a report of how it was produced. Remove that text; give the path, verification scope and limitations in the final chat answer instead."}));
            }
            issues.extend(unread_citations(s, &doc)?);
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
            let mut result = json!({"hash":document_hash,"revision":revision,"total_lines":doc.lines().count(),"citations_checked":checked,"structural_ok":structural_ok,"semantic_verified":false,"format_check":format_check,"issue_count":issues.len(),"issue_kinds":issue_kinds(&issues),"issues":issues[offset..end],"next_offset":(end<issues.len()).then_some(end)});
            if s.is_document_work()
                && let Some(check) = test_code_check(s, &path, &doc)
            {
                result["test_code_check"] = check;
            }
            Ok(result)
        }
        _ => bail!("unsupported_tool"),
    }
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
    for source in s.sources.values() {
        if source.origin != "file" || source.evidence_truncated {
            continue;
        }
        let (Some(path), Some(start), Some(end)) =
            (source.path.as_deref(), source.start_line, source.end_line)
        else {
            continue;
        };
        let Ok(resolved) = read_path(&s.project, path) else {
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
    for citation in citation_spans(doc)? {
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
        let Ok(cited) = read_path(&s.project, &path) else {
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
            let mut issue = json!({"kind":"unread_citation","path":path,"start_line":start,"end_line":end,
                "next":format!("file_read {path} with start_line {start} through line {end} (complete lines of the current file version), or narrow the citation to the lines already read")});
            // A live run read 900 lines in pages because an opening cited a
            // whole screen component as a pointer; offer the cheaper repair.
            if end + 1 - start >= BROAD_CITATION_LINES {
                issue["note"] = json!(
                    "This range is long: if the citation only points at where a screen, component or feature lives, cite its entry lines (for example the declaration) instead of its whole span"
                );
            }
            issues.push(issue);
        }
    }
    Ok(issues)
}

struct CitationFence {
    marker: char,
    width: usize,
    mermaid: bool,
    start_line: usize,
    list_content_indent: Option<usize>,
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
const CITATION_PATTERN: &str =
    r"([\p{L}\p{N}_./@-]+\.[A-Za-z][A-Za-z0-9]*)(:|#L)([0-9]+)(?:[-–]L?([0-9]+))?";

/// Whether the text before a citation match makes it part of a URL.
fn url_prefix(before: &str) -> bool {
    before
        .rsplit(|ch: char| ch.is_whitespace() || ['`', '(', '"'].contains(&ch))
        .next()
        .unwrap_or("")
        .contains("://")
}

fn scan_citations(doc: &str) -> Result<(Vec<Citation>, Vec<Value>)> {
    let pattern = regex::Regex::new(CITATION_PATTERN)?;
    let continuation = regex::Regex::new(r"^\s*,\s*([0-9]+)(?:[-–]L?([0-9]+))?")?;
    let mut spans = vec![];
    let mut issues = vec![];
    let mut fence: Option<CitationFence> = None;
    let mut in_comment = false;
    for (document_line, line) in doc.lines().enumerate() {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        if let Some(open) = &fence {
            let base_indent = open.list_content_indent.unwrap_or(0);
            // An outdented sibling item or heading ends a list item even if
            // its code fence has no explicit closer.
            if open.list_content_indent.is_some() && !line.trim().is_empty() && indent < base_indent
            {
                issues.push(unclosed_fence_issue(open));
                fence = None;
            } else if indent >= base_indent
                && indent <= base_indent + 3
                && fence_run(trimmed).is_some_and(|(marker, width, tail)| {
                    marker == open.marker && width >= open.width && tail.trim().is_empty()
                })
            {
                fence = None;
                continue;
            }
        }
        if fence.as_ref().is_some_and(|open| !open.mermaid) {
            continue;
        }
        if fence.is_none() {
            let opening = (indent <= 3)
                .then(|| fence_opener(trimmed).map(|run| (run, None)))
                .flatten()
                .or_else(|| {
                    list_fence_content(line).and_then(|(content_indent, content)| {
                        fence_opener(content).map(|run| (run, Some(content_indent)))
                    })
                });
            if !in_comment && let Some(((marker, width, info), list_content_indent)) = opening {
                fence = Some(CitationFence {
                    marker,
                    width,
                    mermaid: info.trim().eq_ignore_ascii_case("mermaid"),
                    start_line: document_line + 1,
                    list_content_indent,
                });
                continue;
            }
            if indent >= 4 {
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
            if url_prefix(&visible[..c.get(0).unwrap().start()]) {
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
            });
            // Repeat the path internally for grouped citations such as a.js:3, 8-10.
            let mut tail = &visible[c.get(0).unwrap().end()..];
            while let Some(extra) = continuation.captures(tail) {
                let begin = extra[1].parse().unwrap_or(0);
                let end = extra
                    .get(2)
                    .map(|m| m.as_str().parse().unwrap_or(0))
                    .unwrap_or(begin);
                spans.push(Citation {
                    raw: format!("{}{}{}-{}", &c[1], &c[2], begin, end),
                    path: c[1].into(),
                    begin,
                    end,
                    relative_link: &c[2] == "#L",
                    document_line: document_line + 1,
                });
                tail = &tail[extra.get(0).unwrap().end()..];
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
        let Ok(resolved) = read_path(&s.project, &path) else {
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
    let (spans, mut issues) = scan_citations(doc)?;
    let mut versions = std::collections::BTreeMap::new();
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
            read_path(&s.project, &path)
                .and_then(|p| super::text_line_count(&p))
                .map_err(|e| e.to_string())
        });
        match check {
            Ok(lines) if *begin > 0 && end >= begin && end <= lines => {}
            Ok(_) => {
                issues.push(json!({"kind":"citation_range","citation":raw,"line":document_line}))
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
                let mut issue = json!({"kind":"citation_path","citation":raw,"line":document_line,"error":error});
                if let Some(guidance) = guidance {
                    issue["guidance"] = json!(guidance);
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
    Ok((spans.len(), issues))
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
        let mut rewrites = Vec::new();
        let mut span: Option<(usize, usize)> = None;
        for c in pattern.captures_iter(line) {
            let whole = c.get(0).unwrap();
            let path = c.get(1).unwrap();
            let Some(file) = paths.get(path.as_str()) else {
                continue;
            };
            if url_prefix(&line[..whole.start()]) {
                continue;
            }
            rewrites.push((path.start(), path.end(), *file));
            span = Some(span.map_or((whole.start(), whole.end()), |(start, end)| {
                (start.min(whole.start()), end.max(whole.end()))
            }));
        }
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
        let mut text = String::new();
        let mut at = start;
        for (path_start, path_end, file) in rewrites {
            text.push_str(&line[at..path_start]);
            text.push_str(file);
            at = path_end;
        }
        text.push_str(&line[at..end]);
        let fix = json!({"action":"replace_text","old_text":&line[start..end],"text":text});
        if let Ok(next) = super::apply_document_edit_operation(&current, &fix) {
            current = next;
            fixes.insert(line_number, fix);
        }
    }
    fixes
}

/// A cited file the tools refuse to read fails every audit until the
/// citation goes. The bare read error gave a model nothing to act on.
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
const NO_CITATIONS_GUIDANCE: &str = "The document cites no project source as relative/path.ext:start-end. Cite the sources of its claims next to them (for example backend/src/server.js:12-30); the final answer is refused while the document cites no project source. memory_read returns the path and lines of each source saved with a memory. Other citation formats require manual review.";

pub(super) fn citation_check(s: &Session, output: &Path, doc: &str) -> Result<Value> {
    let (checked, issues) = citation_issues(s, output, doc)?;
    // The write already succeeded: a failed unread check is reported beside
    // it, not as a failure of the save.
    let (unread, unread_error) = match unread_citations(s, doc) {
        Ok(unread) => (unread, None),
        Err(error) => (vec![], Some(error.to_string())),
    };
    let guidance = "Fix citation or code-fence issues in the next section edit; a citation_path issue's fix is a ready document_edit_batch edit that rewrites the citations on its line to the suggested project path (an issue marked covered_by_line_fix needs no edit of its own), so send the listed fixes together in one document_edit_batch. unread_citations are cited ranges never delivered to you as complete lines of the current file version: file_read each listed range, or narrow the citation to the lines you read, before the final answer; the final audit treats them as unresolved evidence.";
    let mut check = json!({"citations_checked":checked,"issue_count":issues.len(),"issues":issues.iter().take(8).collect::<Vec<_>>(),
        "unread_citation_count":unread.len(),"unread_citations":unread.iter().take(8).collect::<Vec<_>>(),
        "unread_citation_error":unread_error,
        "semantic_verified":false,
        "guidance":guidance});
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
