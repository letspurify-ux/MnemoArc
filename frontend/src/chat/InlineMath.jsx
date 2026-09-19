import { useLayoutEffect, useRef, useState } from 'react';
import { flushSync } from 'react-dom';

// Markdown의 인라인 서식은 폭을 제공하지 않는다. 실제 줄을 배치하는
// 문단·목록 항목·제목·셀까지 올라가 사용 가능한 내용 폭을 읽는다.
function containingBlock(element) {
  let parent = element.parentElement;
  while (parent && ['inline', 'contents'].includes(getComputedStyle(parent).display)) parent = parent.parentElement;
  return parent;
}

export default function InlineMath({ inLink, className, children, style, ...props }) {
  const ref = useRef(null);
  const [layout, setLayout] = useState({ scroll: false, scale: 1 });
  useLayoutEffect(() => {
    const element = ref.current;
    if (!element || element.closest('.katex-display')) return;
    const block = containingBlock(element);
    if (!block) return;
    let alive = true;
    const measure = () => {
      if (!alive) return;
      const css = getComputedStyle(block);
      const available = block.clientWidth - parseFloat(css.paddingLeft) - parseFloat(css.paddingRight);
      // KaTeX의 base는 줄을 나눌 수 없는 단위다. 이미 여러 base 사이에서
      // 자연스럽게 줄바꿈한 수식을 불필요하게 한 줄로 늘리지 않는다.
      const bases = [...element.querySelectorAll(':scope > .katex-html > .base')];
      const width = Math.max(0, ...bases.map(base => base.offsetWidth));
      const scroll = available > 0 && width > available + 1;
      // 스크롤 상자에서는 여러 base를 한 줄에 놓는다. 인쇄 축척에는
      // 가장 넓은 base만이 아니라 그 줄 전체의 폭이 필요하다.
      const lineWidth = bases.reduce((sum, base) => sum + base.offsetWidth, 0);
      const scale = scroll ? Math.min(1, available / (lineWidth + bases.length)) : 1;
      setLayout(previous => previous.scroll === scroll && Math.abs(previous.scale - scale) < 0.001
        ? previous : { scroll, scale });
    };
    measure();
    const observer = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(measure);
    observer?.observe(block);
    document.fonts?.ready.then(measure);
    // 인쇄는 ResizeObserver의 다음 프레임을 기다리지 않는다. 인쇄용 CSS가
    // 적용된 폭에서 동기적으로 배치하고, 종이에서는 스크롤 대신 크기를 맞춘다.
    const print = () => flushSync(measure);
    window.addEventListener('beforeprint', print);
    window.addEventListener('afterprint', measure);
    window.addEventListener('resize', measure);
    return () => {
      alive = false;
      observer?.disconnect();
      window.removeEventListener('beforeprint', print);
      window.removeEventListener('afterprint', measure);
      window.removeEventListener('resize', measure);
    };
  }, [children]);
  return <span {...props} ref={ref} className={className + (layout.scroll ? ' math-scroll' : '')}
    style={layout.scroll ? { ...style, '--math-print-scale': layout.scale } : style}
    tabIndex={layout.scroll && !inLink ? 0 : undefined}
    role={layout.scroll && !inLink ? 'group' : undefined}
    aria-label={layout.scroll && !inLink ? '가로로 스크롤할 수식' : undefined}>{children}</span>;
}
