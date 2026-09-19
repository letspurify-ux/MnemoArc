import { collectMathSpans } from './math-spans.mjs';
import { inlineCodeSpans, markdownLiteralRanges, mergeLiteralRanges, codeSpanInLiteral } from './inline-code.mjs';
import { texGroupEnds } from './tex-environments.mjs';

// 표의 인라인 문법은 셀마다 새로 시작한다. 표를 끈 문서·행의 inlineCode나
// image는 이웃 셀을 가로지를 수 있다. 구조 투영이 확정한 셀을 원문으로
// 분석하고 정의만 공유한다. AST 값은 원문, 위치는 문서 좌표로 유지한다.
export function tableOwnership(source, tree, structure, parse) {
  const rows = [], cells = [], definitions = [];
  const visit = (node, type, output) => {
    if (node.type === type) output.push(node);
    for (const child of node.children ?? []) visit(child, type, output);
  };
  visit(structure, 'tableRow', rows);
  if (!rows.length) return tree;
  visit(structure, 'tableCell', cells);
  visit(tree, 'definition', definitions);
  visit(tree, 'footnoteDefinition', definitions);
  const declarations = definitions.map(node => source.slice(node.position.start.offset, node.position.end.offset)).join('\n\n');
  const ranges = rows.map(row => [row.position.start.offset, row.position.end.offset]);
  const outside = node => {
    const start = node.position?.start.offset, end = node.position?.end.offset;
    // 링크 등 원자 노드가 새 블록 경계를 가로지르면 그 짝 자체가 무효다.
    if ((!node.children || ['link', 'linkReference'].includes(node.type)) &&
      ranges.some(([a, b]) => start < b && end > a)) return null;
    return node.children ? { ...node, children: node.children.map(outside).filter(Boolean) } : node;
  };
  const ownership = outside(tree);
  for (const cell of cells) {
    const start = cell.position.start.offset, end = cell.position.end.offset;
    // 인라인 컨테이너이므로 #, >, 들여쓰기, 백틱을 새 블록의 시작으로
    // 읽지 않게 한다. 접두사는 분석에만 쓰며 표시·치환 원문에 들어가지 않는다.
    const row = 'x ' + source.slice(start, end);
    const parsed = parse(row + (declarations ? '\n\n' + declarations : ''));
    const relocate = node => {
      if (node.position) for (const point of [node.position.start, node.position.end]) point.offset += start - 2;
      for (const child of node.children ?? []) relocate(child);
      return node;
    };
    ownership.children.push(...parsed.children.filter(node => node.position.end.offset <= row.length).map(relocate));
  }
  return ownership;
}

// 표의 존재를 판정하기 전에 내부 언어가 소유한 열 구분자를 가린다. 머리글의
// 수식·시각화에도 |가 올 수 있으므로 첫 GFM AST의 table 여부에 의존할 수 없다.
// 표 문법 자체(열 수, 구분 행, 목록·인용·각주 경계)는 계속 Markdown 파서가 판정한다.
// 이 AST는 구조와 좌표만 제공한다. 원문 길이·개행을 그대로 두고 |만 치환하므로
// 모든 위치는 원문 좌표이며, 표시 값과 조회 치환에는 반드시 원문을 사용한다.
export function analyzeRichStructure(source, tree, parse, parseWithoutTables, atomicRanges = []) {
  const candidates = [];
  if (/`[ \t]*(?:chart|mermaid)(?=(?:\\r)?\\n|\\?<br\s*\/?>)/i.test(source)) {
    for (const line of source.matchAll(/[^\r\n]+/g))
      for (const span of inlineCodeSpans(line[0], [], 0, { visualizationCandidates: true, includeUnclosed: true }))
        candidates.push({ start: line.index + span.start, end: line.index + span.end, incomplete: span.incomplete });
  }
  // 미완성 시각화도 라벨의 수식 표시를 바깥에 빌려주지 않는다. 단, 이미
  // 완성된 TeX 인자에 적힌 짝 없는 백틱은 그 TeX의 문자다. 후보에는 실행권이 없다.
  if (candidates.some(span => span.incomplete)) {
    const initial = collectMathSpans(tree, source, parse);
    const groups = texGroupEnds(source);
    for (let i = candidates.length - 1; i >= 0; i--) {
      const span = candidates[i];
      if (span.incomplete && initial.candidates.some(math => [...groups].some(([open, close]) =>
        open >= math.start && close < math.end && open < span.start && close > span.start))) candidates.splice(i, 1);
    }
  }
  // 원자가 완전히 수식 안에 들어가면 TeX가 소유할 수 있지만 수식의 닫는
  // 구분자가 시각화 내부에 있으면 그 짝은 무효다. 초기 GFM이 코드를 |에서
  // 잘랐더라도 Mermaid 라벨의 $$를 바깥 수식의 닫는 표시로 빌리지 않는다.
  const math = collectMathSpans(tree, source, parse, tree, [...atomicRanges, ...candidates]);
  if (!source.includes('|')) return { structure: tree, ownership: tree, ...math };
  const ranges = math.candidates.map(({ start, end }) => [start, end]);
  const project = ranges => {
    let projected = '', cursor = 0;
    for (const [start, end] of mergeLiteralRanges(ranges)) {
      projected += source.slice(cursor, start) + source.slice(start, end).replaceAll('|', 'x');
      cursor = end;
    }
    return projected + source.slice(cursor);
  };
  let proposal, proposed;
  const completeCandidates = candidates.filter(span => !span.incomplete);
  if (completeCandidates.length) {
    // 후보는 구조 발견에만 쓴다. 첫 GFM의 잘린 셀이나 아직 행·셀이 없는
    // 문서의 코드 짝으로 판정하면 순환 의존이 생긴다. 내부 |를 투영한 뒤
    // 원문의 셀을 읽어 주소·HTML·일반 코드가 실제로 소유한 후보를 제외한다.
    proposed = project([...ranges, ...completeCandidates.map(({ start, end }) => [start, end])]);
    proposal = proposed === source ? tree : parse(proposed);
    const literals = markdownLiteralRanges(tableOwnership(source, parseWithoutTables(source), proposal, parse), source);
    for (const span of completeCandidates) if (!codeSpanInLiteral(span, literals)) ranges.push([span.start, span.end]);
  }
  const projected = project(ranges);
  const structure = projected === source ? tree : projected === proposed ? proposal : parse(projected);
  const ownership = tableOwnership(source, tree, structure, parse);
  // 후보로 구조를 발견한 뒤 확정된 컨테이너에서 수식 경계를 결정한다. 단계를
  // 반복하는 고정점 복구가 아니며, 미완성 수식에도 본문과 같은 셀 경계를 적용한다.
  return { structure, ownership, ...(structure === tree && ownership === tree ? math
    : collectMathSpans(ownership, source, parse, structure, [...atomicRanges, ...candidates])) };
}
