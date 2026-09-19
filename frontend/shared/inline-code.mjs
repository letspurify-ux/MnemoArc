// CommonMark 인라인 코드의 닫는 백틱은 여는 백틱과 길이가 정확히 같아야 한다.
// 바깥 코드부터 소비하므로 그 안의 시각화 예시를 실행하지 않는다. 서버의 데이터
// 주입과 브라우저의 표 보호가 같은 경계를 사용한다.
const escapedAt = (source, index) => {
  let count = 0;
  while (index > 0 && source[--index] === '\\') count++;
  return count % 2 === 1;
};
const indexedRuns = (source, pattern, ranges, offset) => {
  const runs = [...source.matchAll(pattern)].map(match => ({
    start: match.index, end: match.index + match[0].length, fence: match[0],
  })).filter(run => !codeSpanInLiteral(run, ranges, offset));
  const next = new Map();
  for (let i = runs.length - 1; i >= 0; i--) {
    runs[i].close = next.get(runs[i].fence);
    next.set(runs[i].fence, runs[i]);
  }
  return runs;
};

export function* inlineCodeSpans(source, ranges = [], offset = 0, { visualizationCandidates = false, includeUnclosed = false } = {}) {
  // 주소·속성에 있는 홑백틱을 먼저 제외한다. 짝을 만든 뒤 제외하면 그 백틱이
  // 이웃 시각화의 여는 기호를 닫는 기호로 소비해 오른쪽 셀까지 잃는다.
  const runs = indexedRuns(source, /`+/g, ranges, offset);
  // 기존 확장 표기인 \`\`\`chart\n…\`\`\`도 지원한다. 일반 Markdown의
  // 이스케이프 백틱은 여는 기호가 아니며, 코드 내부의 백슬래시는 문자 그대로다.
  const serialized = new Map(indexedRuns(source, /(?:\\`)+/g, ranges, offset).map(run => [run.start + 1, run]));
  let consumed = 0;
  for (const run of runs) {
    if (run.start < consumed) continue;
    let open = run;
    if (escapedAt(source, run.start)) {
      open = serialized.get(run.start);
      if (!open || !escapedAt(source, open.start + 1) ||
        !/^[ \t]*(?:chart|mermaid)(?=(?:\\r)?\\n|\\?<br\s*\/?>)/i.test(source.slice(open.end))) continue;
    }
    // 구조 발견용 후보 수집에서는 미완성 일반 코드의 짝을 먼저 확정하지 않는다.
    // 실행 여부와 실제 소유 범위는 호출자가 확정된 Markdown 컨테이너에서 판정한다.
    if (visualizationCandidates && !/^[ \t]*(?:chart|mermaid)(?=(?:\\r)?\\n|\\?<br\s*\/?>)/i.test(source.slice(open.end))) continue;
    if (!open.close) {
      if (visualizationCandidates && includeUnclosed) yield { start: open.start, end: source.length,
        bodyStart: open.end, bodyEnd: source.length, value: source.slice(open.end), incomplete: true };
      continue;
    }
    consumed = open.close.end;
    yield { start: open.start, end: consumed, bodyStart: open.end, bodyEnd: open.close.start,
      value: source.slice(open.end, open.close.start) };
  }
}

// Markdown 파서가 확정한 주소·HTML·코드블록은 직렬화 시각화로 재해석하지 않는다.
// 링크의 표시 글자는 확장을 허용하지만 목적지·참조 이름은 그대로 둔다.
export function markdownLiteralRanges(tree, source) {
  const ranges = [];
  const pending = [tree];
  while (pending.length) {
    const node = pending.pop();
    const { start, end } = node.position ?? {};
    const literalCode = node.type === 'inlineCode' &&
      !/^(?:chart|mermaid)(?=(?:\\r)?\\n|\\?<br\s*\/?>)/i.test(node.value.trim());
    if (literalCode || ['code', 'html', 'image', 'imageReference', 'definition', 'footnoteReference', 'math', 'inlineMath'].includes(node.type)) {
      if (start && end) ranges.push([start.offset, end.offset]);
      continue;
    }
    if (node.type === 'link' || node.type === 'linkReference') {
      // 자동 링크에는 별도의 표시문이 없다. 표시 글자도 주소 자체이므로
      // 그 안의 백틱을 실행하면 화면과 href 양쪽이 함께 변형된다.
      if (start && end && source[start.offset] !== '[') {
        ranges.push([start.offset, end.offset]);
        continue;
      }
      const labelEnd = node.children.at(-1)?.position?.end.offset;
      if (labelEnd !== undefined && end) ranges.push([labelEnd, end.offset]);
    }
    if (node.type === 'footnoteDefinition' && start && end) {
      ranges.push([start.offset, node.children[0]?.position?.start.offset ?? end.offset]);
    }
    for (const child of node.children ?? []) pending.push(child);
  }
  return mergeLiteralRanges(ranges);
}

export function mergeLiteralRanges(ranges) {
  const merged = [];
  for (const range of ranges.sort((a, b) => a[0] - b[0])) {
    const last = merged.at(-1);
    if (last && range[0] <= last[1]) last[1] = Math.max(last[1], range[1]);
    else merged.push(range);
  }
  return merged;
}

export function codeSpanInLiteral(span, ranges, offset = 0) {
  const contains = index => {
    let low = 0, high = ranges.length;
    while (low < high) {
      const mid = (low + high) >>> 1;
      if (ranges[mid][1] <= index) low = mid + 1;
      else high = mid;
    }
    return low < ranges.length && ranges[low][0] <= index;
  };
  // 확장 코드가 감싼 <br>은 코드의 개행 표기다. 포함된 HTML 조각만으로 코드를
  // 제외하지 않고, 여닫는 기호 자체가 주소·속성 등에 속하는지 판정한다.
  return contains(span.start + offset) || contains(span.end + offset - 1);
}
