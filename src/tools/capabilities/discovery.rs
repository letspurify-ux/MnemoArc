//! Conservative candidate extraction, never an assertion of exhaustive coverage.
//! Every source/config file, including unsupported languages, requires review.
use super::*;
use regex::Regex;
use std::sync::OnceLock;

pub(super) fn in_scope(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if matches!(
        name,
        "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "Cargo.lock"
            | "go.sum"
            | ".DS_Store"
            | "Thumbs.db"
            | "desktop.ini"
    ) {
        return false;
    }
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // Markdown inside file-router directories is screen source, even though
    // ordinary Markdown artifacts are excluded from the inventory.
    if ext == "md"
        && path
            .components()
            .any(|c| matches!(c.as_os_str().to_str(), Some("pages" | "routes")))
    {
        return true;
    }
    !matches!(
        ext.as_str(),
        "md" | "rst"
            | "txt"
            | "log"
            | "lock"
            | "map"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "ico"
            | "svg"
            | "pdf"
            | "woff"
            | "woff2"
            | "ttf"
            | "otf"
            | "zip"
            | "gz"
            | "tar"
            | "br"
            | "db"
            | "sqlite"
            | "sqlite3"
            | "wasm"
            | "exe"
            | "dll"
            | "so"
            | "dylib"
            | "class"
            | "jar"
            | "pyc"
            | "mp4"
            | "mp3"
    )
}

type Candidate = (String, usize, usize, String, String);
fn patterns() -> &'static Vec<(&'static str, Regex)> {
    static PATTERNS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    PATTERNS.get_or_init(|| [
        ("screen", r#"<Route\b[^>]{0,600}\bpath\s*=\s*\{?\s*["'][^"']+["']"#),
        ("screen", r#"\b(?:page|screen|view)\s*={2,3}\s*["'][^"']+["']"#),
        ("tab", r#"\b(?:tab|activeTab|selectedTab)\s*={2,3}\s*["'][^"']+["']"#),
        ("dialog", r#"\brole\s*=\s*["'](?:dialog|alertdialog)["']|<(?:dialog|Dialog|Modal)\b"#),
        ("http", r#"\.(?:route|nest|Mount|HandleFunc|Handle|MapGet|MapPost|MapPut|MapDelete|get|post|put|patch|delete|head|options|all)\s*\(\s*["'`][^"'`\r\n]+["'`]|@(?:GetMapping|PostMapping|PutMapping|DeleteMapping|RequestMapping)\s*\([^\r\n]{1,200}|\[Http(?:Get|Post|Put|Delete|Patch)\b[^\r\n]{0,200}|\b(?:path|re_path)\s*\(\s*["'][^"']+["']"#),
        ("rpc", r#"\brpc\s+\w+\s*\([^)]{0,200}\)|\b(?:register|add)_\w+(?:Servicer|Service|Server)_to_server\s*\("#),
        ("websocket", r#"\b(?:WebSocketUpgrade|WebSocketServer|websocket_endpoint)\b|\.websocket\s*\(\s*["'][^"']+["']"#),
        ("event", r#"\.(?:on|subscribe|consume|addEventListener)\s*\(\s*["'][^"']+["']|@(?:EventListener|KafkaListener|RabbitListener)\b"#),
        ("job", r#"\b(?:tokio::spawn|setInterval|scheduleJob|cron::Job|CronJob)\s*\(|@(?:Scheduled|Cron)\s*\("#),
        ("cli", r#"\b(?:enum\s+(?:Command|Commands)|Subcommand|add_subparsers|add_parser)\b|\.command\s*\(\s*["'][^"']+["']"#),
        ("lifecycle", r#"\b(?:on_startup|on_shutdown|with_graceful_shutdown|register_shutdown_hook)\b|@(?:PostConstruct|PreDestroy)\b"#),
    ].into_iter().map(|(kind, pattern)| (kind, Regex::new(pattern).expect("static discovery regex"))).collect())
}

pub(super) fn detect(
    path: &str,
    content: &str,
    cancel: &CancellationToken,
) -> Result<Vec<Candidate>> {
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(content.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let mut found = Vec::new();
    let mut occurrences = BTreeMap::new();
    for (kind, pattern) in patterns() {
        for matched in pattern.find_iter(content) {
            if cancel.is_cancelled() {
                bail!("cancelled");
            }
            let line_start = content[..matched.start()].rfind('\n').map_or(0, |i| i + 1);
            let prefix = content[line_start..matched.start()].trim_start();
            if prefix.starts_with("//") || prefix.starts_with("# ") || prefix.starts_with("*") {
                continue;
            }
            let normalized = matched
                .as_str()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let base = format!("{kind}:{normalized}");
            let occurrence = occurrences.entry(base.clone()).or_insert(0);
            *occurrence += 1;
            let anchor = format!("{}:{}", hash(base.as_bytes()), occurrence);
            let start = line_starts.partition_point(|i| *i <= matched.start());
            let end = line_starts.partition_point(|i| *i < matched.end());
            found.push((
                kind.to_string(),
                start,
                end,
                anchor,
                normalized.chars().take(160).collect(),
            ));
            if found.len() > MAX_FEATURES {
                bail!(
                    "Too many discovery candidates in {path}; no candidates were silently truncated"
                );
            }
        }
    }
    // File-system routers need no explicit route registration in the source.
    let stem = Path::new(path)
        .file_stem()
        .and_then(|p| p.to_str())
        .unwrap_or("");
    let ext = Path::new(path)
        .extension()
        .and_then(|p| p.to_str())
        .unwrap_or("");
    if !content.trim().is_empty()
        && matches!(
            ext,
            "jsx" | "tsx" | "js" | "ts" | "vue" | "svelte" | "mdx" | "md" | "astro" | "html"
        )
        && ((path.starts_with("pages/") || path.contains("/pages/"))
            || (matches!(stem, "page" | "+page")
                && (path.starts_with("app/")
                    || path.contains("/app/")
                    || path.contains("routes/"))))
    {
        found.push((
            "screen".into(),
            1,
            1,
            format!("file-route:{path}"),
            format!("Page {path}").chars().take(160).collect(),
        ));
    }
    found.sort_by_key(|f| f.1);
    Ok(found)
}
