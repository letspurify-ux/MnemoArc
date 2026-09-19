// 수식이 소유한 원문 범위를 서버와 화면에서 같은 규칙으로 판정한다.
// parse는 호출자의 Markdown 파서이며, 여기서는 AST나 원문을 변경하지 않는다.
import { mathEnvironments, verbEnd, texGroupEnds } from './tex-environments.mjs';
import { incompleteTableMath } from './table-math.mjs';

export const MAX_MATH_SPAN = 5000;
const MATH_LANGUAGES = new Set(['math', 'latex', 'tex']);
const SLASH = /[\\₩￦＼]/;
// 실제 원화 금액(₩ 100)은 유지하고, 수학 기호 앞의 제어 공백(₩ x)도 복구한다.
export const normalizeMath = tex => tex.replace(/[₩￦＼]{2}|[₩￦＼](?=[A-Za-z()[\]{}\\,;:!%$|_#&^~]|[ \t](?![ \t]*\d))/g,
  match => '\\'.repeat(match.length));
const isLetter = c => /[A-Za-z0-9]/.test(c ?? '');
const cjk = /[ᄀ-ᇿ぀-ヿ㄰-㆏一-鿿가-힣]/;
const escapedAt = (s, i) => {
  let n = 0;
  while (i > 0 && SLASH.test(s[--i])) n++;
  return n % 2 === 1;
};

// Markdown 수식 구분자와 TeX 문법 검증은 별개다. 중괄호 오류가 있어도 수식 범위는 보호해야
// 내부 |가 표를 쪼개지 않는다. 명령의 인자 끝(})을 찾을 때만 중괄호 깊이를 검사한다.
// 길이를 제한하므로 스트리밍 도중 닫히지 않은 시작 기호가 반복되어도 탐색량이 제한된다.
export function closingMathDelimiter(source, start, closing, end = source.length) {
  let depth = 0, comment = false;
  const groups = closing === '}';
  const limit = Math.min(end, start + MAX_MATH_SPAN);
  for (let i = start; i < limit; i++) {
    const c = source[i];
    if (comment) { if (c === '\n' || c === '\r') comment = false; continue; }
    if (!depth && source.startsWith(closing, i)) return i;
    // 뒤에서 새 수식이 시작되면 그 수식의 닫는 표시를 앞 오류에 빌려주지 않는다.
    if ((closing === '\\]' && source.startsWith('\\[', i)) ||
      (closing === '\\)' && source.startsWith('\\(', i))) return -1;
    if (c === '%') { comment = true; continue; }
    if (SLASH.test(c)) {
      const verb = verbEnd(source, i);
      i = verb !== i ? verb : i + 1;
      continue;
    }
    if (groups && c === '{') depth++;
    if (groups && c === '}') depth--;
    if (depth < 0) return -1;
  }
  return -1;
}

function commandEnd(source, start, names) {
  const re = new RegExp(`\\\\(?:${names})\\*?\\s*\\{`, 'y');
  re.lastIndex = start;
  const match = re.exec(source);
  if (!match) return -1;
  const close = closingMathDelimiter(source, re.lastIndex, '}');
  return close >= 0 && source.slice(re.lastIndex, close).trim() ? close + 1 : -1;
}

// 수식 코드펜스 안에 모델이 다시 넣은 구분자는 한 겹만 벗긴다. 본문의 $·중괄호는 유지한다.
export function unwrapMath(tex) {
  const source = normalizeMath(tex).trim();
  for (const [open, close] of [['$$', '$$'], ['\\[', '\\]'], ['\\(', '\\)'], ['$', '$']]) {
    if (source.startsWith(open) && closingMathDelimiter(source, open.length, close) === source.length - close.length)
      return source.slice(open.length, -close.length).trim();
  }
  return source;
}

const offsets = node => [node.position?.start?.offset, node.position?.end?.offset];
export function collectMathSpans(tree, source, parse, structure = tree, atomicRanges = []) {
  const normalized = normalizeMath(source); // 길이가 같으므로 원문 좌표가 유지된다.
  const protectedRanges = [], noBare = [], candidates = [], contexts = [], tableRows = [], textScopes = [], tagScopes = [], referenceEdits = [];
  const tableSeparators = [], blockCodes = [], knownLinks = new Set(), definitions = new Map();
  let hasDefinitions = false;
  const contextAt = start => {
    for (let i = contexts.length - 1; i >= 0; i--)
      if (contexts[i].start <= start && contexts[i].end > start) return contexts[i];
    return { quotes: 0, boundaryEnd: source.length };
  };
  const protect = (start, end) => { if (start !== undefined && end !== undefined) protectedRanges.push({ start, end }); };
  for (const { start, end } of atomicRanges) protect(start, end);
  let groups;
  // 모든 범위는 [start, end)다. 최초 AST와 후속 스캐너가 같은 소유권
  // 판정을 사용해야, 수신 조각의 끝이나 다시 파싱한 경계에서 뜻이 바뀌지 않는다.
  const owns = (range, offset) => !!range && range.start <= offset && offset < range.end;
  const conflictsWithAtom = (start, end) => atomicRanges.some(range => {
    if (range.end <= start || range.start >= end) return false;
    if (owns(range, start) || owns(range, end - 1)) return true;
    // TeX 인자 안의 코드 모양 글자는 TeX 원문이다. 독립된 시각화는
    // 바깥 수식의 오류 복구 영역에 포함되지 않는다. 명령 이름은 해석하지 않는다.
    groups ??= texGroupEnds(source);
    return ![...groups].some(([open, close]) => open >= start && open < range.start && close >= range.end && close < end);
  });
  // 구조 좌표와 값의 출처를 분리한다. 표 발견용 AST의 가려진 글자를 TeX 값으로
  // 읽지 않으면서, 새로 발견한 표에서도 수식이 다른 행·셀을 삼키지 않도록 한다.
  const inspectStructure = (node, quotes = 0, inTable = false, tableWidth = 0, boundaryEnd = source.length) => {
    const [start, end] = offsets(node);
    if (node.type === 'blockquote') quotes++;
    if (['blockquote', 'listItem', 'footnoteDefinition'].includes(node.type)) boundaryEnd = Math.min(boundaryEnd, end);
    if (node.type === 'table') {
      inTable = true; tableWidth = node.children[0].children.length;
      tableSeparators.push({ start: offsets(node.children[0])[1], end: node.children[1] ? offsets(node.children[1])[0] : end });
    }
    if (node.type === 'tableRow') tableRows.push({ start, end, width: tableWidth });
    if (['paragraph', 'heading', 'tableRow'].includes(node.type)) textScopes.push({ start, end });
    if (['paragraph', 'heading', 'tableCell'].includes(node.type) && node.children?.length) {
      tagScopes.push({ start: offsets(node.children[0])[0], end: offsets(node.children.at(-1))[1], quotes });
    }
    if (start !== undefined) contexts.push({ start, end, quotes, inTable, boundaryEnd });
    for (const child of node.children ?? []) inspectStructure(child, quotes, inTable, tableWidth, boundaryEnd);
  };
  inspectStructure(structure);
  const inspect = node => {
    const [start, end] = offsets(node);
    if (node.type === 'definition') {
      hasDefinitions = true;
      if (!definitions.has(node.identifier)) definitions.set(node.identifier, node);
    }
    if (node.type === 'code') {
      protect(start, end);
      blockCodes.push({ start, end });
      if (MATH_LANGUAGES.has(node.lang?.toLowerCase())) candidates.push({ start, end, value: unwrapMath(node.value), display: true });
      return;
    }
    if (['inlineCode', 'image', 'imageReference', 'definition', 'html', 'footnoteReference'].includes(node.type)) { protect(start, end); return; }
    if (node.type === 'footnoteDefinition') {
      // 정의의 이름은 본문이 아니다. 각주 본문 속 수식은 계속 검사한다.
      const labelEnd = source.indexOf(']:', start);
      if (labelEnd >= start && labelEnd < end) protect(start, labelEnd + 2);
    }
    if (node.type === 'link' || node.type === 'linkReference') {
      knownLinks.add(start);
      if (source[start] !== '[') { protect(start, end); return; }
      const [labelStart] = offsets(node.children[0] ?? {});
      const [, labelEnd] = offsets(node.children.at(-1) ?? {});
      if (labelStart === undefined) { protect(start, end); return; }
      protect(start, labelStart); protect(labelEnd, end);
      noBare.push({ start, end });
      // 참조 식별자 자체의 |도 표를 나눌 수 있다. 두 번째 파싱에서는 안전한 별칭을 쓰고
      // 그 별칭의 정의에 원래 URL·제목을 연결한다. 표시 글자는 원문에서 따로 복원한다.
      if (node.type === 'linkReference') {
        referenceEdits.push({ start: node.referenceType === 'shortcut' ? end
          : node.referenceType === 'collapsed' ? end - 2 : source.indexOf(']', labelEnd) + 1,
        end, linkStart: start, referenceId: node.identifier });
      }
    }
    if (node.type === 'math' || node.type === 'inlineMath') {
      // 첫 Markdown 파싱이 확장 코드의 내부 구분자로 수식을 닫았으면
      // 그 수식 노드도 잠정 판정이다. 바깥 스캐너가 같은 원자 경계로 다시 읽는다.
      if (conflictsWithAtom(start, end)) return;
      protect(start, end);
      const raw = source.slice(start, end);
      if (node.type === 'math') {
        const lines = raw.split(/\r\n?|\n/);
        const fence = /^\${2,}/.exec(lines[0])?.[0] ?? '$$';
        const closing = /^[ \t>]*(\${2,})[ \t]*$/.exec(lines.at(-1));
        const bodyLines = node.value ? node.value.split(/\r\n?|\n/).length : 0;
        if (!closing || closing[1].length < fence.length || lines.length < bodyLines + 2) {
          // 완성되지 않은 $$를 다시 Markdown으로 파싱하면 뒤 설명을 또 삼킨다. 원문 노드로 보존한다.
          const head = node.meta ? `${fence} ${node.meta}` : fence;
          candidates.push({ start, end, literal: node.value ? `${head}\n${node.value}` : head });
          return;
        }
      }
      candidates.push({ start, end, value: node.meta ? `${node.meta}\n${node.value}`.trim() : node.value, display: true });
      return;
    }
    for (const child of node.children ?? []) inspect(child);
  };
  inspect(tree);
  // 첫 표 파싱은 수식 안의 |에서도 셀을 나눠 링크 전체를 놓칠 수 있다. 구분 행만 같은
  // 길이의 공백으로 바꾼 분석에서 링크 원문 좌표를 보충한다. 실제로 그릴 표는 바꾸지 않는다.
  if (tableSeparators.length && (hasDefinitions || /\]\s*\(/.test(source))) {
    let linkSource = '', cursor = 0;
    for (const range of tableSeparators) {
      linkSource += source.slice(cursor, range.start) + source.slice(range.start, range.end).replace(/[|:-]/g, ' ');
      cursor = range.end;
    }
    linkSource += source.slice(cursor);
    const collect = node => {
      const [start, end] = offsets(node);
      if (['link', 'linkReference'].includes(node.type) && !knownLinks.has(start) &&
        (node.type === 'link' || definitions.has(node.identifier)) &&
        tableRows.some(row => row.start <= start && row.end >= end)) {
        // [수식][ref]의 뒷부분만 독립된 [ref]로 읽었던 보정은 폐기한다.
        for (let i = referenceEdits.length - 1; i >= 0; i--)
          if (referenceEdits[i].linkStart > start && referenceEdits[i].linkStart < end) referenceEdits.splice(i, 1);
        inspect(node);
        return;
      }
      for (const child of node.children ?? []) collect(child);
    };
    collect(parse(linkSource));
  }
  protectedRanges.sort((a, b) => a.start - b.start);
  // 보충한 링크 주소가 기존 보호 범위를 포함할 수 있다. 이진 탐색의 끝 좌표도 단조롭게 유지한다.
  let rangeCount = 0;
  for (const range of protectedRanges) {
    const previous = protectedRanges[rangeCount - 1];
    if (previous && range.start <= previous.end) previous.end = Math.max(previous.end, range.end);
    else protectedRanges[rangeCount++] = range;
  }
  protectedRanges.length = rangeCount;
  const firstRange = start => {
    let lo = 0, hi = protectedRanges.length;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      if (protectedRanges[mid].end <= start) lo = mid + 1;
      else hi = mid;
    }
    return protectedRanges[lo];
  };
  const intersects = (start, end) => (firstRange(start)?.start ?? Infinity) < end;
  let rowIndex = 0, scopeIndex = 0;
  const texAt = (start, end) => {
    if (!/[\r\n]/.test(normalized.slice(start, end))) return normalized.slice(start, end).trim();
    const { quotes } = contextAt(start);
    return normalized.slice(start, end).split(/\r\n?|\n/).map((line, i) => {
      if (!i) return line;
      for (let q = 0; q < quotes; q++) line = line.replace(/^[ \t]*> ?/, '');
      return line.replace(/^[ \t]+/, '');
    }).join('\n').trim();
  };
  // 코드 예시 안의 미완성 begin이 뒤의 실제 환경을 중첩으로 붙잡지 않도록 경계 검사에도 가린다.
  let scanSource = '', protectedEnd = 0;
  const scanRanges = [...protectedRanges, ...noBare].sort((a, b) => a.start - b.start);
  for (const range of scanRanges) {
    if (range.end <= protectedEnd) continue;
    scanSource += normalized.slice(protectedEnd, Math.max(protectedEnd, range.start));
    scanSource += normalized.slice(Math.max(protectedEnd, range.start), range.end).replace(/[^\r\n]/g, ' ');
    protectedEnd = range.end;
  }
  scanSource += normalized.slice(protectedEnd);
  const environments = new Map(mathEnvironments(scanSource, { textContext: true, includeUnknown: true })
    .filter(span => span.end - span.start <= MAX_MATH_SPAN).map(span => [span.start, span]));
  for (let i = 0; i < source.length; i++) {
    if (!['$', '\\'].includes(normalized[i])) continue;
    const protectedRange = firstRange(i);
    if (owns(protectedRange, i)) { i = protectedRange.end - 1; continue; }
    if (escapedAt(source, i)) continue;
    while (rowIndex < tableRows.length && tableRows[rowIndex].end <= i) rowIndex++;
    const row = tableRows[rowIndex];
    const inRow = row && row.start <= i;
    // 표 셀은 한 물리적 행 안에 있다. 다음 행의 $를 닫는 구분자로 빌려오지 않는다.
    const limit = Math.min(inRow ? row.end : source.length, contextAt(i).boundaryEnd);
    while (scopeIndex < textScopes.length && textScopes[scopeIndex].end <= i) scopeIndex++;
    const scope = textScopes[scopeIndex];
    let start = i, end = -1, contentStart = i, contentEnd = i, display = false, bare = false;
    const env = environments.get(i);
    if (env && env.end <= limit) { end = env.end; contentEnd = end; display = true; bare = true; }
    else if (normalized.startsWith('\\(', i) || normalized.startsWith('\\[', i)) {
      display = normalized[i + 1] === '[';
      contentStart = i + 2;
      contentEnd = closingMathDelimiter(normalized, contentStart, display ? '\\]' : '\\)', limit);
      if (contentEnd >= 0) end = contentEnd + 2;
    } else if (normalized.startsWith('$$', i)) {
      const fence = /^\${2,}/.exec(normalized.slice(i))[0];
      contentStart = i + fence.length;
      contentEnd = closingMathDelimiter(normalized, contentStart, fence, limit);
      if (contentEnd >= 0) { end = contentEnd + fence.length; display = true; }
    } else if (normalized[i] === '$' && normalized[i + 1] !== '$' &&
      (normalized[i - 1] !== '$' || candidates.at(-1)?.end === i)) {
      contentStart = i + 1;
      // 통화의 $와 다음 문단·표에 있는 수식의 $를 연결하지 않는다.
      contentEnd = closingMathDelimiter(normalized, contentStart, '$', scope && scope.start <= i ? Math.min(limit, scope.end) : limit);
      if (contentEnd >= 0) {
        const tex = normalized.slice(contentStart, contentEnd).trim();
        const nextOpener = /\s/.test(normalized[contentEnd - 1]) && (normalized[contentEnd + 1] === '\\' ||
          (inRow && /^[ \t]+(?:\\[A-Za-z]+|[A-Za-z][ \t]*[=^_])/.test(normalized.slice(contentEnd + 1, limit))));
        if (tex && !nextOpener && !isLetter(source[i - 1]) && !isLetter(source[contentEnd + 1]) &&
          (!cjk.test(tex) || /[\\=+\-*/^_<>]/.test(tex))) end = contentEnd + 1;
      }
    } else {
      end = commandEnd(normalized, i, 'ce|pu');
      contentEnd = end; bare = true;
      // 완전한 단독 명령은 별행으로, 문장 속 화학식은 인라인으로 표시한다.
      if (end > 0) display = !source.slice(source.lastIndexOf('\n', i - 1) + 1, i).trim() &&
        !source.slice(end, source.indexOf('\n', end) < 0 ? source.length : source.indexOf('\n', end)).trim();
    }
    // 시작/끝이 코드·주소 안에 있으면 제외한다. 수식에 완전히 포함된 `·[링크]는 TeX 본문이다.
    const endRange = firstRange(end - 1);
    if (end < 0 || end > limit || conflictsWithAtom(start, end) || blockCodes.some(r => r.start > start && r.start < end) ||
      // end는 마지막 문자 다음 좌표다. 수신 조각 끝과 원자 끝이 같아도
      // 마지막 닫는 기호는 그 원자의 소유이며 바깥 수식을 닫을 수 없다.
      owns(endRange, end - 1) ||
      (bare && noBare.some(r => r.start <= start && r.end >= end))) continue;
    candidates.push({ start, end, value: texAt(contentStart, contentEnd), display });
    i = end - 1;
  }
  // 닫는 표시가 없는 TeX 입력도 셀 단위로 보존한다. 완성된 이웃 수식·표의 열은 유지한다.
  candidates.push(...incompleteTableMath(normalized, tableRows, [...protectedRanges, ...noBare], candidates));
  // 구분자 밖으로 떨어진 식 번호도 바로 앞 수식에만 연결한다.
  for (const span of candidates) {
    if (span.literal !== undefined || span.incomplete) continue;
    const limit = Math.min(contextAt(span.start).boundaryEnd, span.end + MAX_MATH_SPAN);
    const after = normalized.slice(span.end, limit).search(/\\tag\*?\s*\{/);
    if (after < 0) continue;
    const start = span.end + after;
    // 같은 인용문 안의 줄바꿈은 공백이다. 다른 목록·인용문으로 넘어가 번호를 빌려오지 않는다.
    if (texAt(span.end, start)) continue;
    const end = commandEnd(normalized, start, 'tag');
    if (end > 0 && end <= limit && !intersects(start, end)) {
      span.value += ' ' + normalized.slice(start, end);
      span.end = end; span.display = true;
    }
  }
  // 식 번호 복구도 Markdown 본문 범위 안에서만 한다. 문서 전체의 줄을 대상으로 하면
  // 인용의 >·목록의 -를 수학 기호로 삼키고, 번호 목록의 1.은 산문으로 오인한다.
  // 첫 줄의 컨테이너 구분자는 AST 좌표가 제외한다. 이어지는 줄에서는 실제 인용 깊이만 벗긴다.
  // 번호를 먼저 찾는다. 식 전체의 탐욕적 정규식은 =가 반복되는 정상 텍스트에서도 제곱 비용이 든다.
  const tagged = /\\tag\*?[ \t]*\{[^{}\r\n]+\}[ \t]*$/;
  for (const scope of tagScopes) for (const line of normalized.slice(scope.start, scope.end).matchAll(/[^\r\n]+/g)) {
    let text = line[0];
    if (line.index > 0) for (let q = 0; q < scope.quotes; q++) text = text.replace(/^[ \t]*> ?/, '');
    text = text.replace(/^[ \t]+/, '');
    const match = tagged.exec(text);
    if (!match || !/[=+^_<>]/.test(text.slice(0, match.index))) continue;
    const start = scope.start + line.index + line[0].length - text.length, end = start + text.length;
    const expression = text.slice(0, match.index).replace(/\\[A-Za-z]+/g, '').replace(/\d+(?:\.\d*)?|\.\d+/g, '0');
    if (!cjk.test(expression) && !/[A-Za-z]{3,}|[.!?:]/.test(expression) &&
      !escapedAt(normalized, start + text.lastIndexOf('\\tag')) && !intersects(start, end) &&
      !candidates.some(span => span.start < end && span.end > start))
      candidates.push({ start, end, value: text.trim(), display: true });
  }
  return { candidates, referenceEdits, definitions };
}
