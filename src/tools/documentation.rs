use super::*;

pub(super) struct Heading {
    pub heading: String,
    pub start: usize,
    pub end: usize,
    line: usize,
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

pub(super) fn execute(
    s: &mut Session,
    name: &str,
    args: &Value,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    match name {
        "document_inspect" => {
            let path = output_path(&s.project)?;
            if !path.exists() {
                return Ok(json!({"exists":false,"path":path,"total_lines":0}));
            }
            let doc = read_text(&path)?;
            if n(args, "offset", 0) > 0 && args["expected_hash"].as_str().is_none() {
                bail!("expected_hash required for paged document reads");
            }
            if let Some(expected) = args["expected_hash"].as_str()
                && expected != hash(doc.as_bytes())
            {
                bail!("document_revision_conflict: output changed during paged read");
            }
            let mut result = json!({"exists":true,"path":path,"hash":hash(doc.as_bytes()),"total_lines":doc.lines().count(),"bytes":doc.len()});
            if let Some(heading) = args["section"].as_str() {
                let section = section_text(&doc, heading)?;
                if n(args, "offset", 0) > section.chars().count() {
                    bail!("invalid_offset");
                }
                result["section"] = json!(heading.trim());
                result["section_hash"] = json!(hash(section.as_bytes()));
                result["content"] = bounded_text(s, section, n(args, "offset", 0));
                result["start_line"] = json!(
                    headings(&doc)
                        .iter()
                        .find(|h| h.heading == heading.trim())
                        .unwrap()
                        .line
                );
                result["section_lines"] = json!(section.lines().count());
            } else {
                let headings = headings(&doc);
                let offset = n(args, "offset", 0);
                if offset > headings.len() {
                    bail!("invalid_offset");
                }
                let end = (offset + n(args, "limit", 50).clamp(1, 100)).min(headings.len());
                result["outline"] = json!(headings[offset..end].iter().map(|h|json!({"heading":h.heading,"start_line":h.line,"lines":doc[h.start..h.end].lines().count(),"hash":hash(&doc.as_bytes()[h.start..h.end])})).collect::<Vec<_>>());
                result["next_offset"] = json!((end < headings.len()).then_some(end));
            }
            Ok(result)
        }
        "symbol_search" => {
            let declaration = regex::Regex::new(
                r"^\s*(?:(?:export|default|pub(?:\([^)]*\))?|async|abstract|declare|static)\s+)*(?:(?:function\*?|class|interface|type|enum|struct|trait|fn|def|const|let|var)\s+([\p{L}_$][\p{L}\p{N}_$]*))",
            )?;
            let query = args["query"].as_str().unwrap_or("").to_lowercase();
            let files = paths(&s.project, args["pattern"].as_str(), cancel)?;
            let mut rows = vec![];
            let mut fingerprint = Sha256::new();
            fingerprint.update(query.as_bytes());
            for path in files {
                if cancel.is_cancelled() {
                    bail!("cancelled");
                }
                if !matches!(
                    path.extension().and_then(|x| x.to_str()),
                    Some("rs" | "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "py")
                ) {
                    continue;
                }
                let contents = read_text(&path)?;
                let digest = hash(contents.as_bytes());
                fingerprint.update(path.to_string_lossy().as_bytes());
                fingerprint.update(digest.as_bytes());
                for (i, line) in contents.lines().enumerate() {
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
                json!({"hash":fingerprint,"symbols":results,"heuristic":true,"limitations":"Declarations only; may include constants/comments and miss multiline or method declarations. Not semantic references.","next_cursor":(end<rows.len()).then(||format!("{fingerprint}:{end}"))}),
            )
        }
        "document_audit" => {
            revalidate(s)?;
            let path = output_path(&s.project)?;
            let doc = read_text(&path)?;
            let mut issues = vec![];
            let mut checked = 0;
            let citations = regex::Regex::new(
                r"([\p{L}\p{N}_./@-]+\.[A-Za-z][A-Za-z0-9]*)(:|#L)([0-9]+)(?:[-–]L?([0-9]+))?",
            )?;
            let mut fence: Option<(char, usize)> = None;
            for line in doc.split_inclusive('\n') {
                if cancel.is_cancelled() {
                    bail!("cancelled");
                }
                let trimmed = line.trim_start_matches(' ');
                let marker = trimmed.chars().next().unwrap_or(' ');
                let width = trimmed.chars().take_while(|c| *c == marker).count();
                if line.len() - trimmed.len() <= 3 {
                    if let Some((open, min_width)) = fence {
                        if marker == open
                            && width >= min_width
                            && trimmed[width..].trim().is_empty()
                        {
                            fence = None;
                        }
                        continue;
                    }
                    if (marker == '`' || marker == '~') && width >= 3 {
                        fence = Some((marker, width));
                        continue;
                    }
                }
                if fence.is_some() {
                    continue;
                }
                for c in citations.captures_iter(line) {
                    let before = &line[..c.get(0).unwrap().start()];
                    let token_prefix = before
                        .rsplit(|ch: char| {
                            ch.is_whitespace() || ch == '`' || ch == '(' || ch == '\"'
                        })
                        .next()
                        .unwrap_or("");
                    if token_prefix.contains("://") {
                        continue;
                    }
                    checked += 1;
                    let begin = c[3].parse::<usize>().unwrap_or(0);
                    let end = c
                        .get(4)
                        .map(|v| v.as_str().parse::<usize>().unwrap_or(0))
                        .unwrap_or(begin);
                    let citation_path = if &c[2] == "#L" {
                        path.parent()
                            .unwrap()
                            .join(&c[1])
                            .to_string_lossy()
                            .to_string()
                    } else {
                        c[1].to_string()
                    };
                    let check = read_path(&s.project, &citation_path).and_then(|p| read_text(&p));
                    match check {
                        Ok(contents)
                            if begin > 0 && end >= begin && end <= contents.lines().count() => {}
                        Ok(_) => issues.push(json!({"kind":"citation_range","citation":&c[0]})),
                        Err(e) => issues.push(
                            json!({"kind":"citation_path","citation":&c[0],"error":e.to_string()}),
                        ),
                    }
                }
            }
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
                bail!("invalid_offset");
            }
            let end = (offset + n(args, "limit", 30).clamp(1, 100)).min(issues.len());
            Ok(
                json!({"hash":hash(doc.as_bytes()),"total_lines":doc.lines().count(),"citations_checked":checked,"structural_ok":issues.is_empty(),"semantic_verified":false,"issue_count":issues.len(),"issues":issues[offset..end],"next_offset":(end<issues.len()).then_some(end)}),
            )
        }
        _ => bail!("unsupported_tool"),
    }
}
