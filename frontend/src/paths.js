// Keep canonical paths intact for requests and file access; format only UI text.
export function displayPath(path) {
  const value = String(path ?? "");
  const prefix = "\\\\?\\";
  if (!value.startsWith(prefix)) return value;
  const rest = value.slice(prefix.length);
  if (/^[A-Za-z]:\\/.test(rest)) return rest;
  if (/^UNC\\[^\\]+\\[^\\]+(?:\\|$)/i.test(rest)) return `\\\\${rest.slice(4)}`;
  return value;
}

// This takes plain text, never serialized JSON or source code.
export function displayPathText(text) {
  return String(text ?? "").replace(
    /\\\\\?\\(?:([A-Za-z]:\\)|UNC\\([^\\\r\n]+\\[^\\\r\n]+))/gi,
    (_, drive, share) => drive || `\\\\${share}`,
  );
}

const pathFields = new Set([
  "path",
  "paths",
  "root",
  "project_root",
  "scope",
  "output",
  "output_path",
  "source_path",
  "destination",
  "files",
  "written_paths",
]);
const diagnosticFields = new Set([
  "error",
  "message",
  "guidance",
  "reason",
  "next_action",
  "instruction",
  "note",
  "progress",
  "summary",
  "title",
  "findings",
  "unresolved",
  "verification_note",
]);
const originalFields = new Set([
  "content",
  "text",
  "excerpt",
  "body",
  "query",
  "sql",
  "arguments",
  "numbered_text",
  "old_text",
  "new_text",
  "replacement",
  "code",
  "rows",
]);

// A presentation copy keeps paths, source text and replay data authoritative.
// Reuse unchanged objects so callers can offer raw JSON only when needed.
export function displayToolResult(value, field = "") {
  if (originalFields.has(field)) return value;
  if (typeof value === "string") {
    if (pathFields.has(field) || /_paths?$/.test(field))
      return displayPath(value);
    if (diagnosticFields.has(field)) return displayPathText(value);
    return value;
  }
  if (Array.isArray(value)) {
    const items = value.map((item) => displayToolResult(item, field));
    return items.some((item, index) => item !== value[index]) ? items : value;
  }
  if (value && typeof value === "object") {
    const entries = Object.entries(value).map(([key, item]) => [
      key,
      displayToolResult(item, key),
    ]);
    return entries.some(([key, item]) => item !== value[key])
      ? Object.fromEntries(entries)
      : value;
  }
  return value;
}
