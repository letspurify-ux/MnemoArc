use super::*;

/// Called only after the final batch budget has been applied. Archives and search
/// excerpts are deliberately not treated as delivered document text.
pub fn record_delivered_read(s: &mut Session, call: &crate::llm::ToolCall, result: &Value) {
    if result["status"] != "ok" {
        return;
    }
    // The observations this result delivers, a read's or each search
    // match's, and those a source lookup showed.
    let data = &result["data"];
    let delivered: Vec<&str> = std::iter::once(&data["source"])
        .chain(
            data["matches"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|row| &row["source"]),
        )
        .filter_map(|source| source["id"].as_str())
        .chain(
            (call.name == "source_lookup")
                .then(|| data["items"].as_array())
                .flatten()
                .into_iter()
                .flatten()
                .filter_map(|item| item["id"].as_str()),
        )
        .filter(|id| !s.delivered_sources.contains(*id))
        .collect();
    if !delivered.is_empty() {
        s.delivered_sources
            .extend(delivered.into_iter().map(str::to_owned));
    }
    if !matches!(call.name.as_str(), "file_read" | "document_inspect") {
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
    let Ok((doc, digest)) = read_text_hashed(&allowed) else {
        return;
    };
    if data["hash"] != digest {
        return;
    }
    if shown.is_empty() && data["content"]["last_line_complete"] != true {
        return;
    }
    // The byte where `count` characters after `from` end, the text's end
    // when it has fewer.
    let advance = |from: usize, count: usize| {
        doc[from..]
            .char_indices()
            .nth(count)
            .map_or(doc.len(), |(i, _)| from + i)
    };
    let byte_start = if call.name == "file_read" {
        let line = data["read_start"].as_u64().unwrap_or(1) as usize;
        let line_start = doc
            .split_inclusive('\n')
            .take(line.saturating_sub(1))
            .map(str::len)
            .sum::<usize>();
        advance(
            line_start,
            data["read_offset"].as_u64().unwrap_or(0) as usize,
        )
    } else {
        // Leaf headings can repeat under different parents. Reuse the full
        // path that identified this section, including on continuation pages.
        let Some(section) = data["section_path"]
            .as_str()
            .or_else(|| data["section"].as_str())
        else {
            return;
        };
        let Ok(heading) = documentation::resolve_heading(&doc, section) else {
            return;
        };
        advance(heading.start, n(data, "read_offset", 0))
    };
    let byte_end = advance(byte_start, shown.chars().count());
    // All reads retain raw line endings; coverage uses LF-normalized character
    // positions. Map each endpoint separately, including a split within CRLF.
    // Counted from byte positions: walking the characters up to each end
    // made a read near the end of a 16MiB file take three times as long as
    // one at its start.
    let normalized_offset = |byte: usize| {
        let before = &doc[..byte];
        before.chars().count()
            - before.matches("\r\n").count()
            - usize::from(before.ends_with('\r') && doc[byte..].starts_with('\n'))
    };
    let start = normalized_offset(byte_start);
    let mut end = normalized_offset(byte_end);
    // file_read omits the range's final line terminator. The boundary flag
    // certifies that the full line body was delivered, including empty lines.
    if call.name == "file_read"
        && data["content"]["last_line_complete"] == true
        && (!shown.ends_with('\n') || data["content"]["truncated"] != true)
        // The LF-normalized character at `end` is a line break exactly
        // where the original has LF or CRLF.
        && byte_end < doc.len()
        && (doc[byte_end..].starts_with('\n') || doc[byte_end..].starts_with("\r\n"))
    {
        end += 1;
    }
    let key = Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path))
        .to_string_lossy()
        .into_owned();
    let mut entry = s
        .read_coverage
        .get(&key)
        .map(|coverage| crate::session::ReadCoverage::clone(coverage))
        .unwrap_or_default();
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
    s.read_coverage.insert_counted(key, entry.into());
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
        json!({"revision":hash(&serde_json::to_vec(&(digest.as_str(), ranges)).expect("coverage revision input is serializable")),"previous_revision_ignored":prior.is_some_and(|r| r.hash != digest),"fully_read_lines":covered.iter().filter(|&&v|v).count(),"complete":covered.iter().all(|&v|v),"missing_ranges":page,"missing_range_count":missing.len(),"next_offset":(end < missing.len()).then_some(end)}),
        covered,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The delivered range as the character walk computed it.
    fn walked_range(
        doc: &str,
        line: usize,
        offset: usize,
        shown: &str,
        complete: bool,
    ) -> (usize, usize) {
        let raw_start = doc
            .split_inclusive('\n')
            .take(line - 1)
            .map(|l| l.chars().count())
            .sum::<usize>()
            + offset;
        let normalized = |raw: usize| {
            doc.char_indices()
                .take(raw)
                .filter(|&(i, _)| !doc[i..].starts_with("\r\n"))
                .count()
        };
        let raw_end = raw_start + shown.chars().count();
        let mut end = normalized(raw_end);
        if complete
            && doc
                .char_indices()
                .nth(raw_end)
                .is_some_and(|(i, _)| doc[i..].starts_with('\n') || doc[i..].starts_with("\r\n"))
        {
            end += 1;
        }
        (normalized(raw_start), end)
    }

    #[test]
    fn delivered_ranges_match_the_character_walk() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(
            crate::config::Project {
                root: dir.path().into(),
                output: dir.path().join("out.md"),
                ..Default::default()
            },
            crate::config::Config::compact_test(),
        );
        let path = dir.path().canonicalize().unwrap().join("a.txt");
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as usize
        };
        let pieces = ["a", "한", "\n", "\r\n", "\r", "é ", "xyz"];
        for round in 0..400 {
            let doc: String = (0..1 + next(30))
                .map(|_| pieces[next(pieces.len())])
                .collect();
            std::fs::write(&path, &doc).unwrap();
            let lines = doc.split_inclusive('\n').count();
            let line = 1 + next(lines);
            let line_start = doc
                .split_inclusive('\n')
                .take(line - 1)
                .map(str::len)
                .sum::<usize>();
            let rest = &doc[line_start..];
            let offset = next(rest.chars().count() + 1);
            let from = rest
                .char_indices()
                .nth(offset)
                .map_or(rest.len(), |(i, _)| i);
            let shown: String = rest[from..].chars().take(next(12)).collect();
            let complete = next(2) == 0;
            let call = crate::llm::ToolCall {
                id: format!("c{round}"),
                name: "file_read".into(),
                arguments: "{}".into(),
            };
            let result = json!({"status":"ok","data":{
                "path":path,"hash":hash(doc.as_bytes()),"read_start":line,"read_offset":offset,
                "content":{"text":shown,"last_line_complete":complete,"truncated":false}
            }});
            s.read_coverage = Default::default();
            record_delivered_read(&mut s, &call, &result);
            let key = path.to_string_lossy().into_owned();
            let ranges = s.read_coverage.get(&key).map(|c| c.ranges.clone());
            // An empty page records nothing unless its line was complete.
            let expected = (!shown.is_empty() || complete)
                .then(|| vec![walked_range(&doc, line, offset, &shown, complete)]);
            assert_eq!(
                ranges, expected,
                "{doc:?} line {line} offset {offset} shown {shown:?} complete {complete}"
            );
        }
    }
}
