import { fromMarkdown } from "mdast-util-from-markdown";

// Match the path/line notation used by the source-document citation checker.
// Edit original spans instead of serializing the AST and reformatting Markdown.
const citationPattern =
  /(?<![\p{L}\p{N}_./@-])[\p{L}\p{N}_./@-]+\.[A-Za-z][A-Za-z0-9]*(?::|#L)[0-9]+(?:[-–]L?[0-9]+)?(?:[ \t]*,[ \t]*[0-9]+(?:[-–]L?[0-9]+)?)*(?![\p{L}\p{N}_–-])/gu;
const sourceLabel = "(?:근거|출처|소스|참고|sources?|evidence)";

function replaceCitations(text, replacement, mermaid = false) {
  return text.replace(citationPattern, (citation, offset) => {
    let tokenStart = offset;
    while (tokenStart > 0 && !/[\s`(\["']/.test(text[tokenStart - 1]))
      tokenStart--;
    const prefix = text.slice(tokenStart, offset);
    // URLs can contain file suffixes and line-like fragments too.
    if (citation.startsWith("//") || prefix.includes("://")) return citation;
    // Serialized Mermaid breaks can otherwise be read as part of the path.
    if (mermaid && text[offset - 1] === "\\" && /^[nr]/.test(citation))
      return citation[0] + replacement;
    return replacement;
  });
}

function citationOnly(text) {
  const remainder = replaceCitations(text, "");
  return (
    remainder !== text &&
    new RegExp(
      `^[\\s,;·()\\[\\]{}]*(?:${sourceLabel}[ \\t]*[:：]?)?[\\s,;·()\\[\\]{}]*$`,
      "i",
    ).test(remainder)
  );
}

function visibleText(node) {
  return node.value ?? (node.children ?? []).map(visibleText).join("");
}

export function cleanDocument(markdown) {
  // micromark's offsets exclude an initial BOM.
  if (markdown.startsWith("\uFEFF"))
    return "\uFEFF" + cleanDocument(markdown.slice(1));
  let marker = "\u0000";
  while (markdown.includes(marker)) marker += "\u0000";
  const tree = fromMarkdown(markdown);
  const definitions = new Map(
    tree.children
      .filter((node) => node.type === "definition")
      .map((node) => [node.identifier, node.url]),
  );

  function transform(node) {
    const start = node.position.start.offset;
    const end = node.position.end.offset;
    const raw = markdown.slice(start, end);
    if (node.type === "code")
      return node.lang?.toLowerCase() === "mermaid"
        ? replaceCitations(raw, marker, true)
        : raw;
    if (node.type === "inlineCode")
      return citationOnly(node.value) ? marker : raw;
    if (node.type === "text") return replaceCitations(raw, marker);
    if (node.type === "definition")
      return citationOnly(node.url) ? marker : raw;
    const children = node.children ?? [];
    const url = node.url ?? definitions.get(node.identifier);
    if (
      ["link", "linkReference", "emphasis", "strong", "paragraph"].includes(
        node.type,
      ) &&
      citationOnly(visibleText(node))
    )
      return marker;
    if (
      ["link", "linkReference"].includes(node.type) &&
      citationOnly(url ?? "")
    )
      return children.map(transform).join("");
    let cursor = start;
    let result = "";
    for (const child of children) {
      result += markdown.slice(cursor, child.position.start.offset);
      result += transform(child);
      cursor = child.position.end.offset;
    }
    return result + markdown.slice(cursor, end);
  }

  let result = transform(tree);
  if (!result.includes(marker)) return result;
  const markers = `${marker}(?:[ \\t]*[,;·]?[ \\t]*${marker})*`;
  const label = `(?:${sourceLabel}[ \\t]*[:：]?[ \\t]*)?`;
  // Remove wrappers only when their entire contents were citation annotations.
  let previous;
  do {
    previous = result;
    for (const [open, close] of [
      ["\\(", "\\)"],
      ["\\[", "\\]"],
      ["\\{", "\\}"],
    ])
      result = result.replace(
        new RegExp(`${open}[ \\t]*${label}${markers}[ \\t]*${close}`, "gi"),
        marker,
      );
  } while (result !== previous);
  result = result
    .replace(new RegExp(`(?:<br\\s*/?>|\\\\n)[ \\t]*${markers}`, "gi"), marker)
    .replace(
      new RegExp(`${sourceLabel}[ \\t]*[:：][ \\t]*${markers}`, "gi"),
      marker,
    )
    .replace(new RegExp(markers, "g"), marker)
    .replace(
      new RegExp(`[ \\t]*${marker}[ \\t]*`, "g"),
      (match, offset, text) => {
        const before = text[offset - 1];
        const after = text[offset + match.length];
        return match.length > marker.length &&
          before &&
          after &&
          !/[\s([{]/.test(before) &&
          !/[\s)\]},.;:!?"']/.test(after)
          ? " "
          : "";
      },
    );
  return result;
}
