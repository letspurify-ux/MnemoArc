//! Deterministic authoring checks, not a proof of browser rendering or content.
//!
//! Markdown parsers repair ragged tables instead of rejecting them. Inspect the
//! original rows inside AST table boundaries before that repair hides data loss.
use comrak::{Arena, Options, nodes::NodeValue, parse_document};
use merman_core::{Engine, Error, ParseOptions};
use serde_json::{Value, json};

const MAX_MERMAID_BYTES: usize = 64 * 1024;
const MAX_MERMAID_BLOCKS: usize = 128;
const PREVIEW_LIMIT: usize = 8;

#[derive(Default)]
pub(super) struct FormatCheck {
    pub issues: Vec<Value>,
    /// Garbled words in document order, also reported as issues.
    pub garbled: Vec<GarbledWord>,
    warnings: Vec<Value>,
    tables_checked: usize,
    mermaid_blocks: usize,
    mermaid_checked: usize,
}

/// A garbled word and where it stands, so an edit can name it by fix_id
/// instead of retyping characters the model keeps getting wrong.
pub(super) struct GarbledWord {
    /// "W" and the first 8 hex digits of the word's hash, with "-2", "-3"
    /// for later copies of the same word. It survives edits elsewhere.
    pub fix_id: String,
    /// One-based document line.
    pub line: usize,
    /// Byte offset of the word in that line, when the source line holds it.
    pub column: Option<usize>,
    pub word: String,
}

impl FormatCheck {
    pub fn summary(&self) -> Value {
        json!({
            "ok":self.issues.is_empty(),
            "markdown_parser":"comrak 0.55",
            "mermaid_parser":"merman-core 0.6",
            "mermaid_baseline":"11.12.3",
            "mermaid_scope":"fenced_blocks",
            "markdown_tables_checked":self.tables_checked,
            "mermaid_blocks":self.mermaid_blocks,
            "mermaid_blocks_checked":self.mermaid_checked,
            "mermaid_blocks_unchecked":self.mermaid_blocks - self.mermaid_checked,
            "issue_count":self.issues.len(),
            "warning_count":self.warnings.len(),
            "warnings":self.warnings.iter().take(PREVIEW_LIMIT).collect::<Vec<_>>(),
            "warnings_truncated":self.warnings.len() > PREVIEW_LIMIT,
            "checks_complete":self.warnings.is_empty(),
            "semantic_verified":false,
            "ui_render_verified":false
        })
    }

    pub fn write_result(&self) -> Value {
        let mut result = self.summary();
        result["issues"] = json!(self.issues.iter().take(PREVIEW_LIMIT).collect::<Vec<_>>());
        result["issues_truncated"] = json!(self.issues.len() > PREVIEW_LIMIT);
        result["guidance"] = json!(
            "Fix Markdown/Mermaid errors in the next edit, and each garbled word with document_edit action=fix_word and its fix_id; document_audit paginates all errors. Warnings identify unchecked syntax or UI extensions. Browser layout and newer Mermaid versions are not checked."
        );
        result
    }
}

pub(super) fn check(doc: &str) -> FormatCheck {
    let mut result = FormatCheck::default();
    // CommonMark recognizes lone CR as a line ending too; keep the line map in
    // agreement with the parser without changing the persisted document.
    let normalized;
    let doc = if doc.contains('\r') {
        normalized = doc.replace("\r\n", "\n").replace('\r', "\n");
        normalized.as_str()
    } else {
        doc
    };
    let lines: Vec<_> = doc.lines().collect();
    let arena = Arena::new();
    let mut options = Options::default();
    options.extension.table = true;
    options.extension.footnotes = true;
    options.extension.strikethrough = true;
    options.extension.front_matter_delimiter = Some("---".into());
    let root = parse_document(&arena, doc, &options);
    let mut engine = None;
    // Literal code and HTML may contain table examples, including multi-line
    // inline code. Those examples must not be mistaken for broken table syntax.
    let mut literal_lines = vec![false; lines.len()];
    for node in root.descendants() {
        let data = node.data.borrow();
        if matches!(data.value, NodeValue::Code(_) | NodeValue::HtmlInline(_)) {
            for line in data.sourcepos.start.line..=data.sourcepos.end.line {
                if let Some(literal) = literal_lines.get_mut(line.saturating_sub(1)) {
                    *literal = true;
                }
            }
        }
    }
    for node in root.descendants() {
        let data = node.data.borrow();
        let start = data.sourcepos.start.line;
        let end = data.sourcepos.end.line;
        match &data.value {
            // Prose only: code spans, code blocks and HTML are other nodes.
            NodeValue::Text(text) => mixed_script_words(
                &mut result,
                &lines,
                start,
                data.sourcepos.start.column,
                text,
            ),
            NodeValue::HtmlInline(html) => {
                if let Some(tag) = unknown_html_tag(html) {
                    result.issues.push(json!({"kind":"raw_html_tag","line":start,"tag":tag,
                        "guidance":"Markdown reads this as an HTML tag and the rendered page drops it: Option<Checkpoint> shows as \"Option\". Put the type or expression in inline code (`Option<Checkpoint>`), or escape the angle brackets as \\< and \\>."}));
                }
            }
            NodeValue::CodeBlock(code) if code.fenced => {
                if !code.closed {
                    result.issues.push(json!({"kind":"unclosed_code_fence","line":start,
                        "guidance":"Close this code block with the same fence marker and at least the opening fence length."}));
                }
                if code
                    .info
                    .split_whitespace()
                    .next()
                    .is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid"))
                {
                    result.mermaid_blocks += 1;
                    if code.literal.len() > MAX_MERMAID_BYTES
                        || result.mermaid_blocks > MAX_MERMAID_BLOCKS
                    {
                        result.warnings.push(json!({"kind":"mermaid_check_limit","line":start,"end_line":end,
                            "guidance":"Diagram not checked: the Rust syntax check accepts at most 64 KiB per block and 128 blocks per document. Inspect this diagram in the UI."}));
                        continue;
                    }
                    if uses_mermaid_ui_extension(&code.literal) {
                        result.warnings.push(json!({"kind":"mermaid_ui_extension_unchecked","line":start,"end_line":end,
                            "guidance":"This diagram uses serialized line breaks or UI math labels. Inspect it in the UI; its transformed source was not validated by the Rust parser."}));
                        continue;
                    }
                    let parser = engine.get_or_insert_with(Engine::new);
                    match parser.parse_diagram_sync(&code.literal, ParseOptions::strict()) {
                        Ok(Some(_)) => result.mermaid_checked += 1,
                        Ok(None) | Err(Error::UnsupportedDiagram { .. }) => {
                            result.warnings.push(json!({"kind":"mermaid_unsupported","line":start,"end_line":end,
                                "guidance":"The Rust parser did not check this diagram. Inspect it with the UI Mermaid renderer."}));
                        }
                        Err(Error::InvalidDirectiveJson { message }) => {
                            // Mermaid.js tolerates/ignores malformed directives
                            // in cases where merman rejects their config JSON.
                            // Do not turn that compatibility gap into a false
                            // document-completion failure.
                            result.warnings.push(json!({"kind":"mermaid_directive_unchecked","line":start,"end_line":end,
                                "message":message.chars().take(1200).collect::<String>(),
                                "guidance":"The Rust parser rejected a configuration directive that Mermaid.js may ignore. Fix the directive or inspect this diagram in the UI; its syntax was not fully checked."}));
                        }
                        Err(error) => {
                            result.mermaid_checked += 1;
                            let message = error.to_string();
                            result.issues.push(json!({"kind":"mermaid_syntax_error","line":start,"end_line":end,
                                "diagram_start_line":start + 1,
                                "message":message.chars().take(1200).collect::<String>(),
                                "message_truncated":message.chars().count() > 1200,
                                "guidance":"Fix the diagram using the parser diagnostic (positions in the message refer to the diagram, not the document). The Rust parser targets Mermaid 11.12.3; check newer syntax in the UI."}));
                        }
                    }
                }
            }
            NodeValue::Table(table) => {
                // The UI has custom math and serialized visualization rules.
                // A plain GFM parser cannot establish those tables' cell bounds.
                if lines[start - 1..end]
                    .iter()
                    .any(|line| uses_table_extension(line))
                {
                    extension_warning(&mut result, start, end);
                    continue;
                }
                result.tables_checked += 1;
                for row in node.children() {
                    let row = row.data.borrow();
                    let line = row.sourcepos.start.line;
                    let raw = source_line(&lines, line, row.sourcepos.start.column);
                    let actual = cells(raw).len();
                    if actual != table.num_columns {
                        result.issues.push(json!({"kind":"markdown_table_columns","line":line,
                            "expected_columns":table.num_columns,"actual_columns":actual,
                            "guidance":"Keep each table row on one physical line with the header's column count. Use <br> within a cell and escape literal pipes as \\|; GFM otherwise pads missing cells or discards extra cells."}));
                    }
                }
            }
            NodeValue::Paragraph => {
                // A mismatched header/delimiter is a paragraph, not a Table AST
                // node. Check only unmistakable pipe-delimiter candidates.
                for line in start..end {
                    if literal_lines[line - 1] && literal_lines[line] {
                        continue;
                    }
                    let header = source_line(
                        &lines,
                        line,
                        if line == start {
                            data.sourcepos.start.column
                        } else {
                            1
                        },
                    );
                    let delimiter = source_line(&lines, line + 1, 1);
                    let header_cells = cells(header);
                    let delimiter_cells = cells(delimiter);
                    if !header.contains('|')
                        || !delimiter.contains('|')
                        || !delimiter.contains('-')
                        || !delimiter_cells.iter().all(|cell| {
                            cell.chars()
                                .all(|c| c.is_ascii_whitespace() || c == '-' || c == ':')
                        })
                    {
                        continue;
                    }
                    if uses_table_extension(header) {
                        extension_warning(&mut result, line, line + 1);
                    } else if !delimiter_cells.iter().all(|cell| is_delimiter(cell)) {
                        result.issues.push(json!({"kind":"markdown_table_delimiter","line":line + 1,
                            "guidance":"Use a nonempty run of hyphens per delimiter cell, with an optional colon at either end."}));
                    } else if header_cells.len() != delimiter_cells.len() {
                        result.issues.push(json!({"kind":"markdown_table_header_columns","line":line + 1,
                            "header_columns":header_cells.len(),"delimiter_columns":delimiter_cells.len(),
                            "guidance":"Give the header and delimiter row the same number of cells; otherwise Markdown renders this as prose."}));
                    }
                }
            }
            _ => {}
        }
    }
    report_garbled_words(&mut result);
    result
}

/// Korean words that run straight into another script, such as "배치末尾로",
/// "인프รา" or "있으면それを": models garble a word into a neighbouring
/// language's characters. In 66 Korean live-run documents from four models
/// every such word in prose was garbled; intended foreign text is set apart
/// with parentheses, quotes or code, so it never touches the Korean word.
fn mixed_script_words(
    result: &mut FormatCheck,
    lines: &[&str],
    line: usize,
    column: usize,
    text: &str,
) {
    let source = lines.get(line.wrapping_sub(1)).copied().unwrap_or("");
    // Look for each word in the source line from this text node on, so the
    // same word earlier on the line (in code, say) is not taken for it.
    let mut from = column.saturating_sub(1).min(source.len());
    while !source.is_char_boundary(from) {
        from -= 1;
    }
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        if !word.chars().any(is_hangul) || !word.chars().any(|c| other_script(c).is_some()) {
            continue;
        }
        let at = source[from..]
            .find(word)
            .map(|offset| from + offset)
            .or_else(|| source.find(word));
        if let Some(at) = at {
            from = at + word.len();
        }
        result.garbled.push(GarbledWord {
            fix_id: String::new(),
            line,
            column: at,
            word: word.into(),
        });
    }
}

/// Name each garbled word and report it as an issue the model can fix by
/// that name.
fn report_garbled_words(result: &mut FormatCheck) {
    let mut copies = std::collections::BTreeMap::<String, usize>::new();
    for garbled in &mut result.garbled {
        let base = format!("W{}", &super::hash(garbled.word.as_bytes())[..8]);
        let copy = copies.entry(base.clone()).or_default();
        *copy += 1;
        garbled.fix_id = if *copy == 1 {
            base
        } else {
            format!("{base}-{copy}")
        };
        let mut scripts: Vec<&str> = garbled.word.chars().filter_map(other_script).collect();
        scripts.sort_unstable();
        scripts.dedup();
        let mut issue = json!({"kind":"mixed_script_word","line":garbled.line,"word":garbled.word,"scripts":scripts,
            "guidance":"This Korean word runs into characters of another script, which is a garbled word. Replace it with document_edit action=fix_word, this fix_id and text set to the whole word rewritten correctly; the garbled characters need not be copied. If the foreign text is intended, set it apart from the Korean text with parentheses or quotes, or put it in inline code."});
        if garbled.column.is_some() {
            issue["fix_id"] = json!(garbled.fix_id);
        }
        result.issues.push(issue);
    }
}

/// HTML elements a document may write inline, such as <br> in a table cell.
const HTML_ELEMENTS: &[&str] = &[
    "a",
    "abbr",
    "b",
    "bdi",
    "bdo",
    "big",
    "blockquote",
    "br",
    "center",
    "cite",
    "code",
    "dd",
    "del",
    "details",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "font",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "mark",
    "ol",
    "p",
    "picture",
    "pre",
    "q",
    "rp",
    "rt",
    "ruby",
    "s",
    "samp",
    "small",
    "source",
    "span",
    "strike",
    "strong",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "time",
    "tr",
    "tt",
    "u",
    "ul",
    "var",
    "video",
    "wbr",
];

/// An inline tag whose name is no HTML element, such as the <Checkpoint> of
/// Option<Checkpoint> or the <u64> of Vec<u64>: a type written as prose that
/// the rendered page drops. Comments, declarations and known elements pass.
fn unknown_html_tag(html: &str) -> Option<String> {
    let name: String = html
        .strip_prefix("</")
        .or_else(|| html.strip_prefix('<'))?
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let known = |name: &str| HTML_ELEMENTS.contains(&name);
    let element = known(&name)
        || (name.chars().all(|c| !c.is_ascii_lowercase()) && known(&name.to_ascii_lowercase()));
    (!name.is_empty() && !element).then(|| html.chars().take(80).collect())
}

fn is_hangul(c: char) -> bool {
    matches!(c, '\u{AC00}'..='\u{D7A3}' | '\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}')
}

/// Scripts that do not belong inside a Korean word. Latin, digits and Greek
/// do (API를, 2단계, λ식), so they are not listed.
fn other_script(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{20000}'..='\u{2FA1F}' => "Han",
        '\u{3040}'..='\u{30FF}' | '\u{31F0}'..='\u{31FF}' | '\u{FF66}'..='\u{FF9D}' => "Kana",
        '\u{0E00}'..='\u{0E7F}' => "Thai",
        '\u{0400}'..='\u{04FF}' => "Cyrillic",
        '\u{0590}'..='\u{05FF}' => "Hebrew",
        '\u{0600}'..='\u{06FF}' => "Arabic",
        '\u{0900}'..='\u{097F}' => "Devanagari",
        _ => return None,
    })
}

fn extension_warning(result: &mut FormatCheck, line: usize, end_line: usize) {
    result.warnings.push(json!({"kind":"markdown_ui_extension_unchecked","line":line,"end_line":end_line,
        "guidance":"This table uses UI math or serialized chart/Mermaid syntax. Its columns and embedded diagrams require UI inspection; they were not validated by the GFM check."}));
}

fn uses_mermaid_ui_extension(source: &str) -> bool {
    source.contains("\\n")
        || source.contains("$$")
        || source.match_indices("\\r").any(|(offset, _)| {
            source[offset + 2..]
                .chars()
                .next()
                .is_none_or(|next| !next.is_ascii_alphabetic())
        })
}

fn source_line<'a>(lines: &[&'a str], line: usize, column: usize) -> &'a str {
    let source = lines[line - 1];
    let mut source = source
        .get(column.saturating_sub(1)..)
        .unwrap_or(source)
        .trim();
    // Source positions are bytes; container prefixes on continuation lines are
    // still present when checking a paragraph's would-be delimiter row.
    while let Some(rest) = source.strip_prefix('>') {
        source = rest.trim_start();
    }
    source
}

fn cells(row: &str) -> Vec<&str> {
    let row = row.trim();
    let mut cells = Vec::new();
    let mut start = 0;
    let mut escaped = false;
    for (i, byte) in row.bytes().enumerate() {
        if byte == b'|' && !escaped {
            cells.push(&row[start..i]);
            start = i + 1;
        }
        escaped = byte == b'\\' && !escaped;
    }
    cells.push(&row[start..]);
    if row.starts_with('|') {
        cells.remove(0);
    }
    if cells.last().is_some_and(|cell| cell.is_empty()) && row.ends_with('|') && start == row.len()
    {
        cells.pop();
    }
    cells
}

fn is_delimiter(cell: &str) -> bool {
    let cell = cell.trim();
    let cell = cell.strip_prefix(':').unwrap_or(cell);
    let cell = cell.strip_suffix(':').unwrap_or(cell);
    !cell.is_empty() && cell.bytes().all(|byte| byte == b'-')
}

fn uses_table_extension(line: &str) -> bool {
    // Conservative coverage boundary, not an implementation of the UI's
    // extended Markdown grammar. Avoid false errors on rich table payloads.
    let lower = line.to_ascii_lowercase();
    let visualization = ["mermaid", "chart"].iter().any(|lang| {
        [
            format!("{lang}\\n"),
            format!("{lang}\\r\\n"),
            format!("{lang}<br"),
            format!("{lang}\\<br"),
        ]
        .iter()
        .any(|prefix| lower.contains(prefix))
    });
    let math_pipe = [("$$", "$$"), ("$", "$"), ("\\(", "\\)"), ("\\[", "\\]")]
        .iter()
        .any(|(open, close)| {
            let mut remaining = line;
            while let Some((_, body)) = remaining.split_once(open) {
                let Some((body, rest)) = body.split_once(close) else {
                    break;
                };
                if body.contains('|') {
                    return true;
                }
                remaining = rest;
            }
            false
        })
        || (line.contains("\\begin{") && line.contains("\\end{"));
    visualization || math_pipe
}
