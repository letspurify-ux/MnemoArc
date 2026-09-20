use super::*;

pub(super) struct Heading {
    pub heading: String,
    pub start: usize,
    pub end: usize,
    pub(super) line: usize,
    level: usize,
}

/// ATX headings outside fenced code. Byte offsets preserve Unicode and CRLF.
pub(super) fn headings(doc: &str) -> Vec<Heading> {
    let mut result: Vec<Heading> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut offset = 0;
    for (i, line) in doc.split_inclusive('\n').enumerate() {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let first = trimmed.chars().next().unwrap_or(' ');
        let run = trimmed.chars().take_while(|c| *c == first).count();
        if indent <= 3 {
            if let Some((ch, size)) = fence {
                if first == ch && run >= size && trimmed[run..].trim().is_empty() {
                    fence = None;
                }
            } else if (first == '`' || first == '~') && run >= 3 {
                fence = Some((first, run));
            } else if first == '#'
                && (1..=6).contains(&run)
                && trimmed.as_bytes().get(run) == Some(&b' ')
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
        offset += line.len();
    }
    result
}

/// Full headings match exactly; bare titles are accepted only when unique.
pub(super) fn resolve_heading(doc: &str, requested: &str) -> Result<Heading> {
    let requested = requested.trim();
    let headings = headings(doc);
    let matches = |h: &&Heading| {
        if requested.is_empty() {
            return false;
        }
        if requested.starts_with('#') {
            h.heading == requested
        } else {
            h.heading.trim_start_matches('#').trim_start() == requested
        }
    };
    let matching: Vec<_> = headings.iter().filter(matches).collect();
    if matching.len() != 1 {
        let candidates: Vec<_> = if matching.is_empty() {
            headings.iter().take(8).collect()
        } else {
            matching.iter().copied().take(8).collect()
        };
        let candidates: Vec<_> = candidates
            .into_iter()
            .map(|h| json!({"heading":h.heading,"start_line":h.line}))
            .collect();
        if matching.is_empty() {
            bail!(
                "section_not_found: {requested:?}; use document_inspect without section for the outline. Headings: {}",
                json!(candidates)
            );
        }
        bail!(
            "ambiguous_section: {requested:?} matches {} headings; supply an exact heading including # when levels differ. If exact headings repeat, use file_read with start_line for reading. Matches: {}",
            matching.len(),
            json!(candidates)
        );
    }
    let start = matching[0].start;
    Ok(headings.into_iter().find(|h| h.start == start).unwrap())
}

pub(super) fn execute(
    s: &mut Session,
    name: &str,
    args: &Value,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    match name {
        "document_inspect" => {
            let path = if let Some(path) = args["path"].as_str() {
                read_path(&s.project, path)?
            } else {
                output_path(&s.project)?
            };
            if !path.exists() {
                return Ok(json!({"exists":false,"path":path,"total_lines":0}));
            }
            let doc = read_text(&path)?;
            if n(args, "offset", 0) > 0 && args["expected_hash"].as_str().is_none() {
                bail!(
                    "document_hash_required: offset > 0 requires expected_hash from the first document_inspect result; copy its hash or the returned next_cursor arguments. If that result is unavailable, call document_inspect with offset 0 first"
                );
            }
            if let Some(expected) = args["expected_hash"].as_str()
                && expected != hash(doc.as_bytes())
            {
                bail!(
                    "document_revision_conflict: document changed during paged read; restart document_inspect with offset 0 and use its new hash"
                );
            }
            let mut result = json!({"exists":true,"path":path,"hash":hash(doc.as_bytes()),"total_lines":doc.lines().count(),"bytes":doc.len()});
            if let Some(heading) = args["section"].as_str() {
                let resolved = resolve_heading(&doc, heading)?;
                let section = &doc[resolved.start..resolved.end];
                let offset = n(args, "offset", 0);
                let section_chars = section.chars().count();
                if offset > section_chars {
                    bail!(
                        "invalid_offset: offset {offset} is beyond {section_chars} characters in section {:?}; use content.next_offset from the prior page or restart at offset 0",
                        resolved.heading
                    );
                }
                result["section"] = json!(resolved.heading);
                result["section_hash"] = json!(hash(section.as_bytes()));
                result["read_offset"] = json!(n(args, "offset", 0));
                result["content"] = bounded_text(s, section, n(args, "offset", 0));
                result["start_line"] = json!(resolved.line);
                result["section_lines"] = json!(section.lines().count());
            } else {
                let headings = headings(&doc);
                let offset = n(args, "offset", 0);
                if offset > headings.len() {
                    bail!(
                        "invalid_offset: offset {offset} is an outline heading index, but this document has {} headings; it is not a document line number. Use file_read with start_line to read a line range, or copy next_offset from the previous document_inspect outline page",
                        headings.len()
                    );
                }
                let end = (offset + n(args, "limit", 50).clamp(1, 100)).min(headings.len());
                let (coverage, read_lines) = super::coverage::report(
                    s,
                    &path,
                    &doc,
                    n(args, "coverage_offset", 0),
                    n(args, "limit", 50),
                );
                result["coverage"] = coverage;
                result["outline"] = json!(headings[offset..end].iter().map(|h| {
                    let lines = doc[h.start..h.end].lines().count();
                    json!({"heading":h.heading,"start_line":h.line,"lines":lines,"hash":hash(&doc.as_bytes()[h.start..h.end]),"fully_read":read_lines[h.line-1..h.line-1+lines].iter().all(|&v|v)})
                }).collect::<Vec<_>>());
                result["next_offset"] = json!((end < headings.len()).then_some(end));
            }
            Ok(result)
        }
        "symbol_search" => {
            let declaration = regex::Regex::new(
                r"^\s*(?:(?:export|default|pub(?:\([^)]*\))?|async|abstract|declare|static)\s+)*(?:(?:function\*?|class|interface|type|enum|struct|trait|fn|def|const|let|var)\s+([\p{L}_$][\p{L}\p{N}_$]*))",
            )?;
            let query = args["query"].as_str().unwrap_or("").to_lowercase();
            let files = candidate_paths(&s.project, path_glob(args)?, cancel)?;
            let mut matched_files = 0;
            let mut scanned_files = 0usize;
            let mut rows = vec![];
            let mut fingerprint = Sha256::new();
            fingerprint.update(query.as_bytes());
            for path in files {
                if cancel.is_cancelled() {
                    bail!("cancelled");
                }
                let Some(contents) = search_text(&path)? else {
                    continue;
                };
                matched_files += 1;
                if !matches!(
                    path.extension().and_then(|x| x.to_str()),
                    Some("rs" | "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "py")
                ) {
                    continue;
                }
                scanned_files += 1;
                let digest = hash(contents.as_bytes());
                fingerprint.update(path.to_string_lossy().as_bytes());
                fingerprint.update(digest.as_bytes());
                for (i, line) in contents.lines().enumerate() {
                    if i % 256 == 0 && cancel.is_cancelled() {
                        bail!("cancelled");
                    }
                    if let Some(c) = declaration.captures(line)
                        && c[1].to_lowercase().contains(&query)
                    {
                        rows.push((
                            path.clone(),
                            digest.clone(),
                            i + 1,
                            c[1].to_string(),
                            line.chars().take(500).collect::<String>(),
                        ));
                        if rows.len() > 100000 {
                            bail!("search_too_broad: narrow pattern/query");
                        }
                    }
                }
            }
            let fingerprint = format!("{:x}", fingerprint.finalize());
            let offset = page_cursor(args, &fingerprint)?;
            if offset > rows.len() {
                bail!("invalid_cursor");
            }
            let end = (offset + n(args, "limit", 20).clamp(1, 100)).min(rows.len());
            let results=rows[offset..end].iter().map(|(path,digest,line,name,excerpt)| {
                let source=observe_hashed(s,path,digest.clone(),*line,*line,excerpt);
                json!({"name":name,"path":path,"line":line,"declaration":excerpt,"source":source})
            }).collect::<Vec<_>>();
            Ok(
                json!({"hash":fingerprint,"symbols":results,"matched_files":matched_files,"scanned_files":scanned_files,"heuristic":true,"limitations":"Declarations only; may include constants/comments and miss multiline or method declarations. Not semantic references.","next_cursor":(end<rows.len()).then(||format!("{fingerprint}:{end}"))}),
            )
        }
        "document_audit" => {
            revalidate(s)?;
            let path = output_path(&s.project)?;
            let doc = read_text(&path)?;
            let (checked, mut issues) = citation_issues(s, &path, &doc)?;
            if checked == 0 {
                issues.push(json!({"kind":"no_machine_readable_citations","guidance":"Use relative/path.ext:start-end. Other citation formats require manual review."}));
            }
            if s.investigations.is_empty() {
                issues.push(json!({"kind":"no_investigation_coverage"}));
            }
            for item in &s.investigations {
                if item.status != "verified" {
                    issues.push(json!({"kind":"pending_verification","id":item.id,"section":item.section,"status":item.status,"next":"Read the linked document section and sources, then verify_batch with evidence and a comparison note."}));
                }
                if section_text(&doc, &item.section).is_err() {
                    issues.push(json!({"kind":"missing_or_ambiguous_section","id":item.id,"section":item.section}));
                }
                for source in &item.sources {
                    if let Some(p) = &source.path {
                        let current = read_path(&s.project, p)
                            .and_then(|p| read_text(&p))
                            .ok()
                            .map(|t| hash(t.as_bytes()));
                        if current != source.hash {
                            issues.push(json!({"kind":"stale_source","id":item.id,"path":p}));
                        }
                    }
                }
            }
            let offset = n(args, "offset", 0);
            if offset > issues.len() {
                bail!(
                    "invalid_offset: audit issue offset {offset} exceeds {} issues; restart document_audit at offset 0 or copy its prior next_offset",
                    issues.len()
                );
            }
            let end = (offset + n(args, "limit", 30).clamp(1, 100)).min(issues.len());
            Ok(
                json!({"hash":hash(doc.as_bytes()),"total_lines":doc.lines().count(),"citations_checked":checked,"structural_ok":issues.is_empty(),"semantic_verified":false,"issue_count":issues.len(),"issues":issues[offset..end],"next_offset":(end<issues.len()).then_some(end)}),
            )
        }
        _ => bail!("unsupported_tool"),
    }
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

pub(super) fn citation_spans(doc: &str) -> Result<Vec<Citation>> {
    let pattern = regex::Regex::new(
        r"([\p{L}\p{N}_./@-]+\.[A-Za-z][A-Za-z0-9]*)(:|#L)([0-9]+)(?:[-–]L?([0-9]+))?",
    )?;
    let continuation = regex::Regex::new(r"^\s*,\s*([0-9]+)(?:[-–]L?([0-9]+))?")?;
    let mut spans = vec![];
    let mut fence: Option<(char, usize, bool)> = None;
    for (document_line, line) in doc.lines().enumerate() {
        let trimmed = line.trim_start_matches(' ');
        let marker = trimmed.chars().next().unwrap_or(' ');
        let width = trimmed.chars().take_while(|c| *c == marker).count();
        if line.len() - trimmed.len() <= 3 {
            if let Some((open, size, _)) = fence {
                if marker == open && width >= size && trimmed[width..].trim().is_empty() {
                    fence = None;
                    continue;
                }
            } else if (marker == '`' || marker == '~') && width >= 3 {
                fence = Some((
                    marker,
                    width,
                    trimmed[width..].trim().eq_ignore_ascii_case("mermaid"),
                ));
                continue;
            }
        }
        if fence.is_some_and(|(_, _, mermaid)| !mermaid) {
            continue;
        }
        for c in pattern.captures_iter(line) {
            let before = &line[..c.get(0).unwrap().start()];
            let prefix = before
                .rsplit(|ch: char| ch.is_whitespace() || ['`', '(', '"'].contains(&ch))
                .next()
                .unwrap_or("");
            if prefix.contains("://") {
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
                document_line,
            });
            // Repeat the path internally for grouped citations such as a.js:3, 8-10.
            let mut tail = &line[c.get(0).unwrap().end()..];
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
                    document_line,
                });
                tail = &tail[extra.get(0).unwrap().end()..];
            }
        }
    }
    Ok(spans)
}

fn citation_issues(s: &Session, output: &Path, doc: &str) -> Result<(usize, Vec<Value>)> {
    let spans = citation_spans(doc)?;
    let mut issues = vec![];
    let mut versions = std::collections::BTreeMap::new();
    for Citation {
        raw,
        path,
        begin,
        end,
        relative_link,
        ..
    } in &spans
    {
        let path = if *relative_link {
            output
                .parent()
                .unwrap()
                .join(path)
                .to_string_lossy()
                .into_owned()
        } else {
            path.clone()
        };
        let check = versions.entry(path.clone()).or_insert_with(|| {
            read_path(&s.project, &path)
                .and_then(|p| read_text(&p))
                .map(|t| t.lines().count())
                .map_err(|e| e.to_string())
        });
        match check {
            Ok(lines) if *begin > 0 && end >= begin && end <= lines => {}
            Ok(_) => issues.push(json!({"kind":"citation_range","citation":raw})),
            Err(error) => issues.push(json!({"kind":"citation_path","citation":raw,"error":error})),
        }
    }
    Ok((spans.len(), issues))
}

pub(super) fn citation_check(s: &Session, output: &Path, doc: &str) -> Result<Value> {
    let (checked, issues) = citation_issues(s, output, doc)?;
    Ok(
        json!({"citations_checked":checked,"issue_count":issues.len(),"issues":issues.iter().take(8).collect::<Vec<_>>(),"semantic_verified":false,"guidance":"Fix citation issues in the next section edit. Partial drafts may still lack investigation coverage."}),
    )
}
