import { fromMarkdown } from "mdast-util-from-markdown";

// Match the path/line notation used by the source-document citation checker.
// Edit original spans instead of serializing the AST and reformatting Markdown.
const citationPattern =
  /(?<![\p{L}\p{N}_./@-])[\p{L}\p{N}_./@-]+\.[A-Za-z][A-Za-z0-9]*(?::|#L)[0-9]+(?:[-–]L?[0-9]+)?(?:[ \t]*,[ \t]*[0-9]+(?:[-–]L?[0-9]+)?)*(?![\p{L}\p{N}_–-])/gu;
const sourceLabel = "(?:근거|출처|소스|참고|sources?|evidence)";

function replaceCitations(text, replacement, mermaid = false) {
  // Matches arrive in order; scanning back from each one to the token start
  // was quadratic on a long line of citations without spaces.
  let scanned = 0,
    tokenStart = 0,
    scheme = -1;
  return text.replace(citationPattern, (citation, offset) => {
    for (; scanned < offset; scanned++) {
      if (/[\s`(\["']/.test(text[scanned])) tokenStart = scanned + 1;
      else if (text.startsWith("://", scanned)) scheme = scanned;
    }
    // URLs can contain file suffixes and line-like fragments too.
    if (
      citation.startsWith("//") ||
      (scheme >= tokenStart && scheme + 3 <= offset)
    )
      return citation;
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
    // Each run of separators has one owner, so long whitespace cannot
    // backtrack quadratically.
    new RegExp(
      `^[\\s,;·()\\[\\]{}]*(?:${sourceLabel}(?:[ \\t]*[:：])?[\\s,;·()\\[\\]{}]*)?$`,
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
  // Whitespace around an optional separator belongs to one quantifier only;
  // overlapping ones backtracked quadratically on long runs of spaces.
  const markers = `${marker}(?:[ \\t]*(?:[,;·][ \\t]*)?${marker})*`;
  const label = `(?:${sourceLabel}[ \\t]*(?:[:：][ \\t]*)?)?`;
  // Remove wrappers only when their entire contents were citation annotations.
  // Fullwidth brackets too: a live document wrapped every citation in 【】.
  let previous;
  do {
    previous = result;
    for (const [open, close] of [
      ["\\(", "\\)"],
      ["\\[", "\\]"],
      ["\\{", "\\}"],
      ["【", "】"],
      ["（", "）"],
      ["［", "］"],
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
    .replace(new RegExp(markers, "g"), marker);
  return joinAroundMarkers(result, marker);
}

// Drop each marker with the spaces and tabs around it, keeping one space only
// between words the removed citation separated. Matching the surrounding
// blanks with a regular expression retried every position of a long run.
function joinAroundMarkers(text, marker) {
  const parts = text.split(marker);
  const blank = (c) => c === " " || c === "\t";
  const bounds = parts.map((part, i) => {
    let start = 0,
      end = part.length;
    if (i > 0) while (start < end && blank(part[start])) start++;
    if (i < parts.length - 1) while (end > start && blank(part[end - 1])) end--;
    return [start, end];
  });
  let result = parts[0].slice(0, bounds[0][1]);
  for (let i = 1; i < parts.length; i++) {
    const [leftStart, leftEnd] = bounds[i - 1];
    const [start, end] = bounds[i];
    const before = leftEnd > leftStart ? parts[i - 1][leftEnd - 1] : undefined;
    const after = parts[i][start];
    const spaced = leftEnd < parts[i - 1].length || start > 0;
    if (
      spaced &&
      before &&
      after &&
      !/[\s([{【（［]/.test(before) &&
      !/[\s)\]},.;:!?"'】）］]/.test(after)
    )
      result += " ";
    result += parts[i].slice(start, end);
  }
  return result;
}
