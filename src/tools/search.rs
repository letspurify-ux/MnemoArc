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
    json!({"line":line,"text":text.chars().take(500).collect::<String>(),"truncated":text.chars().nth(500).is_some()})
}

/// A searched text file at one version and how many of its lines the
/// expression matches. Its digest is the digest cache's.
struct ScannedFile {
    version: FileVersion,
    matching_lines: usize,
}

/// A walked file's digest and matching-line count from an earlier scan.
struct Scanned {
    digest: String,
    matching_lines: usize,
}

/// Recent search expressions, each with the files a search of it scanned.
/// Every page of a search read and matched every file in scope again (a
/// 20,000-file scope took 360ms a page, 38ms of it the walk); a file at a
/// version scanned before is read only when its matches fall on the page.
/// Versions follow the digest cache's rules (settled, unchanged across the
/// read).
#[derive(Default)]
struct ScanCache {
    scans: std::collections::VecDeque<(String, std::collections::HashMap<PathBuf, ScannedFile>)>,
    entries: usize,
}

const SCAN_CACHE_EXPRESSIONS: usize = 8;
/// Files kept across all expressions, as many as one scan can list.
const SCAN_CACHE_ENTRIES: usize = 100_000;

fn scan_cache() -> &'static std::sync::Mutex<ScanCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<ScanCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The scan of `path` an earlier search of `key` made at its current version.
fn scanned(key: &str, path: &Path) -> Option<Scanned> {
    let version = super::file_version(&path.metadata().ok()?).ok()?;
    let matching_lines = scan_cache()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .scans
        .iter()
        .find(|(known, _)| known == key)?
        .1
        .get(path)
        .filter(|file| file.version == version)?
        .matching_lines;
    Some(Scanned {
        digest: super::cached_digest(path, version, |entry| entry.hash.clone())?,
        matching_lines,
    })
}

fn remember_scan(key: &str, path: &Path, file: ScannedFile) {
    if !super::settled(&file.version) {
        return;
    }
    let mut cache = scan_cache().lock().unwrap_or_else(|p| p.into_inner());
    let ScanCache { scans, entries } = &mut *cache;
    if !scans.iter().any(|(known, _)| known == key) {
        if scans.len() >= SCAN_CACHE_EXPRESSIONS
            && let Some((_, dropped)) = scans.pop_front()
        {
            *entries -= dropped.len();
        }
        scans.push_back((key.to_owned(), Default::default()));
    }
    let position = |scans: &std::collections::VecDeque<(String, _)>| {
        scans.iter().position(|(known, _)| known == key).unwrap()
    };
    // Room for a new file is made by dropping the oldest other expression,
    // or this one's files when it is the only one.
    while *entries >= SCAN_CACHE_ENTRIES && !scans[position(scans)].1.contains_key(path) {
        let oldest = if position(scans) == 0 { 1 } else { 0 };
        let dropped = if oldest < scans.len() {
            scans.remove(oldest).unwrap().1.len()
        } else {
            std::mem::take(&mut scans[0].1).len()
        };
        *entries -= dropped;
    }
    let index = position(scans);
    if scans[index].1.insert(path.to_path_buf(), file).is_none() {
        *entries += 1;
    }
}

/// A matcher of the search expression over a whole file that matches
/// wherever one of its lines matches, `^` and `$` matching at line ends: a
/// file it does not match has no matching line, and a file it matches is
/// checked only at the lines its matches start on. A 20,000-file scope spent
/// most of a search calling the line matcher once per line. An expression
/// tied to the text's own start or end (`\A`, `\z`, or `^`/`$` with
/// multi-line mode turned off) has no such matcher.
struct WholeText {
    regex: regex::Regex,
    /// `$` meets lines whose `\r\n` the line matcher drops but the whole
    /// text keeps, and CRLF mode would change what `.` matches, so a file
    /// holding `\r` is checked line by line.
    line_end: bool,
    /// Over non-ASCII text a Unicode word boundary makes the fast engines
    /// give up, and a whole file would be scanned by the slowest one.
    word_boundary: bool,
}

impl WholeText {
    fn applies(&self, contents: &str) -> bool {
        (!self.line_end || !contents.contains('\r')) && (!self.word_boundary || contents.is_ascii())
    }
}

fn whole_text_matcher(expression: &str, case_sensitive: bool) -> Option<WholeText> {
    use regex_syntax::hir::Look;
    let hir = regex_syntax::ParserBuilder::new()
        .multi_line(true)
        .case_insensitive(!case_sensitive)
        .build()
        .parse(expression)
        .ok()?;
    let looks = hir.properties().look_set();
    let any = |wanted: &[Look]| wanted.iter().any(|&look| looks.contains(look));
    if any(&[Look::Start, Look::End, Look::StartCRLF, Look::EndCRLF]) {
        return None;
    }
    let regex = regex::RegexBuilder::new(&within_lines(hir).to_string())
        .size_limit(1024 * 1024)
        .build()
        .ok()?;
    Some(WholeText {
        regex,
        line_end: any(&[Look::EndLF]),
        word_boundary: any(&[
            Look::WordUnicode,
            Look::WordUnicodeNegate,
            Look::WordStartUnicode,
            Look::WordEndUnicode,
            Look::WordStartHalfUnicode,
            Look::WordEndHalfUnicode,
        ]),
    })
}

/// `hir` with no class matching '\n'. No line holds one, and a match that
/// crossed lines (`\s+` running into the next) was a candidate the line
/// matcher then rejected, making the search slower than line by line.
fn within_lines(hir: regex_syntax::hir::Hir) -> regex_syntax::hir::Hir {
    use regex_syntax::hir::{
        Capture, Class, ClassBytes, ClassBytesRange, ClassUnicode, ClassUnicodeRange, Hir, HirKind,
        Repetition,
    };
    match hir.into_kind() {
        HirKind::Class(Class::Unicode(mut class)) => {
            class.difference(&ClassUnicode::new([ClassUnicodeRange::new('\n', '\n')]));
            Hir::class(Class::Unicode(class))
        }
        HirKind::Class(Class::Bytes(mut class)) => {
            class.difference(&ClassBytes::new([ClassBytesRange::new(b'\n', b'\n')]));
            Hir::class(Class::Bytes(class))
        }
        HirKind::Repetition(repetition) => Hir::repetition(Repetition {
            sub: Box::new(within_lines(*repetition.sub)),
            ..repetition
        }),
        HirKind::Capture(capture) => Hir::capture(Capture {
            sub: Box::new(within_lines(*capture.sub)),
            ..capture
        }),
        HirKind::Concat(parts) => Hir::concat(parts.into_iter().map(within_lines).collect()),
        HirKind::Alternation(parts) => {
            Hir::alternation(parts.into_iter().map(within_lines).collect())
        }
        HirKind::Empty => Hir::empty(),
        // A '\n' in a literal never matches a line either. Kept, `(?:.*\n)+x`
        // matched from each line to the end of the file and every next line
        // was searched again from there: a 200KB file took 1.4s to find no
        // matching line.
        HirKind::Literal(literal) if literal.0.contains(&b'\n') => Hir::fail(),
        HirKind::Literal(literal) => Hir::literal(literal.0),
        HirKind::Look(look) => Hir::look(look),
    }
}

/// The lines of `contents` a search checks, as (index, start, end) with
/// `end` past the line's '\n': every line, or with a whole-text matcher only
/// the lines its matches start on, in order.
fn candidate_lines<'a>(
    contents: &'a str,
    whole: Option<&'a WholeText>,
) -> impl Iterator<Item = (usize, usize, usize)> + 'a {
    let whole = whole
        .filter(|whole| whole.applies(contents))
        .map(|whole| &whole.regex);
    let (mut position, mut index) = (0, 0);
    std::iter::from_fn(move || {
        if position >= contents.len() {
            return None;
        }
        let mut start = position;
        if let Some(whole) = whole {
            let found = whole.find_at(contents, position)?.start();
            let skipped = &contents[position..found];
            if let Some(last) = skipped.rfind('\n') {
                index += skipped.bytes().filter(|&byte| byte == b'\n').count();
                start = position + last + 1;
            }
        }
        let end = contents[start..]
            .find('\n')
            .map_or(contents.len(), |newline| start + newline + 1);
        let line = (index, start, end);
        (position, index) = (end, index + 1);
        Some(line)
    })
}

/// Models often send the search text as `pattern`, which is the legacy
/// file-glob alias of path_glob. Without this, a bare "missing query" error
/// left a live run retrying the same call and then avoiding the tool.
fn pattern_hint(args: &Value) -> String {
    let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
        return "; send query as one nonempty search string, or queries as an array of 1 to 16 literal strings".into();
    };
    let shown: String = pattern.chars().take(80).collect();
    format!(
        "; pattern {shown:?} is a file glob that filters paths (legacy alias of path_glob), not the text to find; send the text as query (literal by default, regex:true for a regular expression){}",
        if args.get("path").is_some() {
            " and drop pattern, since path already sets the scope"
        } else {
            ""
        }
    )
}

pub(super) fn execute(
    s: &mut Session,
    args: &Value,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    // Providers that fill every field send an empty queries list or query
    // string beside the real one; an empty alternative is not a conflict.
    let mut args = args.clone();
    let mut joined = None;
    if let Some(object) = args.as_object_mut() {
        if object
            .get("query")
            .is_some_and(|q| q.as_str().is_some_and(str::is_empty))
            && object.contains_key("queries")
        {
            object.remove("query");
        }
        if object
            .get("queries")
            .is_some_and(|q| q.as_array().is_some_and(Vec::is_empty))
            && object.contains_key("query")
        {
            object.remove("queries");
        }
        // A literal query repeated inside queries asks for nothing new: a
        // live run sent query:"표로 보기" with queries:["표로 보기"] six times.
        if object.get("regex") != Some(&json!(true))
            && let (Some(query), Some(queries)) = (
                object.get("query").and_then(Value::as_str),
                object.get("queries").and_then(Value::as_array),
            )
            && queries.iter().any(|term| term.as_str() == Some(query))
        {
            object.remove("query");
        }
        // A different literal query is one more alternative of the literal
        // OR, the call the conflict error asked for: a live run's query
        // "chat" beside four other queries was refused and cost a request.
        if object.get("regex") != Some(&json!(true))
            && let (Some(query), Some(queries)) = (
                object.get("query").and_then(Value::as_str),
                object.get("queries").and_then(Value::as_array),
            )
            && !query.is_empty()
            && (1..16).contains(&queries.len())
            && queries
                .iter()
                .all(|term| term.as_str().is_some_and(|term| !term.is_empty()))
        {
            let terms: Vec<Value> = std::iter::once(json!(query))
                .chain(queries.iter().cloned())
                .collect();
            object.insert("queries".into(), json!(terms));
            object.remove("query");
            joined = Some(Value::Array(terms));
        }
    }
    let args = &args;
    match (args.get("query").is_some(), args.get("queries").is_some()) {
        (false, false) => bail!("missing_argument: query or queries{}", pattern_hint(args)),
        (true, true) => {
            // Name both values and the call that keeps every term, so the
            // retry is not the same rejected pair.
            let mut merged: Vec<Value> = args["queries"].as_array().cloned().unwrap_or_default();
            merged.insert(0, args["query"].clone());
            bail!(
                "conflicting_arguments: received both query {} and queries {}; send only one. For a literal OR over all of them send queries:{} without query; for a regular expression send query alone with regex:true",
                args["query"],
                args["queries"],
                Value::Array(merged)
            )
        }
        _ => {}
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
        bail!("missing_argument: query or queries{}", pattern_hint(args));
    }
    if args.get("path").is_some()
        && (args.get("path_glob").is_some() || args.get("pattern").is_some())
    {
        // pattern sent beside path is usually meant as the text to find (a
        // live run sent path, pattern:"async fn" and query together), so say
        // where that text belongs.
        let query_field = if args.get("path_glob").is_none() && args.get("pattern").is_some() {
            "; the text to find goes in query"
        } else {
            ""
        };
        bail!(
            "conflicting_arguments: use path for one exact file or directory OR path_glob/pattern for a file glob{}{query_field}",
            super::path_glob_conflict_hint(s, args)
        );
    }
    let mut exact_path = args["path"]
        .as_str()
        .map(|p| read_path_suggesting(&s.project, p))
        .transpose()?;
    // `path` is literal even for directories such as Next.js `[slug]` routes.
    // Comparing path components avoids turning their names into glob syntax.
    let directory = if exact_path.as_ref().is_some_and(|path| path.is_dir()) {
        exact_path.take()
    } else {
        None
    };
    let scope_glob = path_glob(args)?;
    let mode = args["mode"].as_str().unwrap_or("matches");
    let before = n(args, "before", 0);
    let after = n(args, "after", 0);
    if before > 20 || after > 20 {
        bail!("invalid_argument_value: before and after must be between 0 and 20");
    }
    if mode != "matches" && (before != 0 || after != 0) {
        bail!("invalid_argument_value: before/after require mode=matches");
    }
    let whole_word = args["whole_word"].as_bool().unwrap_or(false);
    // `\b` beside punctuation needs a word character on its other side, so
    // a whole-word `load()` never matched `load();`. A literal term gets a
    // boundary only on a side that ends in a word character.
    let literal = |term: &str| {
        let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        let boundary = |at_word: bool| if whole_word && at_word { r"\b" } else { "" };
        format!(
            "{}{}{}",
            boundary(word(term.chars().next())),
            regex::escape(term),
            boundary(word(term.chars().next_back()))
        )
    };
    let expression = if let Some(terms) = terms {
        format!(
            "(?:{})",
            terms
                .iter()
                .map(|t| literal(t.as_str().unwrap()))
                .collect::<Vec<_>>()
                .join("|")
        )
    } else if args["regex"].as_bool().unwrap_or(false) {
        if whole_word {
            // A whole-word match has no word character beside it, as grep -w
            // reads it: `\b` around a pattern ending in punctuation needs a
            // word character on its far side, so `load\(\)` never matched
            // `load();`. Only whether a line matches is used, so the
            // neighbours may be part of the match.
            format!(r"(?:^|\W)(?:{query})(?:$|\W)")
        } else {
            query.to_owned()
        }
    } else {
        literal(query)
    };
    let case_sensitive = args["case_sensitive"].as_bool().unwrap_or(true);
    let regex = regex::RegexBuilder::new(&expression)
        .case_insensitive(!case_sensitive)
        .size_limit(1024 * 1024)
        .build().map_err(|e| anyhow::anyhow!("invalid_search_regex: {e}; no search executed. For literal code such as .on(, retry with regex:false; for literal alternatives use queries. Keep the same file scope."))?;
    let whole = whole_text_matcher(&expression, case_sensitive);
    let mut fingerprint = Sha256::new();
    // Presentation options are part of the cursor identity too: an offset into
    // matching lines cannot be reused as an offset into matching files.
    fingerprint.update(serde_json::to_vec(&json!({
        "expression":expression,"case_sensitive":case_sensitive,
        "mode":mode,"before":before,"after":after,"path_glob":scope_glob,"path":exact_path,"directory":directory
    }))?);
    let requested_offset = if let Some(cursor) = args["cursor"].as_str() {
        cursor
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!(INVALID_CURSOR))?
            .1
            .parse::<usize>()
            .map_err(|_| anyhow::anyhow!(INVALID_CURSOR))?
    } else {
        0
    };
    let mut requested_end = requested_offset.saturating_add(n(args, "limit", 20).clamp(1, 100));
    // A page holds about eight result budgets of text at most: a hundred
    // matches with twenty context lines each made a 2MB page that the result
    // limit then counted and cut for seconds, leaving no match in it. The
    // cursor continues after the last match kept.
    let page_bytes = s.config.result_budget().saturating_mul(32).max(16 * 1024);
    let mut kept_bytes = 0usize;
    let mut rows = Vec::new();
    let mut files = Vec::new();
    let mut matched_files = 0;
    let mut matching_file_count = 0;
    let mut total_matching_lines = 0usize;
    let named = exact_path.is_some();
    let candidates = match exact_path {
        // A named file that cannot be read is an error, not an empty
        // result: only walked files are skipped when unreadable.
        Some(path) => {
            super::open_regular_file(&path)?;
            vec![path]
        }
        None => candidate_paths_scoped(&s.project, scope_glob, cancel, directory.as_deref(), None)?,
    };
    // What a file's scan depends on: the expression and its case.
    let scan_key = hash(&serde_json::to_vec(
        &json!({"expression":expression,"case_sensitive":case_sensitive}),
    )?);
    for path in candidates {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        // A walked file scanned before at this version is read again only
        // for the lines this page shows.
        if !named && let Some(scanned) = scanned(&scan_key, &path) {
            let first = total_matching_lines;
            let count = scanned.matching_lines;
            let on_page = mode == "matches"
                && count > 0
                && first < requested_end
                && first + count > requested_offset;
            if !on_page {
                matched_files += 1;
                fingerprint.update(serde_json::to_vec(&(
                    path.to_string_lossy(),
                    &scanned.digest,
                ))?);
                total_matching_lines += count;
                if count > 0 {
                    let index = matching_file_count;
                    if mode != "matches" && index >= requested_offset && index < requested_end {
                        files.push(
                            json!({"path":path,"hash":scanned.digest,"matching_lines":count}),
                        );
                    }
                    matching_file_count += 1;
                }
                continue;
            }
        }
        let (text, version) = if named {
            (super::named_text_hashed(&path)?, None)
        } else {
            match super::walked_text(&path)? {
                Some((text, version)) => {
                    (Some(super::with_digest(&path, (text, version))), version)
                }
                None => (None, None),
            }
        };
        let Some((contents, digest)) = text else {
            continue;
        };
        matched_files += 1;
        fingerprint.update(serde_json::to_vec(&(path.to_string_lossy(), &digest))?);
        let mut matching_lines = 0usize;
        for (checked, (i, line_start, byte_offset)) in
            candidate_lines(&contents, whole.as_ref()).enumerate()
        {
            let chunk = &contents[line_start..byte_offset];
            let line = chunk
                .strip_suffix('\n')
                .map(|line| line.strip_suffix('\r').unwrap_or(line))
                .unwrap_or(chunk);
            if checked % 256 == 0 && cancel.is_cancelled() {
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
            let text: String = line.chars().take(500).collect();
            kept_bytes = kept_bytes
                .saturating_add(text.len())
                .saturating_add(
                    context
                        .iter()
                        .map(|line| line["text"].as_str().map_or(0, str::len))
                        .sum(),
                )
                .saturating_add(path.as_os_str().len() + 300);
            if kept_bytes >= page_bytes {
                requested_end = index + 1;
            }
            rows.push(Match {
                path: path.clone(),
                hash: digest.clone(),
                line: i + 1,
                truncated: line.chars().nth(500).is_some(),
                text,
                context,
            });
        }
        if let Some(version) = version {
            remember_scan(
                &scan_key,
                &path,
                ScannedFile {
                    version,
                    matching_lines,
                },
            );
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
        bail!(INVALID_CURSOR);
    }
    let end = requested_end.min(total);
    let mut output = json!({
        "hash":fingerprint,"mode":mode,"matched_files":matched_files,
        "matching_files":matching_file_count,"total_matching_lines":total_matching_lines,
        "next_cursor":(end<total).then(||format!("{fingerprint}:{end}"))
    });
    if let Some(terms) = joined {
        output["notice"] = json!(format!(
            "query and queries were searched together as one literal OR, queries:{terms}. Send several literal terms in queries alone, or query alone with regex:true for a regular expression."
        ));
    }
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
            let source = super::observe_hashed_quality(
                s,
                &row.path,
                row.hash.clone(),
                row.line,
                row.line,
                &row.text,
                super::EvidenceQuality {
                    line_start_complete: true,
                    line_end_complete: true,
                    evidence_truncated: row.truncated,
                },
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Matching line numbers as the line-by-line search finds them.
    fn line_by_line(regex: &regex::Regex, contents: &str) -> Vec<usize> {
        contents
            .split_inclusive('\n')
            .enumerate()
            .filter(|(_, chunk)| {
                let line = chunk
                    .strip_suffix('\n')
                    .map(|line| line.strip_suffix('\r').unwrap_or(line))
                    .unwrap_or(chunk);
                !line.trim().is_empty() && regex.is_match(line)
            })
            .map(|(index, _)| index + 1)
            .collect()
    }

    #[test]
    fn whole_text_candidates_find_every_matching_line() {
        let contents = [
            "fn alpha() {}\nlet beta = 1;\r\nalpha\r\n\n  \nend alpha",
            "한글 alpha 단어\nalpha한글\r\n한글alpha\nalpha\n",
            "x\ny\nxy\n\nyx\r\n\r\nx",
            "alpha\n",
            "",
            "\n\n\nalpha",
            "load();\nload()\r\nunload()\nend$\n",
            "a\rb\nalpha\r",
        ];
        let expressions = [
            "alpha",
            "^alpha$",
            "^alpha",
            "alpha$",
            r"\balpha\b",
            r"(?:^|\W)(?:alpha)(?:$|\W)",
            "x*",
            "^$",
            r"\s",
            "y\nx",
            "(?:.*\n)+alpha",
            r"\x0Aalpha|beta",
            "(?:x\n)*y",
            "[^a]",
            "(?s).",
            r"\Aalpha",
            r"alpha\z",
            "(?-m)^alpha",
            "(?-R)alpha$",
            "(?-R)^alpha",
            r"\Bpha",
            r"load\(\)",
            r"\bload\(\)",
            "end\\$$",
            "b$",
            r"\b{start}alpha",
            "(?i)ALPHA",
            "한글$",
        ];
        for expression in expressions {
            for case_sensitive in [true, false] {
                let regex = regex::RegexBuilder::new(expression)
                    .case_insensitive(!case_sensitive)
                    .build()
                    .unwrap();
                let whole = whole_text_matcher(expression, case_sensitive);
                for text in contents {
                    let found: Vec<usize> = candidate_lines(text, whole.as_ref())
                        .filter(|&(_, start, end)| {
                            let chunk = &text[start..end];
                            let line = chunk
                                .strip_suffix('\n')
                                .map(|line| line.strip_suffix('\r').unwrap_or(line))
                                .unwrap_or(chunk);
                            !line.trim().is_empty() && regex.is_match(line)
                        })
                        .map(|(index, _, _)| index + 1)
                        .collect();
                    assert_eq!(
                        found,
                        line_by_line(&regex, text),
                        "{expression:?} case_sensitive={case_sensitive} in {text:?}"
                    );
                }
            }
        }
        // Generated texts and expressions over line ends, boundaries and
        // non-ASCII letters.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as usize
        };
        let letters = ["a", "b", " ", "\n", "\r", "\r\n", "é", "한", "_", "("];
        let pieces = [
            "a",
            "b",
            "^",
            "$",
            r"\b",
            r"\B",
            " ",
            r"\s",
            ".",
            "a*",
            "[^b]",
            "é",
            "한",
            r"\(",
            "(?:a|b)",
            r"\w",
            r"\W",
            r"\r",
            r"\n",
            "(?s)",
            "(?R)",
            "(?-m)",
            "(?i)",
            r"\A",
            r"\z",
            "(?-u:\\b)",
            "[a-z]+",
        ];
        for _ in 0..3000 {
            let text: String = (0..next(24))
                .map(|_| letters[next(letters.len())])
                .collect();
            let expression: String = (0..1 + next(4))
                .map(|_| pieces[next(pieces.len())])
                .collect();
            let case_sensitive = next(2) == 0;
            let Ok(regex) = regex::RegexBuilder::new(&expression)
                .case_insensitive(!case_sensitive)
                .build()
            else {
                continue;
            };
            let whole = whole_text_matcher(&expression, case_sensitive);
            let found: Vec<usize> = candidate_lines(&text, whole.as_ref())
                .filter(|&(_, start, end)| {
                    let chunk = &text[start..end];
                    let line = chunk
                        .strip_suffix('\n')
                        .map(|line| line.strip_suffix('\r').unwrap_or(line))
                        .unwrap_or(chunk);
                    !line.trim().is_empty() && regex.is_match(line)
                })
                .map(|(index, _, _)| index + 1)
                .collect();
            assert_eq!(
                found,
                line_by_line(&regex, &text),
                "{expression:?} in {text:?}"
            );
        }
        // Ordinary expressions get a whole-text matcher.
        for expression in [
            "alpha",
            r"\balpha\b",
            "^alpha$",
            r"(?:^|\W)(?:alpha)(?:$|\W)",
            r"\s+\w+",
            "(?s).",
            "(?i)ALPHA",
            "한글$",
            r"load\(\)",
        ] {
            for case_sensitive in [true, false] {
                assert!(
                    whole_text_matcher(expression, case_sensitive).is_some(),
                    "{expression}"
                );
            }
        }
        // A literal line break matches no line, so a match spanning lines is
        // no candidate: each one was searched again to the end of the text.
        let long = "alpha\n".repeat(10_000);
        for expression in ["(?:.*\n)+zzz", "(?:alpha\n)+", r"alpha\x0A"] {
            let whole = whole_text_matcher(expression, true).unwrap();
            assert_eq!(
                candidate_lines(&long, Some(&whole)).count(),
                0,
                "{expression}"
            );
        }
        // Expressions tied to the text's own ends are searched line by line.
        for expression in [r"\Aalpha", r"alpha\z", "(?-m)^alpha", "(?R)alpha$"] {
            assert!(
                whole_text_matcher(expression, true).is_none(),
                "{expression}"
            );
        }
    }
}
