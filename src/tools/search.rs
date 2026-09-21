use super::*;

struct Match {
    path: PathBuf,
    hash: String,
    line: usize,
    text: String,
    truncated: bool,
    context: Vec<Value>,
}

fn displayed_line(line: usize, text: &str) -> Value {
    json!({"line":line,"text":text.chars().take(500).collect::<String>(),"truncated":text.chars().count()>500})
}

pub(super) fn execute(
    s: &mut Session,
    args: &Value,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    if args.get("query").is_some() == args.get("queries").is_some() {
        bail!(
            "conflicting_arguments: supply exactly one of query (literal by default) or queries (literal OR)"
        );
    }
    let query = args["query"].as_str().unwrap_or("");
    let terms = args["queries"].as_array();
    if let Some(terms) = terms {
        if terms.is_empty()
            || terms.len() > 16
            || terms.iter().any(|t| t.as_str().is_none_or(str::is_empty))
        {
            bail!("invalid_argument_value: queries requires 1 to 16 nonempty literal strings");
        }
        if args["regex"] == true {
            bail!("conflicting_arguments: queries is literal OR; use query for regex");
        }
    } else if query.is_empty() {
        bail!("query required");
    }
    if args.get("path").is_some()
        && (args.get("path_glob").is_some() || args.get("pattern").is_some())
    {
        bail!(
            "conflicting_arguments: use path for one exact file OR path_glob/pattern for a file glob"
        );
    }
    let exact_path = args["path"]
        .as_str()
        .map(|p| read_path(&s.project, p))
        .transpose()?;
    let mode = args["mode"].as_str().unwrap_or("matches");
    let before = n(args, "before", 0);
    let after = n(args, "after", 0);
    if before > 20 || after > 20 {
        bail!("invalid_argument_value: before and after must be between 0 and 20");
    }
    if mode != "matches" && (before != 0 || after != 0) {
        bail!("invalid_argument_value: before/after require mode=matches");
    }
    let mut expression = if let Some(terms) = terms {
        format!(
            "(?:{})",
            terms
                .iter()
                .map(|t| regex::escape(t.as_str().unwrap()))
                .collect::<Vec<_>>()
                .join("|")
        )
    } else if args["regex"].as_bool().unwrap_or(false) {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    let whole_word = args["whole_word"].as_bool().unwrap_or(false);
    if whole_word {
        expression = format!(r"\b(?:{expression})\b");
    }
    let case_sensitive = args["case_sensitive"].as_bool().unwrap_or(true);
    let regex = regex::RegexBuilder::new(&expression)
        .case_insensitive(!case_sensitive)
        .size_limit(1024 * 1024)
        .build().map_err(|e| anyhow::anyhow!("invalid_search_regex: {e}; no search executed. For literal code such as .on(, retry with regex:false; for literal alternatives use queries. Keep the same file scope."))?;
    let mut fingerprint = Sha256::new();
    // Presentation options are part of the cursor identity too: an offset into
    // matching lines cannot be reused as an offset into matching files.
    fingerprint.update(serde_json::to_vec(&json!({
        "expression":expression,"case_sensitive":case_sensitive,
        "mode":mode,"before":before,"after":after,"path_glob":path_glob(args)?,"path":exact_path
    }))?);
    let requested_offset = if let Some(cursor) = args["cursor"].as_str() {
        cursor
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("invalid_cursor"))?
            .1
            .parse::<usize>()?
    } else {
        0
    };
    let requested_end = requested_offset.saturating_add(n(args, "limit", 20).clamp(1, 100));
    let mut rows = Vec::new();
    let mut files = Vec::new();
    let mut matched_files = 0;
    let mut matching_file_count = 0;
    let mut total_matching_lines = 0usize;
    let candidates = match exact_path {
        Some(path) => vec![path],
        None => candidate_paths(&s.project, path_glob(args)?, cancel)?,
    };
    for path in candidates {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        let Some(contents) = search_text(&path)? else {
            continue;
        };
        matched_files += 1;
        let digest = hash(contents.as_bytes());
        fingerprint.update(serde_json::to_vec(&(path.to_string_lossy(), &digest))?);
        let mut matching_lines = 0usize;
        let mut byte_offset = 0;
        for (i, chunk) in contents.split_inclusive('\n').enumerate() {
            let line = chunk
                .strip_suffix('\n')
                .map(|line| line.strip_suffix('\r').unwrap_or(line))
                .unwrap_or(chunk);
            let line_start = byte_offset;
            byte_offset += chunk.len();
            if i % 256 == 0 && cancel.is_cancelled() {
                bail!("cancelled");
            }
            // A blank or whitespace-only line cannot provide the non-empty
            // excerpt required for source evidence, so do not expose it as a
            // searchable match even when a regex can match an empty string.
            if line.trim().is_empty() || !regex.is_match(line) {
                continue;
            }
            matching_lines += 1;
            total_matching_lines += 1;
            let index = total_matching_lines - 1;
            if mode != "matches" || index < requested_offset || index >= requested_end {
                continue;
            }
            let mut context = Vec::new();
            for (distance, text) in contents[..line_start]
                .lines()
                .rev()
                .take(before)
                .enumerate()
            {
                context.push(displayed_line(i - distance, text));
            }
            context.reverse();
            for (distance, text) in contents[byte_offset..].lines().take(after).enumerate() {
                context.push(displayed_line(i + distance + 2, text));
            }
            rows.push(Match {
                path: path.clone(),
                hash: digest.clone(),
                line: i + 1,
                text: line.chars().take(500).collect(),
                truncated: line.chars().count() > 500,
                context,
            });
        }
        if matching_lines > 0 {
            if mode != "matches" {
                let index = matching_file_count;
                if index >= requested_offset && index < requested_end {
                    files.push(json!({"path":path,"hash":digest,"matching_lines":matching_lines}));
                }
            }
            matching_file_count += 1;
        }
    }
    let fingerprint = format!("{:x}", fingerprint.finalize());
    let offset = page_cursor(args, &fingerprint)?;
    let total = if mode == "matches" {
        total_matching_lines
    } else {
        matching_file_count
    };
    if offset > total {
        bail!("invalid_cursor");
    }
    let end = offset
        .saturating_add(n(args, "limit", 20).clamp(1, 100))
        .min(total);
    let mut output = json!({
        "hash":fingerprint,"mode":mode,"matched_files":matched_files,
        "matching_files":matching_file_count,"total_matching_lines":total_matching_lines,
        "next_cursor":(end<total).then(||format!("{fingerprint}:{end}"))
    });
    if total == 0 {
        output["empty_reason"] = json!(if matched_files == 0 {
            "no_searchable_files"
        } else {
            "no_matching_lines"
        });
        let mut guidance = if matched_files == 0 {
            "No searchable text files matched the scope. Check path_glob with file_list mode=paths; a known basename can use **/filename. Excluded, binary and oversized files are not searched."
        } else {
            "Files were searched but the query did not match. Keep the file scope and try a shorter known identifier or route; do not guess call syntax, receiver names or quote style. Use before/after for nearby navigation context, then file_read for evidence."
        }.to_owned();
        if matched_files > 0 && !args["regex"].as_bool().unwrap_or(false) && query.contains('|') {
            guidance.push_str(" This query was literal: | does not mean OR. For literal alternatives put the intended strings in the queries array. Use regex:true only for intentional regex syntax. Literal matching has not been changed automatically.");
        }
        output["guidance"] = json!(guidance);
    }
    if mode == "matches" {
        output["matches"] = json!(rows.iter().map(|row| {
            // Context is navigation help. The source observation attests only
            // the matching line, preserving the existing evidence contract.
            let source = observe_hashed(s, &row.path, row.hash.clone(), row.line, row.line, &row.text);
            let mut result = json!({"path":row.path,"line":row.line,"text":row.text,"truncated":row.truncated,"source":source});
            if before > 0 || after > 0 {
                result["context"] = json!(row.context);
            }
            result
        }).collect::<Vec<_>>());
    } else if mode == "files" {
        output["files"] = json!(files.iter().map(|file| &file["path"]).collect::<Vec<_>>());
    } else {
        output["counts"] = json!(&files);
    }
    Ok(output)
}
