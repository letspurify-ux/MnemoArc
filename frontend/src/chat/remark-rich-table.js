// GFM 표 셀의 제한된 확장: <br> 줄바꿈과 언어가 명시된 직렬화 시각화만 처리한다.
// 일반 HTML·일반 인라인 코드는 실행하거나 재해석하지 않는다.
import { decodeSerializedLines, decodeVisualizationBreaks } from './serialized-markdown.js';
import { fromMarkdown } from 'mdast-util-from-markdown';
import { markCellTextSyntax, sliceCellText } from './cell-text.js';
import { inlineCodeSpans, markdownLiteralRanges, codeSpanInLiteral } from '../../shared/inline-code.mjs';
import { analyzeRichStructure } from '../../shared/rich-table-structure.mjs';
const BR = /\\?<br\s*\/?>/gi;

// 다른 문법(예: LaTeX)이 보호 구간 전체를 포함하면 그 문법에 원문을 돌려준다.
// 임시 토큰이나 보호용 백틱이 KaTeX의 본문·오류 원문에 남아서는 안 된다.
export const restoreTableVisualizations = (value, file) => file.data.richTablePattern
  ? value.replace(file.data.richTablePattern, token => file.data.richTableSources.get(token)) : value;

// GFM은 인라인 코드 안의 |도 열 구분자로 읽는다. 명시적인 시각화 코드 안의
// 파이프만 먼저 보호해 표가 데이터 열을 버리지 않도록 한다. 개행 수는 바뀌지 않는다.
export function remarkProtectTableVisualizations() {
  const processor = this;
  return (tree, file) => {
    const source = String(file);
    if (!/`[ \t]*(?:chart|mermaid)(?=(?:\\r)?\\n|\\?<br\s*\/?>)/i.test(source)) return tree;
    const edits = [];
    let prefix = 'LLMRICHTABLE';
    while (source.includes(prefix)) prefix += 'X';
    const values = new Map();
    const originals = new Map();
    const definitions = [];
    const collectDefinitions = node => {
      if (node.type === 'definition' || node.type === 'footnoteDefinition')
        definitions.push(source.slice(node.position.start.offset, node.position.end.offset));
      for (const child of node.children ?? []) collectDefinitions(child);
    };
    collectDefinitions(tree);
    const visit = node => {
      if (node.type === 'tableCell' || node.type === 'paragraph') {
        const start = node.position.start.offset;
        const row = source.slice(start, node.position.end.offset);
        if (!/`[ \t]*(?:chart|mermaid)(?=(?:\\r)?\\n|\\?<br\s*\/?>)/i.test(row)) return;
        // 표 구분자를 제외한 한 행을 분석하면 | 때문에 잘렸던 주소·HTML도 온전히
        // 인식된다. 주소/속성에 적힌 백틱은 실행할 코드가 아니다.
        const inlinePrefix = node.type === 'tableCell' ? 'x ' : '';
        const rowSource = inlinePrefix + row + (definitions.length ? '\n\n' + definitions.join('\n') : '');
        const protectedRanges = markdownLiteralRanges(processor.parse(rowSource), rowSource)
          .map(([start, end]) => [start - inlinePrefix.length, end - inlinePrefix.length]);
        for (const span of inlineCodeSpans(row, protectedRanges)) {
          if (!/^(chart|mermaid)(?=(?:\\r)?\\n|\\?<br\s*\/?>)/i.test(span.value.trim())) continue;
          if (codeSpanInLiteral(span, protectedRanges)) continue;
          if (node.type === 'paragraph' && (row.slice(0, span.start).trim() || row.slice(span.end).trim())) continue;
          // 바깥 GFM 표의 이스케이프 한 겹을 벗긴다. 내부 표 값의 \|는
          // 바깥 셀에서 \\\|로 써야 하며, 코드 노드로 재파싱하며 잃지 않도록 보관한다.
          const body = node.type === 'tableCell' ? span.value.replace(/(\\+)\|/g,
            (_whole, slashes) => '\\'.repeat(Math.floor(slashes.length / 2)) + '|') : span.value;
          const token = prefix + values.size;
          values.set(token, body);
          originals.set('`' + token + '`', row.slice(span.start, span.end));
          edits.push({ start: start + span.start, end: start + span.end, value: '`' + token + '`' });
        }
        return;
      }
      if (node.type === 'code') return;
      for (const child of node.children ?? []) visit(child);
    };
    const { structure } = analyzeRichStructure(source, tree, value => processor.parse(value), value => fromMarkdown(value, {
      extensions: [...processor.data('micromarkExtensions'), { disable: { null: ['table'] } }],
      mdastExtensions: processor.data('fromMarkdownExtensions'),
    }));
    visit(structure);
    if (!edits.length) return tree;
    let result = '', at = 0;
    for (const edit of edits) { result += source.slice(at, edit.start) + edit.value; at = edit.end; }
    result += source.slice(at);
    if (result === source) return tree;
    file.data.richTableVisualizations = values;
    file.data.richTableSources = originals;
    file.data.richTablePattern = new RegExp('`' + prefix + '\\d+`', 'g');
    file.value = result;
    return processor.parse(result);
  };
}

export function tableVisualization(value) {
  const raw = String(value ?? '').trim();
  const match = /^(?:`+|~{3,})?(mermaid|chart)(?:(?:\\r)?\\n|\r\n?|\n|\\?<br\s*\/?>)/i.exec(raw);
  if (!match) return null;
  let body = raw.slice(match[0].length);
  const fence = /^(?:`+|~{3,})/.exec(raw)?.[0];
  if (fence && body.endsWith(fence)) body = body.slice(0, -fence.length);
  // \n으로 직렬화된 조회 데이터의 '<br>' 값은 그대로다. br 구분자를 선택한 코드만 변환한다.
  const language = match[1].toLowerCase();
  body = /<br/i.test(match[0]) ? decodeVisualizationBreaks(body, language) : decodeSerializedLines(body, language);
  body = body.trim();
  return { type: 'code', lang: language, value: body };
}

// 강조·링크 안의 줄바꿈도 서식을 유지하면서 줄 단위로 나눈다.
function linesOf(node) {
  if (node.type === 'inlineCode') {
    const code = tableVisualization(node.value);
    if (code) return [[code]];
  }
  if (node.type === 'break' || (node.type === 'html' && /^<br\s*\/?>$/i.test(node.value))) return [[], []];
  if (node.type === 'text') {
    const lines = []; let at = 0;
    for (const match of node.value.matchAll(BR)) {
      lines.push(match.index > at ? [sliceCellText(node, at, match.index)] : []);
      at = match.index + match[0].length;
    }
    lines.push(at < node.value.length ? [sliceCellText(node, at)] : []);
    return lines;
  }
  if (['strong', 'emphasis', 'delete', 'link', 'linkReference'].includes(node.type)) {
    const lines = splitLines(node.children);
    return lines.map(children => {
      const out = []; let phrasing = [];
      const flush = () => { if (phrasing.length) out.push({ ...node, children: phrasing }); phrasing = []; };
      for (const child of children) {
        if (child.type === 'code') { flush(); out.push(child); }
        else phrasing.push(child);
      }
      flush(); return out;
    });
  }
  return [[node]];
}
function splitLines(children) {
  const lines = [[]];
  for (const child of children) {
    const split = linesOf(child);
    lines.at(-1).push(...split[0]);
    lines.push(...split.slice(1));
  }
  return lines;
}
function blocksOf(lines, parse) {
  // 블록 경계만 다시 파싱한다. 수식·코드·주소뿐 아니라 이미 해석한 일반
  // 글자도 토큰으로 보관해, 이스케이프·문자 참조를 Markdown으로 재실행하지 않는다.
  // 목록 깊이·연속 행·인용 안의 목록은 바깥 문서와 같은 파서가 결정한다.
  const nodes = new Map();
  const source = lines.map(line => {
    if (!line.some(node => node.type !== 'text' || node.value.trim())) return '';
    let prefix = '';
    const content = line.map((node, index) => {
      if (index === 0 && node.type === 'text') {
        const syntax = node.data?.cellSyntax ?? '';
        prefix = /^[ \t]*(?:(?:>[ \t]?|(?:[-+*]|\d{1,9}[.)])[ \t]+)[ \t]*)*(?:#{1,6}[ \t]+)?/.exec(syntax)[0];
        let value = node.value.slice(prefix.length);
        if (/(?:[-+*]|\d[.)])[ \t]+$/.test(prefix)) {
          const task = /^\[[ xX]\][ \t]+/.exec(syntax.slice(prefix.length))?.[0] ?? '';
          prefix += task; value = value.slice(task.length);
        }
        node = { ...node, value };
        if (!value) return '';
      }
      const token = `LLMCELLNODE${nodes.size}END`;
      nodes.set(token, node);
      return token;
    }).join('');
    return prefix + content;
  }).join('\n');
  const tree = parse(source);
  const restore = node => {
    if (!node.children) return;
    node.children = node.children.flatMap(child => {
      if (child.type !== 'text') { restore(child); return [child]; }
      const parts = []; let at = 0;
      for (const match of child.value.matchAll(/LLMCELLNODE\d+END|\n/g)) {
        if (match.index > at) parts.push({ type: 'text', value: child.value.slice(at, match.index) });
        parts.push(match[0] === '\n' ? { type: 'break' } : nodes.get(match[0]));
        at = match.index + match[0].length;
      }
      if (at < child.value.length) parts.push({ type: 'text', value: child.value.slice(at) });
      return parts;
    });
    node.children = node.children.flatMap(child => {
      if (!['paragraph', 'heading'].includes(child.type) || !child.children.some(part => part.type === 'code')) return [child];
      const blocks = []; let phrasing = [];
      const flush = () => {
        if (phrasing.some(part => part.type !== 'text' || part.value.trim())) blocks.push({ ...child, children: phrasing });
        phrasing = [];
      };
      for (const part of child.children) {
        if (part.type === 'code') { flush(); blocks.push(part); }
        else phrasing.push(part);
      }
      flush(); return blocks;
    });
  };
  restore(tree);
  return tree.children;
}
export default function remarkRichTable() {
  const processor = this;
  // 셀에는 이미 해석한 인라인 노드가 있다. 들여쓰기는 컨테이너의 깊이에만
  // 사용하며, 그 노드를 새 일반 코드 블록으로 감싸 토큰을 노출하지 않는다.
  const parseBlocks = source => fromMarkdown(source, {
    extensions: [...processor.data('micromarkExtensions'), { disable: { null: ['codeIndented'] } }],
    mdastExtensions: processor.data('fromMarkdownExtensions'),
  });
  return (tree, file) => {
    markCellTextSyntax(tree, String(file));
    const restore = node => {
      if (node.type === 'inlineCode' && file.data.richTableVisualizations?.has(node.value)) {
        node.value = file.data.richTableVisualizations.get(node.value);
      }
      for (const child of node.children ?? []) restore(child);
    };
    restore(tree);
    const visit = node => {
      // 표 밖에서도 독립된 시각화 코드 한 덩어리는 같은 렌더 경로를 사용한다.
      if (node.type === 'paragraph') {
        const meaningful = node.children.filter(child => child.type !== 'text' || child.value.trim());
        if (meaningful.length === 1 && ['inlineCode', 'text'].includes(meaningful[0].type)) {
          const code = tableVisualization(meaningful[0].value);
          if (code) return code;
        }
      }
      if (node.type === 'tableCell') {
        // 전체 셀의 시각화 또는 설명 사이의 완전한 시각화 코드만 블록으로 승격한다.
        const meaningful = node.children.filter(child => child.type !== 'text' || child.value.trim());
        const only = meaningful.length === 1 && meaningful[0];
        const code = only && ['inlineCode', 'text'].includes(only.type) && tableVisualization(only.value);
        if (code) node.children = [code];
        else {
          const lines = splitLines(node.children);
          if (lines.length <= 1 && !lines[0].some(child => child.type === 'code')) return node;
          node.children = blocksOf(lines, parseBlocks);
        }
        node.data = { ...node.data, hProperties: { ...node.data?.hProperties, className: ['rich-cell'] } };
        return node;
      }
      if (node.children) node.children = node.children.map(visit);
      return node;
    };
    visit(tree);
  };
}
