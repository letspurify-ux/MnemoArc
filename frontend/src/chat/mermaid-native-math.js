// Mermaid가 이미 분리한 라벨만 받는다. 그림 종류마다 TeX를 다시 정규식으로
// 해석하지 않고 본문과 동일한 경계·KaTeX 설정·오류 격리를 사용한다.
import { closingMathDelimiter } from '../../shared/math-spans.mjs';
import { renderMathML } from './math.js';
import { literalMermaidMathML } from './mermaid-math.js';
import { MATH_ADAPTER_KEY } from '../../shared/mermaid-math-spans.mjs';

function labelMathSpans(text) {
  const spans = [];
  for (let start = text.indexOf('$$'); start >= 0; start = text.indexOf('$$', start + 2)) {
    let slashes = 0;
    for (let i = start - 1; i >= 0 && text[i] === '\\'; i--) slashes++;
    if (slashes % 2) continue;
    const close = closingMathDelimiter(text, start + 2, '$$');
    if (close < 0) continue;
    if (close > start + 2) spans.push({ start, end: close + 2, value: text.slice(start + 2, close) });
    start = close;
  }
  return spans;
}

export const hasNativeMermaidMath = text => typeof text === 'string' && labelMathSpans(text).length > 0;
let currentErrors = null;
export async function withNativeMermaidMath(render, initialErrors = []) {
  const previous = currentErrors;
  const errors = [...initialErrors];
  currentErrors = errors;
  try { return { ...(await render()), mathErrors: errors }; }
  finally { currentErrors = previous; }
}

export function renderNativeMermaidLabel(text) {
  const spans = labelMathSpans(text);
  if (!spans.length) return text;
  let at = 0, html = '';
  const lines = value => value.replace(/<br\s*\/?\s*>|\r\n?|\n/gi, '</div><div>');
  for (const span of spans) {
    html += lines(text.slice(at, span.start));
    try { html += literalMermaidMathML(renderMathML(span.value)); }
    catch {
      if (!currentErrors) throw new Error('Mermaid 수식 렌더링에 오류 범위가 없습니다');
      const original = text.slice(span.start, span.end);
      let index = currentErrors.indexOf(original);
      if (index < 0) { index = currentErrors.length; currentErrors.push(original); }
      html += `수식 오류 ${index + 1}`;
    }
    at = span.end;
  }
  // 기존 Mermaid sanitizer는 이 함수의 반환 뒤에 그대로 실행된다.
  return `<div>${html + lines(text.slice(at))}</div>`;
}

globalThis[Symbol.for(MATH_ADAPTER_KEY)] = {
  hasMath: hasNativeMermaidMath,
  renderLabel: renderNativeMermaidLabel,
};
