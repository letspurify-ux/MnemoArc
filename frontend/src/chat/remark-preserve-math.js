import { analyzeRichStructure } from '../../shared/rich-table-structure.mjs';
import { fromMarkdown } from 'mdast-util-from-markdown';
import { restoreTableVisualizations } from './remark-rich-table.js';
import { markCellTextSyntax, sliceCellText } from './cell-text.js';
export { MAX_MATH_SPAN, normalizeMath, unwrapMath } from '../../shared/math-spans.mjs';

const mathNode = (value, display, source, incomplete = false) => ({
  type: 'inlineMath', value,
  data: {
    display, source,
    hName: 'span',
    hProperties: { className: ['math', display ? 'math-display' : 'math-inline'],
      'data-math-source': source, 'data-math-incomplete': incomplete },
    hChildren: [{ type: 'text', value }],
  },
});

export default function remarkPreserveMath() {
  const processor = this;
  return (tree, file) => {
    const source = String(file);
    // 앞 단계에서 보호한 시각화도 언어 소유 범위를 유지한다. 임시 코드의
    // 이름에 기대어 재추론하지 않고, 그 단계가 남긴 정확한 치환 범위를 전달한다.
    const visualizations = file.data.richTablePattern ? [...source.matchAll(file.data.richTablePattern)]
      .map(match => ({ start: match.index, end: match.index + match[0].length })) : [];
    const { candidates, referenceEdits, definitions } = analyzeRichStructure(source, tree,
      value => processor.parse(value), value => fromMarkdown(value, {
        extensions: [...processor.data('micromarkExtensions'), { disable: { null: ['table'] } }],
        mdastExtensions: processor.data('fromMarkdownExtensions'),
      }), visualizations);
    if (!candidates.length) return tree;
    const edits = [...candidates, ...referenceEdits].sort((a, b) => a.start - b.start);
    let prefix = 'LLMMATHPLACEHOLDER';
    while (source.toLowerCase().includes(prefix.toLowerCase())) prefix += 'X';
    const placeholders = new Map(), aliases = new Map();
    let masked = '', cursor = 0;
    for (const [index, span] of edits.entries()) {
      if (span.start < cursor) continue;
      if (span.referenceId !== undefined) {
        const alias = `${prefix}REF${index}END`.toLowerCase();
        aliases.set(alias, { ...definitions.get(span.referenceId), identifier: alias, label: alias });
        masked += source.slice(cursor, span.start) + `[${alias}]`;
        cursor = span.end; continue;
      }
      const placeholder = `${prefix}${index}END`;
      placeholders.set(placeholder, span);
      masked += source.slice(cursor, span.start) + placeholder;
      cursor = span.end;
    }
    masked += source.slice(cursor);
    // 첫 파싱은 코드·주소 경계를 제공하고, 두 번째는 수식 내부의 Markdown 문법을 보지 못한다.
    // 파서가 이 토큰을 읽으므로 표의 |·목록·인용문·빈 줄도 따로 흉내 낼 필요가 없다.
    // 정의를 앞에 둬 스트림 끝의 미완성 코드 펜스에 삼켜지지 않게 한다.
    const declarations = [...aliases.keys()].map(alias => `[${alias}]: /`).join('\n');
    const parseSource = declarations ? declarations + '\n\n' + masked : masked;
    const result = processor.parse(parseSource);
    markCellTextSyntax(result, parseSource);
    const pattern = new RegExp(`${prefix}\\d+END`, 'g');
    const restore = node => {
      if (!node.children) return;
      node.children = node.children.flatMap(child => {
        if (child.type === 'definition' && aliases.has(child.identifier)) return [aliases.get(child.identifier)];
        if (child.type !== 'text') {
          restore(child);
          // 임시 토큰 때문에 새로 생긴 자동 링크는 링크가 아니다. 주소에 토큰을 남기지 않는다.
          return child.type === 'link' && child.url.includes(prefix) ? child.children : [child];
        }
        const parts = []; let last = 0;
        for (const match of child.value.matchAll(pattern)) {
          const span = placeholders.get(match[0]);
          if (!span) continue;
          if (match.index > last) parts.push(sliceCellText(child, last, match.index));
          parts.push(span.literal !== undefined ? { type: 'text', value: restoreTableVisualizations(span.literal, file) }
            : mathNode(restoreTableVisualizations(span.value, file), span.display,
              restoreTableVisualizations(source.slice(span.start, span.end), file), span.incomplete));
          last = match.index + match[0].length;
        }
        if (!parts.length) return [child];
        if (last < child.value.length) parts.push(sliceCellText(child, last));
        return parts;
      });
      // 별행 수식은 문단을 나눈다. 표 셀·제목에서는 span으로 표시한다.
      node.children = node.children.flatMap(child => {
        if (child.type !== 'paragraph' || !child.children.some(c => c.data?.display)) return [child];
        const out = []; let text = [];
        const flush = () => { if (text.some(c => c.type !== 'text' || c.value.trim())) out.push({ type: 'paragraph', children: text }); text = []; };
        for (const part of child.children) {
          if (part.data?.display) { flush(); part.type = 'math'; out.push(part); }
          else text.push(part);
        }
        flush(); return out;
      });
    };
    restore(result);
    return result;
  };
}
