import { collectMathSpans } from './math-spans.mjs';
import { scopedMermaidMathExpressions } from './mermaid-math-spans.mjs';
import { mergeLiteralRanges, inlineCodeSpans } from './inline-code.mjs';
import { tableOwnership, analyzeRichStructure } from './rich-table-structure.mjs';
import { texGroupEnds } from './tex-environments.mjs';

// 모델이 JSON 이스케이프를 한 겹 더 남긴 경우만 복구한다. 코드·표 셀은 각자의 문법을 유지한다.
function replaceOutsideMath(value, pattern, replace, language) {
  // Mermaid 라벨의 $$...$$·TeX 구분자 내부는 개행 직렬화 문법이 아니다.
  // 명령 이름을 나열하면 새 명령·매크로마다 다시 깨지므로 수식 범위 전체를 보존한다.
  const spans = language === 'mermaid'
    ? scopedMermaidMathExpressions(value, { lineBreaks: pattern })
    : collectMathSpans({ type: 'root', children: [] }, value).candidates.sort((a, b) => a.start - b.start);
  let index = 0;
  return value.replace(pattern, (match, offset) => {
    while (index < spans.length && spans[index].end <= offset) index++;
    return index < spans.length && spans[index].start <= offset ? match : replace(match, offset);
  });
}
export function decodeSerializedLines(value, language) {
  return replaceOutsideMath(value, /\\\\|\\r\\n|\\n|\\r(?![A-Za-z])/g, match =>
    match === '\\\\' ? match : '\n', language);
}
export const decodeVisualizationBreaks = (value, language) => replaceOutsideMath(value, /\\?<br\s*\/?>/gi, () => '\n', language);
export function decodeMathEscapes(value) {
  // 정상 TeX의 \\ 행 구분자는 건드리지 않는다. 직렬화된 개행과 이중 명령이 함께 있거나,
  // $$ 안의 앞뒤에 직렬화된 개행이 남은 경우가 추가 이스케이프의 증거다.
  const frame = /^(\s*)(?:\\r)?\\n([\s\S]*?)(?:\\r)?\\n(\s*)$/.exec(value);
  if (!/\\n/.test(value) || (!/\\\\[A-Za-z]/.test(value) && !frame)) return value;
  // TeX 본문에서 한 겹으로 남은 제어 단어는 이름 전체가 하나의 토큰이다.
  // 직렬화된 개행 토큰만 복원하며 알려진 명령·매크로 목록에 의존하지 않는다.
  // 양끝 개행으로 감싼 직렬화 형식은 본문을 읽기 전에 벗긴다. 그렇지 않으면
  // 첫 글자가 x인 식의 여는 개행까지 제어 단어 \\nx로 읽게 된다.
  const body = frame ? frame[2] : value;
  const decoded = body.replace(/\\\\|\\r\\n|\\n[A-Za-z]*|\\[^\r\n]/g, match =>
    match === '\\\\' ? '\\' : match === '\\n' || match === '\\r\\n' ? '\n' : match);
  return frame ? frame[1] + '\n' + decoded + '\n' + frame[3] : decoded;
}

function mathStructureFrame(source) {
  const pieces = []; let cursor = 0;
  // TeX 인자 내부의 개행 문자는 인자의 원문이다. 이미 사용하는 TeX 경계
  // 엔진으로 완성된 그룹만 보호하므로 명령·매크로 이름에 의존하지 않는다.
  const groups = texGroupEnds(source);
  for (let i = 0; i < source.length; i++) {
    const end = groups.get(i);
    if (end === undefined) continue;
    pieces.push(source.slice(cursor, i), source.slice(i, end + 1).replace(/\\r\\n|\\n/g, match => 'x'.repeat(match.length)));
    cursor = end + 1; i = end;
  }
  pieces.push(source.slice(cursor));
  return pieces.join('').replace(/[|$]/g, 'x');
}

export function normalizeSerializedMarkdown(source, tree, parse, parseWithoutTables = parse) {
  if (!source.includes('\\n') && !source.includes('\\>')) return source;
  const ranges = [];
  const scanLiterals = [];
  // 후보 발견만 한다. 구분 행의 대시 개수·열 수는 아래의 GFM 파서가 판정한다.
  const serializedTable = /\\n[ \t>]*\|?[ \t]*:?-+[ \t]*:?[ \t]*\|/.test(source);
  const framedRows = [], visualizations = [];
  let structure = tree;
  let frame = '';
  if (!serializedTable && source.includes('|') && /[\r\n]/.test(source)) {
    // 실제 개행의 표도 첫 AST가 머리글의 내부 | 때문에 놓칠 수 있다.
    // 복원 전에 같은 구조 분석으로 셀 코드의 개행 소유 범위를 확정한다.
    const analyzed = analyzeRichStructure(source, tree, parse, parseWithoutTables);
    structure = analyzed.structure;
    tree = analyzed.ownership;
    ranges.push(...analyzed.candidates.map(({ start, end }) => [start, end]));
  }
  if (serializedTable) {
    // 구조 발견용 좌표 투영이다. 시각화·수식 안의 개행은 가리고, 문서의 직렬화
    // 개행만 같은 길이의 실제 개행으로 투영한다. 행·구분 행은 GFM 파서가 판정한다.
    visualizations.push(...inlineCodeSpans(source, [], 0, { visualizationCandidates: true }));
    // 아직 행 경계가 없는 AST의 수식·링크는 여러 직렬화 행을 한 원자로
    // 읽을 수 있다. 수식은 열 구분자만 가리고 개행·컨테이너는 남겨 둔다.
    // 시각화는 명시된 내부 언어가 개행까지 소유한다.
    const formulas = collectMathSpans(tree, source, parse).candidates;
    let mathFrame = '', mathCursor = 0;
    for (const [start, end] of mergeLiteralRanges(formulas.map(({ start, end }) => [start, end]))) {
      mathFrame += source.slice(mathCursor, start) + mathStructureFrame(source.slice(start, end));
      mathCursor = end;
    }
    mathFrame += source.slice(mathCursor);
    const masks = mergeLiteralRanges(visualizations.map(({ start, end }) => [start, end]));
    let cursor = 0;
    for (const [start, end] of masks) {
      frame += mathFrame.slice(cursor, start) + source.slice(start, end).replace(/[^\r\n]/g, 'x');
      cursor = end;
    }
    frame += mathFrame.slice(cursor);
    frame = frame.replace(/(?<!\\)\\r\\n|(?<!\\)\\n/g, match => ' '.repeat(match.length - 1) + '\n');
    const completeRows = [];
    const collect = (node, width = 0) => {
      if (node.type === 'table') width = node.children[0].children.length;
      if (node.type === 'tableRow') {
        const start = node.position.start.offset, end = node.position.end.offset;
        framedRows.push([start, end]);
        // 수식의 제어 단어(예: \\nleqq)를 가상의 개행으로 나눈 조각은 새
        // 표 행이 아니다. 파서로 독립된 행의 열 수를 검증하며, 명령 이름은
        // 해석하지 않는다. 충분한 열을 가진 새 행만 기존 원자의 경계를 끊는다.
        const heading = Array(width).fill('h').join(' | ') + '\n' + Array(width).fill('---').join(' | ') + '\n';
        const row = parse(heading + source.slice(start, end)).children[0]?.children?.[1];
        if (row?.type === 'tableRow' && row.children.length >= width) completeRows.push([start, end]);
      }
      for (const child of node.children ?? []) collect(child, width);
    };
    structure = parse(frame);
    // 인라인 코드·링크가 머리글 시작부터 구분 행까지 소유한 경우에는 코드
    // 예시 안의 직렬화 표다. 이미 표 셀 안에서 시작한 원자만 새 행 경계에
    // 종속된다. 문서의 리터럴 컨테이너를 복구 과정에서 표로 승격하지 않는다.
    const envelopes = [];
    const envelope = node => {
      if (['code', 'inlineCode', 'html', 'definition', 'link', 'image', 'inlineMath', 'math'].includes(node.type))
        envelopes.push([node.position.start.offset, node.position.end.offset]);
      for (const child of node.children ?? []) envelope(child);
    };
    envelope(tree);
    const retain = node => {
      if (node.type === 'table') {
        const header = node.children[0].position;
        if (envelopes.some(([start, end]) => start >= header.start.offset && start < header.end.offset &&
          end > header.end.offset && !frame.slice(header.start.offset, start).includes('|'))) return null;
      }
      return node.children ? { ...node, children: node.children.map(retain).filter(Boolean) } : node;
    };
    structure = retain(structure);
    collect(structure);
    for (const span of formulas) if (!completeRows.some(([start, end]) => start > span.start && start < span.end &&
      source.slice(start, Math.min(end, span.end)).includes('|')))
      ranges.push([span.start, span.end]);
    tree = tableOwnership(source, tree, structure, parse);
    for (const span of visualizations) if (framedRows.some(([start, end]) => span.start >= start && span.end <= end))
      ranges.push([span.start, span.end]);
  }
  const visit = node => {
    if (['code', 'inlineCode', 'table', 'html', 'definition', 'link', 'image', 'inlineMath', 'math'].includes(node.type)) {
      const start = node.position.start.offset, end = node.position.end.offset;
      ranges.push([start, end]);
      if (!['inlineCode', 'inlineMath', 'math'].includes(node.type)) scanLiterals.push([start, end]);
      return;
    }
    for (const child of node.children ?? []) visit(child);
  };
  visit(tree);
  const scanRanges = mergeLiteralRanges(scanLiterals);
  // 이스케이프된 백틱 확장은 첫 AST에서 일반 글자·br 조각이다. 화면과 서버의
  // 공통 코드 경계로 보호해야 내부 개행이 바깥 문서의 물리적 줄이 되지 않는다.
  for (const line of source.matchAll(/[^\r\n]+/g)) {
    if (!/`[ \t]*(?:chart|mermaid)(?=(?:\\r)?\\n|\\?<br\s*\/?>)/i.test(line[0])) continue;
    // 개행 복구 전에도 행을 넘는 미완성 수식·백틱이 이 행의 코드를 가리면 안 된다.
    for (const span of inlineCodeSpans(line[0], scanRanges, line.index))
      if (/^(?:chart|mermaid)(?=(?:\\r)?\\n|\\?<br\s*\/?>)/i.test(span.value.trim()))
        ranges.push([line.index + span.start, line.index + span.end]);
  }
  // 양끝 파이프가 없는 표는 다음 행이 영문으로 시작할 수 있다. 직렬화된
  // 구분 행이 있는 문서에서만 그 개행도 복구하며, 수식 본문은 먼저 보호한다.
  if (serializedTable) for (const span of collectMathSpans(tree, source, parse, structure).candidates)
    ranges.push([span.start, span.end]);
  // 직렬화된 CRLF를 한 번에 복구해야 표 구분 행 끝에 문자 \\r이 남지 않는다.
  // 수식은 위 소유 범위에서 보호했다. 표 행 첫 글자가 u·eq 등이어도 그 앞의
  // 개행은 TeX 명령이 아니다. 여기서 명령 이름을 검사하면 정상 행이 합쳐진다.
  const decode = (text, offset) => (serializedTable ? text.replace(/(?<!\\)\\r\\n|(?<!\\)\\n/g, '\n') : text).replace(/(?<!\\)\\r\\n|(?<!\\)\\n\\n|(?<!\\)\\n(?=[^A-Za-z]|$)/g,
    match => match === '\\n\\n' ? '\n\n' : '\n')
    .replace(/^([ \t]{0,3})\\>(?=\s|$)/gm, (match, indent, index) =>
      index > 0 || offset === 0 || /[\r\n]/.test(source[offset - 1]) ? indent + '>' : match);
  let out = '', at = 0;
  for (const [start, end] of mergeLiteralRanges(ranges)) {
    out += decode(source.slice(at, start), at) + source.slice(start, end);
    at = end;
  }
  out += decode(source.slice(at), at);
  return out;
}
