// 코드·주소를 구분한 뒤 수식 원문을 보존하여 Markdown을 파싱한다. KaTeX 렌더링은 한 경로로 모은다.
import remarkGfm from './remark-gfm.js';
import remarkMath from 'remark-math';
import katex from 'katex';
import 'katex/contrib/mhchem';
import { fromHtmlIsomorphic } from 'hast-util-from-html-isomorphic';
import { compatibleEnvironments, requiresDisplay } from './tex-environments.js';
import remarkPreserveMath, { normalizeMath, unwrapMath } from './remark-preserve-math.js';
import remarkSerializedMarkdown, { decodeMathEscapes } from './serialized-markdown.js';
import remarkRichTable, { remarkProtectTableVisualizations } from './remark-rich-table.js';

const KATEX_OPTIONS = { strict: 'ignore', trust: false, maxSize: 10, maxExpand: 1000, throwOnError: true };
export const renderMathML = tex => katex.renderToString(compatibleEnvironments(normalizeMath(decodeMathEscapes(tex).trim())),
  { ...KATEX_OPTIONS, displayMode: true, output: 'mathml' });
const element = (tagName, properties, children) => ({ type: 'element', tagName, properties, children });
const text = value => ({ type: 'text', value });
// KaTeX의 번호는 기본적으로 절대 위치라 좁은 컨테이너에서 수식과 겹친다.
// 생성된 HTML의 본문과 번호를 분리해 CSS가 두 영역의 실제 폭을 함께 계산하게 한다.
function numberLayout(node) {
  if (node.properties?.className?.includes('katex-html')) {
    const tag = node.children.find(child => child.properties?.className?.includes('tag'));
    if (tag) {
      node.properties.className.push('math-numbered');
      node.children = [element('span', { className: ['math-equation'] }, node.children.filter(child => child !== tag)), tag];
    }
  }
  for (const child of node.children ?? []) numberLayout(child);
}
const fallback = source => element('details', { className: ['math-error', 'katex-error'] }, [
  element('summary', {}, [text('수식을 표시하지 못했습니다 · 원문 보기')]),
  element('pre', {}, [element('code', {}, [text(source)])]),
]);

function rehypeMath() {
  return tree => {
    const walk = node => {
      const classes = node.properties?.className ?? [];
      const fence = node.tagName === 'code' && classes.some(c => /^language-(math|latex|tex)$/i.test(c));
      if (classes.includes('math-inline') || classes.includes('math-display') || fence) {
        const original = (node.children ?? []).map(child => child.value ?? '').join('');
        if (node.properties?.['data-math-incomplete']) return [fallback(node.properties['data-math-source'] ?? original)];
        const tex = compatibleEnvironments(normalizeMath(decodeMathEscapes(fence ? unwrapMath(original) : original).trim()));
        const displayMode = fence || classes.includes('math-display') || requiresDisplay(tex);
        try {
          // KaTeX만 만든 HTML을 HAST로 바꾼다. 모델의 HTML을 파싱하거나 trust를 켜지 않는다.
          const html = katex.renderToString(tex, { ...KATEX_OPTIONS, displayMode });
          const rendered = fromHtmlIsomorphic(html, { fragment: true });
          numberLayout(rendered);
          return rendered.children;
        } catch {
          // 모르는 명령도 붉은 원문 조각으로 조판하지 않는다. 내용은 펼쳐 확인할 수 있게 남긴다.
          return [fallback(node.properties?.['data-math-source'] ?? original)];
        }
      }
      if (!node.children) return [node];
      node.children = node.children.flatMap(walk);
      // 별행 전용 명령을 인라인 구분자에 쓴 경우와 오류 원문(details)도 유효한 블록으로 분리한다.
      const phrasing = ['p', 'em', 'strong', 'a', 'del', 'span', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6'];
      const isBlock = child => child.tagName === 'details' ||
        (node.tagName === 'p' && child.properties?.className?.includes('katex-display'));
      if (phrasing.includes(node.tagName) && node.children.some(isBlock)) {
        const out = []; let children = [];
        const flush = () => {
          if (children.some(child => child.type !== 'text' || child.value.trim())) out.push({ ...node, children });
          children = [];
        };
        for (const child of node.children) {
          if (isBlock(child)) {
            flush(); out.push(child);
          } else children.push(child);
        }
        flush(); return out;
      }
      if (node.tagName === 'pre' && node.children.some(child => child.tagName !== 'code')) return node.children;
      return [node];
    };
    walk(tree);
  };
}

export const REHYPE_PLUGINS = [rehypeMath];
// 개행 복구가 새 표를 만들 수 있으므로, 복구된 AST에서 시각화의 내부 |를 보호한다.
export const REMARK_PLUGINS = [remarkGfm, [remarkMath, { singleDollarTextMath: false }], remarkSerializedMarkdown,
  remarkProtectTableVisualizations, remarkPreserveMath, remarkRichTable];
