use super::*;

/// Called only after the final batch budget has been applied. Archives, search
/// excerpts and outlines are deliberately not treated as delivered document text.
pub fn record_delivered_read(s: &mut Session, call: &crate::llm::ToolCall, result: &Value) {
    if !matches!(
        call.name.as_str(),
        "file_read" | "symbol_read" | "document_inspect"
    ) || result["status"] != "ok"
    {
        return;
    }
    let data = &result["data"];
    let Some(shown) = data["content"]["text"].as_str() else {
        return;
    };
    let Some(path) = data["path"].as_str() else {
        return;
    };
    let Ok(allowed) = read_path(&s.project, path) else {
        return;
    };
    let Ok(doc) = read_text(&allowed) else {
        return;
    };
    let digest = hash(doc.as_bytes());
    if data["hash"] != digest {
        return;
    }
    let normalized = doc.replace("\r\n", "\n");
    if shown.is_empty() && data["content"]["last_line_complete"] != true {
        return;
    }
    let (start, mut end) = if matches!(call.name.as_str(), "file_read" | "symbol_read") {
        let line = data["read_start"].as_u64().unwrap_or(1) as usize;
        let start = normalized
            .split_inclusive('\n')
            .take(line.saturating_sub(1))
            .map(|l| l.chars().count())
            .sum::<usize>()
            + data["read_offset"].as_u64().unwrap_or(0) as usize;
        (start, start + shown.chars().count())
    } else {
        let Some(section) = data["section"].as_str() else {
            return;
        };
        let Ok(heading) = documentation::resolve_heading(&doc, section) else {
            return;
        };
        let raw_start = doc[..heading.start].chars().count() + n(data, "read_offset", 0);
        let normalized_offset = |raw: usize| {
            doc.char_indices()
                .take(raw)
                .filter(|&(i, _)| !doc[i..].starts_with("\r\n"))
                .count()
        };
        (
            normalized_offset(raw_start),
            normalized_offset(raw_start + shown.chars().count()),
        )
    };
    // file_read omits the range's final line terminator. The boundary flag
    // certifies that the full line body was delivered, including empty lines.
    if matches!(call.name.as_str(), "file_read" | "symbol_read")
        && data["content"]["last_line_complete"] == true
        && (!shown.ends_with('\n') || data["content"]["truncated"] != true)
        && normalized.chars().nth(end) == Some('\n')
    {
        end += 1;
    }
    let key = Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path))
        .to_string_lossy()
        .into_owned();
    let entry = s.read_coverage.entry(key).or_default();
    if entry.hash != digest {
        entry.hash = digest;
        entry.ranges.clear();
    }
    entry.ranges.push((start, end));
    entry.ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for &(a, b) in &entry.ranges {
        if let Some(last) = merged.last_mut().filter(|last| a <= last.1) {
            last.1 = last.1.max(b);
        } else {
            merged.push((a, b));
        }
    }
    entry.ranges = merged;
}

pub(super) fn report(
    s: &Session,
    path: &Path,
    doc: &str,
    offset: usize,
    limit: usize,
) -> (Value, Vec<bool>) {
    let key = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_owned())
        .to_string_lossy()
        .into_owned();
    let digest = hash(doc.as_bytes());
    let prior = s.read_coverage.get(&key);
    let ranges = prior
        .filter(|r| r.hash == digest)
        .map(|r| r.ranges.as_slice())
        .unwrap_or(&[]);
    let normalized = doc.replace("\r\n", "\n");
    let mut cursor = 0;
    let mut covered = Vec::new();
    let mut index = 0;
    for line in normalized.split_inclusive('\n') {
        let len = line.chars().count();
        let body = line.strip_suffix('\n').unwrap_or(line).chars().count();
        let end = cursor + body.max(1);
        while index < ranges.len() && ranges[index].1 <= cursor {
            index += 1;
        }
        covered.push(
            ranges
                .get(index)
                .is_some_and(|&(a, b)| a <= cursor && b >= end),
        );
        cursor += len;
    }
    let mut missing: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < covered.len() {
        if covered[i] {
            i += 1;
            continue;
        }
        let start = i + 1;
        while i < covered.len() && !covered[i] {
            i += 1;
        }
        missing.push(json!({"start_line":start,"end_line":i}));
    }
    let end = offset
        .saturating_add(limit.clamp(1, 100))
        .min(missing.len());
    let page = if offset < missing.len() {
        &missing[offset..end]
    } else {
        &[]
    };
    (
        json!({"scope":"session_delivered_text","hash":digest,"revision":hash(&serde_json::to_vec(&(digest.as_str(), ranges)).expect("coverage revision input is serializable")),"previous_revision_ignored":prior.is_some_and(|r| r.hash != digest),"total_lines":covered.len(),"fully_read_lines":covered.iter().filter(|&&v|v).count(),"complete":covered.iter().all(|&v|v),"missing_ranges":page,"missing_range_count":missing.len(),"next_offset":(end < missing.len()).then_some(end)}),
        covered,
    )
}
