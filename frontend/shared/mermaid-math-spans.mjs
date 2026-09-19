// Mermaid 원문의 수식 경계. 직렬화 복원과 화면 렌더가 같은 범위를 사용한다.
import { closingMathDelimiter } from './math-spans.mjs';
import { verbEnd, texGroupEnds } from './tex-environments.mjs';
export const MATH_ADAPTER_KEY = 'llm-agent.mermaid.math.v1';
const mask = value => value.replace(/[^\r\n]/g, ' ');
export function executableMermaidSource(source) {
  let text = source.replace(/^([^\S\n\r]*)-{3}\s*[\n\r](.*?)[\n\r]\1-{3}\s*[\n\r]+/s, mask);
  let at = 0, masked = '';
  for (let start = text.indexOf('%%{'); start >= 0; start = text.indexOf('%%{', at)) {
    const close = text.indexOf('}%%', start + 3);
    const end = close < 0 ? text.length : close + 3;
    masked += text.slice(at, start) + mask(text.slice(start, end));
    at = end;
  }
  return (masked + text.slice(at)).replace(/^\s*%%(?!{)[^\r\n]+(?:\r\n?|\n)?/gm, mask);
}

export function mermaidMathExpressions(source, owns) {
  if (!owns) return scopedMermaidMathExpressions(source);
  const text = executableMermaidSource(source);
  const spans = [];
  for (let start = text.indexOf('$$'); start >= 0; start = text.indexOf('$$', start + 2)) {
    let slashes = 0;
    for (let i = start - 1; i >= 0 && text[i] === '\\'; i--) slashes++;
    if (slashes % 2) continue;
    // 본문과 같은 TeX 경계를 사용한다. 줄바꿈은 수식의 공백이며,
    // 주석·\\verb 안의 $$와 이스케이프된 달러는 닫는 구분자가 아니다.
    const close = closingMathDelimiter(text, start + 2, '$$');
    if (close < 0) continue;
    if (!owns(start, close)) continue;
    if (close > start + 2) spans.push({ start, end: close + 2, value: source.slice(start + 2, close) });
    start = close;
  }
  return spans;
}

// 원문 Mermaid가 허용하지 않는 TeX 구두점(예: 따옴표 없는 라벨의 분수)을
// 확장할 때도 바깥 문자열/괄호/문장 경계는 보존한다. 완성된 TeX 그룹·verb만
// 원자적으로 읽으며, 미완성 TeX 그룹은 Mermaid의 닫는 기호를 소유하지 않는다.
export function scopedMermaidMathExpressions(source, { lineBreaks } = {}) {
  const view = lineBreaks ? source.replace(lineBreaks, value => '\n' + ' '.repeat(value.length - 1)) : source;
  const text = executableMermaidSource(view), spans = [], stack = [];
  const pairs = { '[': ']', '(': ')', '{': '}' };
  let quote = '';
  const inside = (start, end) => {
    // 바깥 문자열의 닫힘은 TeX의 미완성 인자가 가져갈 수 없다. 원문
    // `label: '$$\\frac{1'}`의 }를 TeX 그룹의 끝으로 읽으면 다음 노드까지
    // 수식이 된다. 같은 문자열 안의 일반 따옴표(\\text{a"b})와는 구분한다.
    if (quote) for (let i = start; i < end; i++) {
      if (text[i] === '\\') { i++; continue; }
      if (text[i] !== quote) continue;
      if (quote === "'" && text[i + 1] === "'") { i++; continue; }
      const next = /^[ \t]*/.exec(text.slice(i + 1, end))[0].length + i + 1;
      const closer = stack.at(-1);
      if ((closer && text[next] === closer) || (!closer && /[)\]}|\r\n]/.test(text[next] ?? '')) ||
        (closer === '}' && text[next] === ',')) return false;
    }
    const groups = [];
    const braceEnds = texGroupEnds(text, start, end);
    for (let i = start; i < end; i++) {
      const c = text[i];
      if (c === '\\') { i = Math.max(i + 1, verbEnd(text, i)); continue; }
      if (c === '%') { while (i < end && !/[\r\n]/.test(text[i])) i++; continue; }
      if (braceEnds.has(i)) { i = braceEnds.get(i); continue; }
      if (quote) { if (c === quote) return false; }
      else {
        if (c === '[' || c === '(') { groups.push(pairs[c]); continue; }
        if (groups.at(-1) === c) { groups.pop(); continue; }
        if (c === '"' || /[\]}]/.test(c) || (c === ')' && stack.at(-1) === ')')) return false;
        if (!groups.length && /^(?:[ox<]?(?:--+>|==+>|-\.+->)|--{2,}|~~{2,})/.test(text.slice(i, end))) return false;
        if (!stack.length && /[\r\n;]/.test(c)) return false;
      }
    }
    return true;
  };
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === '\\') { i++; continue; }
    if (text.startsWith('$$', i)) {
      const close = closingMathDelimiter(text, i + 2, '$$');
      if (close > i + 2 && inside(i + 2, close)) {
        spans.push({ start: i, end: close + 2, value: source.slice(i + 2, close) });
        i = close + 1; continue;
      }
      i++; continue;
    }
    if (quote) { if (c === quote) quote = ''; continue; }
    if (c === '"' || (c === "'" && stack.at(-1) === '}')) { quote = c; continue; }
    if (pairs[c]) stack.push(pairs[c]);
    else if (stack.at(-1) === c) stack.pop();
  }
  return spans;
}
