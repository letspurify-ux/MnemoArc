// Mermaid가 본문에서 제외하는 frontmatter·지시문·주석에는 수식 라벨이 없다.
// 원문과 같은 길이로 가려 위치를 유지하고, 실제 표현식만 치환한다.
// 경계는 설치된 Mermaid의 preprocess/frontmatter/comments 규칙을 따른다.
import { decodeString } from 'micromark-util-decode-string';
import { executableMermaidSource, mermaidMathExpressions, scopedMermaidMathExpressions } from '../../shared/mermaid-math-spans.mjs';
export { mermaidMathExpressions } from '../../shared/mermaid-math-spans.mjs';

export function replaceMermaidMath(source, render, spans = mermaidMathExpressions(source)) {
  let out = '', at = 0;
  for (const span of spans) {
    out += source.slice(at, span.start) + render(span.value);
    at = span.end;
  }
  return out + source.slice(at);
}

const parts = diagram => {
  const vertices = [...diagram.db.getVertices().values()];
  const edges = diagram.db.getEdges(), groups = diagram.db.getSubGraphs();
  return {
    labels: [...vertices.map(node => node.text), ...edges.map(edge => edge.text), ...groups.map(group => group.title)],
    ids: [...vertices.map(node => node.id), ...edges.map(edge => edge.id), ...groups.map(group => group.id)],
  };
};

// 텍스트 치환이 그래프 자체를 바꿀 수 없다. Mermaid 버전/라벨 정화 방식이
// 달라져도 노드·연결·그룹이 조용히 사라지는 결과는 게시하지 않는다.
const topology = (diagram, restore = value => value) => JSON.stringify({
  nodes: [...diagram.db.getVertices().values()].map(node => [restore(node.id), node.type]).sort(),
  edges: diagram.db.getEdges().map(edge => [restore(edge.start), restore(edge.end), edge.type, edge.stroke]),
  groups: diagram.db.getSubGraphs().map(group => [restore(group.id), group.nodes?.map(restore)]),
});

// 실제 파서가 확인한 원문 구분자의 소유 범위 → 범위 안 TeX → 라벨 치환의
// 순서를 지킨다. 원문을 삭제해서 파싱에 성공시키는 전역 치환은 하지 않는다.
export async function prepareMermaidMath(source, parse, render) {
  if (!mermaidMathExpressions(source).length) return { source, math: false, errors: [] };
  let prefix = 'LLMMERMAIDMATH';
  while (source.includes(prefix)) prefix += 'X';
  // 내용은 지우지 않고 구분자만 표시한다. 두 구분자가 같은 실제 라벨에
  // 속한다는 사실을 먼저 증명해야 수식으로 합칠 수 있다. 가린 뒤 확인하면
  // A["$$미완성"] --> B["$$x^2$$"]의 B와 연결까지 사라질 수 있다.
  const delimiters = [...executableMermaidSource(source).matchAll(/\$\$/g)];
  const marker = index => `${prefix}BOUNDARY${index}END`;
  const markerPattern = new RegExp(`${prefix}BOUNDARY\\d+END`, 'g');
  let at = 0, marked = '';
  for (const [index, match] of delimiters.entries()) {
    marked += source.slice(at, match.index) + marker(index); at = match.index + 2;
  }
  marked += source.slice(at);
  let spans, originalTopology, original;
  try { original = await parse(marked); } catch { /* TeX 확장 구문은 아래의 공통 범위 분석으로 판정한다. */ }
  if (original) {
    const { labels, ids } = parts(original);
    originalTopology = topology(original, value => value?.replace(markerPattern, () => '$$'));
    const idMarkers = new Set(ids.flatMap(value => value?.match(markerPattern) ?? []));
    const owners = new Map();
    labels.forEach((label, owner) => {
      for (const token of label?.match(markerPattern) ?? []) if (!idMarkers.has(token)) owners.set(token, owner);
    });
    const locations = new Map(delimiters.map((match, index) => [match.index, owners.get(marker(index))]));
    spans = mermaidMathExpressions(source, (start, close) => locations.get(start) !== undefined &&
      locations.get(start) === locations.get(close));
  } else {
    // TeX를 포함하는 확장 문법도 무경계 전체 치환으로 돌아가지 않는다.
    spans = scopedMermaidMathExpressions(source);
  }
  if (!spans.length) return { source, math: false, errors: [] };
  const tokens = spans.map((_, index) => `${prefix}${index}END`);
  let index = 0;
  const diagram = await parse(replaceMermaidMath(source, () => tokens[index++], spans));
  if (originalTopology !== undefined && topology(diagram) !== originalTopology)
    throw new Error('수식 치환이 Mermaid 그래프 구조를 변경했습니다');
  const { labels, ids } = parts(diagram);
  const pattern = new RegExp(`${prefix}\\d+END`, 'g');
  const containedTokens = values => new Set(values.flatMap(value => typeof value === 'string' ? value.match(pattern) ?? [] : []));
  const labelTokens = containedTokens(labels), idTokens = containedTokens(ids);
  const errors = [];
  index = 0;
  return {
    source: replaceMermaidMath(source, value => {
      const token = tokens[index++];
      // 생략된 라벨은 노드 ID 자체다. ID를 HTML로 바꾸면 연결·class·click이
      // 끊어진다. 그 경우 원문 라벨의 조판은 Mermaid에 맡긴다.
      if (!labelTokens.has(token) || idTokens.has(token)) return `$$${value}$$`;
      try { return render(value); }
      catch {
        // 수식 하나의 오류로 정상 노드·연결·다른 수식까지 지우지 않는다.
        // 라벨에는 안전한 안내를 넣고, 정확한 TeX 원문은 그림 밖 details로 돌려준다.
        errors.push(`$$${value}$$`);
        return `수식 오류 ${errors.length}`;
      }
    }, spans),
    math: labelTokens.size > 0,
    errors,
  };
}

// KaTeX가 만든 HTML의 값은 유지하고 바깥 Mermaid/YAML 문법이 될 문자만
// 문자 참조로 표현한다. 큰/작은따옴표 라벨뿐 아니라 shape의 평문 값에서도
// TeX annotation의 ^·중괄호·백슬래시가 문법을 끊지 않는다. 사용자 HTML용이 아니다.
// Mermaid는 #번호;를 파싱 동안 보호하고 HTML의 &#번호;로 복원한다.
// HTML 표기를 직접 넣으면 앞의 &가 중복되어 표시 문자열·속성이 달라진다.
const reference = char => `#${char.codePointAt(0)};`;
// 구두점은 모두 문자 참조로 써서 바깥 문법의 구분자나 다음 수식 후보가 되지
// 않게 한다. 기존 HTML 문자 참조도 같은 값의 Mermaid 표기로 한 번만 옮긴다.
const literalText = value => value.replace(/&(?:#x[\da-f]+|#\d+|[a-z][\da-z]*);|[\x00-\x1f\x21-\x2f\x3a-\x40\x5b-\x60\x7b-\x7f]/gi,
  value => value.length === 1 ? reference(value) : [...decodeString(value)].map(reference).join(''));
export const literalMermaidMathML = html => html
  // Mermaid 자체 KaTeX 경로도 annotation을 제거한다. 이 대체 원문은 그림의
  // 라벨이 아니며 Markdown 재해석 중 이스케이프가 풀리면 새 수식이 될 수 있다.
  .replace(/<annotation\b[^>]*>[\s\S]*?<\/annotation>/g, '')
  // HTML 디코딩 뒤에도 $$를 다시 찾으므로 연속 달러만 인접한 MathML 텍스트로
  // 나눈다. 같은 mrow 안에서 서식·글자·폭은 유지하며 HTML span의 CSS도 받지 않는다.
  .replace(/<mtext([^>]*)>([^<]*)<\/mtext>/g, (whole, attrs, value) => value.includes('$$')
    ? `<mrow><mtext${attrs}>${value.replace(/\$(?=\$)/g, `$</mtext><mtext${attrs}>`)}</mtext></mrow>` : whole);
export const mermaidMathML = html => literalMermaidMathML(html)
  .replace(/="([^"]*)"/g, (_, value) => '=' + literalText(value).replace(/[\s=<>]/g, reference) + ' ')
  .replace(/>([^<]*)(?=<)/g, (_, value) => '>' + literalText(value));
