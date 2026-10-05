//! Runtime evidence for preservation constraints, separate from stale reads.
use super::*;
use std::{ops::Range, sync::Arc};

const MAX_BASELINE_BYTES: usize = 256 * 1024;
const MAX_DIFF_CELLS: usize = 1_000_000;
const MAX_CHANGES: usize = 64;
const MAX_CHANGE_CHARS: usize = 12_000;

#[derive(Clone, Debug, Serialize)]
pub(super) struct Baseline {
    path: String,
    existed: bool,
    hash: String,
    bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<Arc<str>>,
}

impl Baseline {
    pub(super) fn new(path: &Path, old: &str, existed: bool) -> Self {
        Self {
            path: path.display().to_string(),
            existed,
            hash: hash(old.as_bytes()),
            bytes: old.len(),
            text: (existed && old.len() <= MAX_BASELINE_BYTES).then(|| Arc::from(old)),
        }
    }

    pub(super) fn is_creation(&self) -> bool {
        !self.existed
    }

    pub(super) fn evidence(&self, path: &str, current: &str) -> Option<Value> {
        // Creating a new document has no pre-existing body to preserve.
        if !self.existed || self.path != path {
            return None;
        }
        let mut evidence = json!({"kind":"runtime_document_changes","path":path,
            "baseline_hash":self.hash,"current_hash":hash(current.as_bytes()),
            "baseline_bytes":self.bytes,"baseline_retained":self.text.is_some(),
            "note":"Computed by the runtime from the exact document before the first successful edit of the current work, compared with the current file. Requirement amendments during unfinished edits of an existing document retain the original baseline. New tasks and explicit amendments after completion start a new comparison; an explicit amendment to a document created by this task can also start comparing against its saved draft. Retries, checkpoints and plain continuation retain the comparison. Before text is historical comparison evidence, not current content. Line ranges use start_line and line_count; a zero count denotes an insertion/deletion boundary. Unchanged text outside all reported ranges is byte-identical only when unchanged_outside_reported_ranges is true. Truncated comparisons cannot prove the omitted changes."});
        if evidence["current_hash"] == self.hash {
            evidence["unchanged"] = json!(true);
            evidence["total_changes"] = json!(0);
            evidence["changes"] = json!([]);
            evidence["truncated"] = json!(false);
            evidence["unchanged_outside_reported_ranges"] = json!(true);
            return Some(evidence);
        }
        let Some(old) = &self.text else {
            evidence["truncated"] = json!(true);
            evidence["unchanged_outside_reported_ranges"] = json!(false);
            evidence["reason"] = json!(
                "The pre-edit document exceeds the 256KiB baseline retention limit; its hash alone does not prove preservation."
            );
            return Some(evidence);
        };
        let ranges = change_ranges(old, current);
        let mut remaining = MAX_CHANGE_CHARS;
        let mut truncated = ranges.len() > MAX_CHANGES;
        let changes = ranges
            .iter()
            .take(MAX_CHANGES)
            .map(|change| {
                let (before, before_truncated) =
                    preview(&old[change.before.clone()], &mut remaining);
                let (after, after_truncated) =
                    preview(&current[change.after.clone()], &mut remaining);
                truncated |= before_truncated || after_truncated;
                json!({"before":{"start_line":change.before_line,"line_count":change.before_lines,
                "text":before,"truncated":before_truncated},
                "after":{"start_line":change.after_line,"line_count":change.after_lines,
                "text":after,"truncated":after_truncated}})
            })
            .collect::<Vec<_>>();
        evidence["unchanged"] = json!(old.as_ref() == current);
        evidence["total_changes"] = json!(ranges.len());
        evidence["changes"] = json!(changes);
        evidence["truncated"] = json!(truncated);
        evidence["unchanged_outside_reported_ranges"] = json!(ranges.len() <= MAX_CHANGES);
        Some(evidence)
    }
}

fn preview(text: &str, remaining: &mut usize) -> (String, bool) {
    let mut chars = text.chars();
    let shown: String = chars
        .by_ref()
        .take((*remaining).min(MAX_CHANGE_CHARS / 2))
        .collect();
    *remaining -= shown.chars().count();
    (shown, chars.next().is_some())
}

struct Change {
    before: Range<usize>,
    after: Range<usize>,
    before_line: usize,
    before_lines: usize,
    after_line: usize,
    after_lines: usize,
}

/// Exact line comparison, including CRLF and the final newline. Trim matching
/// ends first; bound both the LCS matrix and previews. Large dissimilar middles
/// use one enclosing range, which still proves preservation outside that range.
fn change_ranges(before: &str, after: &str) -> Vec<Change> {
    let mut prefix = 0;
    let mut prefix_lines = 0;
    for (a, b) in before
        .split_inclusive('\n')
        .zip(after.split_inclusive('\n'))
    {
        if a != b {
            break;
        }
        prefix += a.len();
        prefix_lines += 1;
    }
    let mut suffix = 0;
    for (a, b) in before[prefix..]
        .split_inclusive('\n')
        .rev()
        .zip(after[prefix..].split_inclusive('\n').rev())
    {
        if a != b {
            break;
        }
        suffix += a.len();
    }
    let old = &before[prefix..before.len() - suffix];
    let new = &after[prefix..after.len() - suffix];
    if old.is_empty() && new.is_empty() {
        return vec![];
    }
    let n = old.split_inclusive('\n').count();
    let m = new.split_inclusive('\n').count();
    if (n + 1).saturating_mul(m + 1) > MAX_DIFF_CELLS {
        return vec![Change {
            before: prefix..before.len() - suffix,
            after: prefix..after.len() - suffix,
            before_line: prefix_lines + 1,
            before_lines: n,
            after_line: prefix_lines + 1,
            after_lines: m,
        }];
    }
    let old_lines: Vec<_> = old.split_inclusive('\n').collect();
    let new_lines: Vec<_> = new.split_inclusive('\n').collect();
    let offsets = |lines: &[&str]| {
        let mut result = vec![prefix];
        for line in lines {
            result.push(result.last().unwrap() + line.len());
        }
        result
    };
    let old_offsets = offsets(&old_lines);
    let new_offsets = offsets(&new_lines);
    let cols = m + 1;
    let mut lcs = vec![0u32; (n + 1) * cols];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * cols + j] = if old_lines[i] == new_lines[j] {
                lcs[(i + 1) * cols + j + 1] + 1
            } else {
                lcs[(i + 1) * cols + j].max(lcs[i * cols + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut result = vec![];
    while i < n || j < m {
        if i < n && j < m && old_lines[i] == new_lines[j] {
            i += 1;
            j += 1;
            continue;
        }
        let (start_i, start_j) = (i, j);
        while (i < n || j < m) && !(i < n && j < m && old_lines[i] == new_lines[j]) {
            if j < m && (i == n || lcs[i * cols + j + 1] >= lcs[(i + 1) * cols + j]) {
                j += 1;
            } else {
                i += 1;
            }
        }
        result.push(Change {
            before: old_offsets[start_i]..old_offsets[i],
            after: new_offsets[start_j]..new_offsets[j],
            before_line: prefix_lines + start_i + 1,
            before_lines: i - start_i,
            after_line: prefix_lines + start_j + 1,
            after_lines: j - start_j,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_reconstruct_text_with_repeated_lines_unicode_and_line_endings() {
        let mut texts = vec![String::new()];
        let mut level = vec![String::new()];
        for _ in 0..3 {
            level = level
                .iter()
                .flat_map(|prefix| ["가\n", "b\r\n", "가\n"].map(|line| format!("{prefix}{line}")))
                .collect();
            texts.extend(level.clone());
        }
        texts.extend(["마지막 줄".into(), "마지막 줄\n".into(), "\r\n".into()]);
        texts.sort();
        texts.dedup();
        for before in &texts {
            for after in &texts {
                let ranges = change_ranges(before, after);
                let mut reconstructed = before.clone();
                for change in ranges.iter().rev() {
                    reconstructed
                        .replace_range(change.before.clone(), &after[change.after.clone()]);
                }
                assert_eq!(&reconstructed, after, "before={before:?} after={after:?}");
            }
        }
    }

    #[test]
    fn oversized_comparisons_are_explicit_and_cannot_claim_preservation() {
        let path = Path::new("doc.md");
        let baseline = Baseline::new(path, &"x".repeat(MAX_BASELINE_BYTES + 1), true);
        assert!(baseline.text.is_none());
        let evidence = baseline.evidence("doc.md", "changed").unwrap();
        assert_eq!(evidence["baseline_retained"], false);
        assert_eq!(evidence["truncated"], true);
        assert_eq!(evidence["unchanged_outside_reported_ranges"], false);
        let unchanged = baseline
            .evidence("doc.md", &"x".repeat(MAX_BASELINE_BYTES + 1))
            .unwrap();
        assert_eq!(unchanged["unchanged"], true);
        assert_eq!(unchanged["truncated"], false);
        assert_eq!(unchanged["total_changes"], 0);

        let before = "keep\n".to_owned() + &"old\n".repeat(1100) + "end\n";
        let after = "keep\n".to_owned() + &"new\n".repeat(1100) + "end\n";
        let ranges = change_ranges(&before, &after);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].before_lines, 1100);
        let mut reconstructed = before.clone();
        reconstructed.replace_range(ranges[0].before.clone(), &after[ranges[0].after.clone()]);
        assert_eq!(reconstructed, after);

        let baseline = Baseline::new(path, "keep\nold\nend\n", true);
        let evidence = baseline
            .evidence(
                "doc.md",
                &("keep\n".to_owned() + &"新".repeat(20_000) + "\nend\n"),
            )
            .unwrap();
        assert_eq!(evidence["truncated"], true);
        assert_eq!(evidence["changes"][0]["after"]["truncated"], true);
        assert!(evidence.to_string().len() < MAX_CHANGE_CHARS * 4);

        let before: String = (0..=MAX_CHANGES)
            .map(|i| format!("keep {i}\nold {i}\n"))
            .collect();
        let after: String = (0..=MAX_CHANGES)
            .map(|i| format!("keep {i}\nnew {i}\n"))
            .collect();
        let evidence = Baseline::new(path, &before, true)
            .evidence("doc.md", &after)
            .unwrap();
        assert_eq!(evidence["total_changes"], MAX_CHANGES + 1);
        assert_eq!(evidence["changes"].as_array().unwrap().len(), MAX_CHANGES);
        assert_eq!(evidence["truncated"], true);
        assert_eq!(evidence["unchanged_outside_reported_ranges"], false);
    }
}
