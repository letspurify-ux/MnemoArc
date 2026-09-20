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
    let query = text(args, "query")?;
    if query.is_empty() {
        bail!("query required");
    }
    let mode = args["mode"].as_str().unwrap_or("matches");
    let before = n(args, "before", 0);
    let after = n(args, "after", 0);
    if before > 20 || after > 20 {
        bail!("invalid_argument_value: before and after must be between 0 and 20");
    }
    if mode != "matches" && (before != 0 || after != 0) {
        bail!("invalid_argument_value: before/after require mode=matches");
    }
    let mut expression = if args["regex"].as_bool().unwrap_or(false) {
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
        .build()?;
    let mut fingerprint = Sha256::new();
    // Presentation options are part of the cursor identity too: an offset into
    // matching lines cannot be reused as an offset into matching files.
    fingerprint.update(serde_json::to_vec(&json!({
        "expression":expression,"case_sensitive":case_sensitive,
        "mode":mode,"before":before,"after":after,"path_glob":path_glob(args)?
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
    for path in candidate_paths(&s.project, path_glob(args)?, cancel)? {
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
            if !regex.is_match(line) {
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
