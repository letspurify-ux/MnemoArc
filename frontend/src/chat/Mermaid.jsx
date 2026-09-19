// ```mermaid 블록 하나를 그림으로. 지식 답변의 절차·흐름을 모델이 흐름도로 쓸 때 쓰인다.
// App.jsx가 React.lazy로 부른다 — mermaid는 번들이 수 MB라 첫 흐름도가 나올 때까지 내려받지 않는다.
import { useEffect, useLayoutEffect, useRef, useState, useId } from 'react';
import mermaid from 'mermaid';
// 그림 안 링크의 주소 규칙과 그림 노드의 판정은 답변의 링크·그림과 같은 자리에 있다 (markdown.js).
import { rawLinkTarget, mermaidLoadsImage, mermaidFetchesViaStyle, mermaidMathLabels } from './markdown.js';
import { renderMathML } from './math.js';
import { prepareMermaidMath, mermaidMathML, mermaidMathExpressions } from './mermaid-math.js';
import { withNativeMermaidMath } from './mermaid-native-math.js';

const MERMAID_CONFIG = {
  startOnLoad: false,
  // 라벨의 HTML을 그대로 넣지 않는다(모델이 만든 글자가 곧 화면에 들어가는 자리다).
  securityLevel: 'strict',
  // 그런데 strict의 정화(DOMPurify)는 스크립트와 이벤트 속성만 걷어낸다 — <img src=…>는 살아남고,
  // 기본값인 HTML 라벨(<foreignObject> 안에 넣는다)에서는 그것이 진짜 <img>가 되어 브라우저가
  // 모델이 쓴 주소를 사용자가 누르기도 전에 부른다(실측: 라벨 하나가 외부 요청 한 건을 냈다).
  // 답변의 글자에는 조회 결과가 섞여 있으므로 그 통로 자체를 닫는다 — 라벨은 SVG 글자로 그린다.
  // (<br/>로 줄을 바꾸는 것은 글자 라벨에서도 그대로 된다)
  // 루트의 이 값이 모든 그림 종류를 덮는다 — mermaid 11에서 flowchart.htmlLabels 같은 종류별 설정은
  // 폐기됐고 루트가 우선한다. 종류마다 따로 적으면 새 그림이 늘 때마다 빠뜨릴 자리만 늘어난다.
  htmlLabels: false,
  // 위 htmlLabels를 그림의 글자가 되돌리지 못하게 한다. mermaid는 본문의 지시문(`%%{init: {…}}%%`)과
  // 머리말(`---` config: `---`)로 설정을 덮어쓰게 해 주는데, 그 덮어쓰기에서 지키는 것은 secure에 적힌
  // 키뿐이다 — 기본 목록에 securityLevel은 있어도 htmlLabels는 없다. 그래서 모델이(또는 모델이 베낀
  // 조회 결과가) 그 한 줄을 쓰면 라벨이 다시 HTML이 된다(실측: `%%{init: {"htmlLabels": true}}%%` 한 줄로
  // <foreignObject> 안에 <img>가 서서 주소가 불려 나갔고, <a href>는 같은 탭에서 열리는 링크로 남았다 —
  // 답변의 링크를 새 탭으로만 여는 이 화면의 규칙(App.jsx NewTabLink) 밖이다). 머리말의 config도
  // 같은 문을 지나므로 함께 막힌다. 지키는 키는 어느 깊이에 있든 걸러진다(flowchart.htmlLabels도).
  // 기본 목록을 이어받아 늘린다 — 통째로 다시 적으면 mermaid가 기본 목록에 키를 더한 날 그것을 조용히 잃는다.
  secure: [...(mermaid.mermaidAPI?.defaultConfig?.secure ?? []), 'htmlLabels', 'dompurifyConfig'],
  // 라벨 문자의 사전 차단 대신 HTML을 DOM에 넣는 라이브러리 경로에서 제거한다.
  // 일반 라벨과 수식 라벨 모두 같은 제한을 받으며 본문 설정이 덮어쓸 수 없다.
  dompurifyConfig: {
    FORBID_TAGS: ['img', 'image', 'style', 'iframe', 'object', 'embed', 'video', 'audio', 'source', 'link'],
    FORBID_ATTR: ['src', 'srcset', 'style'],
  },
  // 문법이 틀린 그림에 mermaid가 '폭탄' 오류 SVG를 문서에 직접 끼워 넣는 것을 막는다 —
  // 그 경우는 아래에서 원문 코드로 되돌린다.
  suppressErrorRendering: true,
  theme: 'neutral',
  fontFamily: 'inherit',
};
mermaid.initialize(MERMAID_CONFIG);

// 설정은 전역이므로 설정 변경과 실제 렌더를 같은 직렬 작업으로 묶는다. 수식 라벨은
// 본문과 같은 제한의 KaTeX가 만든 MathML을 주입하고 라벨 HTML은 잠긴 설정으로 정화한다.
let renderQueue = Promise.resolve();
function renderDiagram(id, text) {
  const task = renderQueue.then(async () => {
    // 직전 그림의 본문 설정과 무관한 기본 설정에서 종류를 판정한다.
    mermaid.initialize(MERMAID_CONFIG);
    const candidate = mermaidMathLabels(text, mermaid.detectType(text.replace(/\r\n?/g, '\n')));
    const { source, math, errors = [] } = candidate ? await prepareMermaidMath(text,
      source => mermaid.mermaidAPI.getDiagramFromText(source), tex => mermaidMathML(renderMathML(tex)))
      : { source: text, math: false };
    // 상태도 등의 공통 라벨 렌더러도 MathML을 배치할 HTML 경로가 필요하다.
    // 글자/수식 모두 같은 잠긴 sanitizer를 사용하므로 그림 종류에 의존하지 않는다.
    mermaid.initialize({ ...MERMAID_CONFIG, htmlLabels: math || mermaidMathExpressions(text).length > 0 });
    return withNativeMermaidMath(() => mermaid.render(id, source), errors);
  });
  renderQueue = task.catch(() => {});
  return task;
}

// 라벨 글자가 이보다 작아지면 그림은 남고 글자는 사라진다 — 그 아래로는 줄이지 않는다(아래 참고).
const MIN_LABEL_PX = 9;

export default function Mermaid({ text }) {
  const [rendered, setRendered] = useState(null);
  const svg = rendered?.svg ?? null;
  const boxRef = useRef(null);
  // mermaid.render는 문서에 이 id의 요소를 만들었다 지운다 — useId의 콜론은 CSS 선택자로 못 쓴다.
  const id = `mmd${useId().replace(/[^A-Za-z0-9]/g, '')}`;
  useEffect(() => {
    let alive = true;
    setRendered(null);
    // 그리는 도중에 주소를 불러오는 그림은 그리지 않는다 — 그림 노드(`A@{ img: "주소" }`)와, 설정 자리의
    // CSS가 바깥을 부르는 것(지시문의 themeCSS·fontFamily에 든 url(…))이다(markdown.js mermaidLoadsImage·
    // mermaidFetchesViaStyle). 둘 다 그린 뒤에는 늦다 — mermaid가 크기를 재려고 SVG를 문서에 넣는 순간
    // 요청이 나간다. 문법 오류와 같이 원문 코드가 남는다. 오류가 아니라 정책이므로 경고가 아니라 알림으로만 남긴다.
    if (mermaidLoadsImage(text) || mermaidFetchesViaStyle(text)) {
      console.info('[mermaid] 그리는 도중에 주소를 불러오는 그림(img: 노드, url( 지시문)은 그리지 않습니다');
      return;
    }
    // 그리지 못하면(문법 오류 등) svg를 null로 둔 채 끝낸다 — 아래에서 원문을 코드로 보여준다.
    renderDiagram(id, text).then(r => { if (alive) setRendered(r); }, e => console.warn('[mermaid] render failed:', e?.message ?? e));
    return () => { alive = false; };
  }, [text, id]);
  // 그림 안의 링크(`click A "주소"`)는 답변의 링크와 같은 규칙으로 연다 — 새 탭, 페이지 안 앵커만
  // 제자리(App.jsx NewTabLink). mermaid는 strict에서도 링크를 남기되 target은 주지 않고(`_blank`라고
  // 적어도 버린다) `_self`는 그대로 두어, 누르면 같은 탭에서 열려 대화가 통째로 사라진다(실측). 주소도
  // 같은 문(markdown.js rawLinkTarget)을 지나게 한다 — 걷어내면 남지 않는 주소는 링크에서 뗀다.
  // SVG는 innerHTML로 넣었으므로 React가 이 속성들을 다시 건드리지 않는다. 그리기 전에 손봐야 하므로
  // useLayoutEffect다.
  useLayoutEffect(() => {
    const box = boxRef.current;
    if (!box || svg === null) return;
    for (const a of box.querySelectorAll('a')) {
      const href = a.getAttribute('href') ?? a.getAttribute('xlink:href');
      if (href === null) continue; // mermaid가 이미 걷어낸 것(javascript: 등) — 누를 것이 없는 <a>다
      const { url, attrs } = rawLinkTarget(href);
      for (const k of ['href', 'xlink:href', 'target', 'rel']) a.removeAttribute(k);
      if (url === '') continue;
      a.setAttribute('href', url);
      for (const [k, v] of Object.entries(attrs)) a.setAttribute(k, v);
    }
  }, [svg]);
  // mermaid의 SVG는 width="100%"라 상자에 맞춰 통째로 줄어든다. 좁은 상자에서는 그 배율이 끝없이
  // 내려가 그림은 다 보이는데 글자를 못 읽는 상태가 된다 — 실측: 폭 238px 상자에서 흐름도 높이 41px,
  // 라벨 글자 3px. 그래서 글자가 MIN_LABEL_PX 아래로 내려가면 거기서 멈추고 남는 폭은 .mermaid의
  // 가로 스크롤에 넘긴다(index.html). 읽히는 그림을 밀어 보는 편이, 다 보이지만 못 읽는 것보다 낫다.
  // 넓은 화면에서는 글자가 이미 크므로 아무것도 하지 않는다(데스크톱 폭에서 라벨은 9px을 넘는다).
  useLayoutEffect(() => {
    const box = boxRef.current;
    if (!box || svg === null) return;
    let lastW = -1;
    const fit = () => {
      const w = box.clientWidth;
      // 우리가 만든 높이 변화로 다시 불려온 것이면 할 일이 없다 — 폭이 그대로면 결과도 그대로다.
      if (w === lastW) return;
      lastW = w;
      const el = box.querySelector('svg');
      if (!el) return;
      el.style.minWidth = '';
      // 제 크기(이 이상 넓혀도 더 커지지 않는다). mermaid가 인라인으로 적어 두지만(useMaxWidth),
      // 그 설정을 끄면 없다 — 그때는 viewBox의 폭이 같은 뜻이다. 둘 다 없을 때만 상한이 없다.
      const natural = parseFloat(el.style.maxWidth) || el.viewBox?.baseVal?.width || Infinity;
      // 가장 작은 글자의 높이. 라벨 목록은 한 번만 모으고(넓힐 때마다 다시 찾지 않는다), 재는 것은
      // 한 줄이다 — 여러 줄로 감긴 라벨은 상자 높이가 줄 수만큼이라 그것을 글자 크기로 삼으면 배율이
      // 줄 수만큼 모자란다. 0으로 나오는 것(빈 줄, 감춰진 라벨)은 세지 않는다: 하나라도 0을 그대로
      // 받으면 넓히기를 아예 포기하게 되어 그림 전체가 못 읽는 크기로 남는다.
      // 첫 라벨이 아니라 가장 작은 것을 쓰는 이유는, 크기가 섞인 그림에서 큰 라벨 하나가 기준을
      // 넘겼다고 멈추면 그보다 작은 글자가 못 읽는 채 남기 때문이다.
      // 한 라벨 안에서도 한글·영문 및 줄마다 글리프 높이가 다를 수 있다.
      // 첫 tspan만 재면 나머지 줄의 작은 글자가 검사에서 빠진다.
      // 연결선·하위 그래프의 라벨과 MathML도 같은 읽기 크기를 보장한다.
      // 노드만 재면 긴 연결선 수식이 모바일에서 더 작아져도 놓치게 된다.
      const labels = [...el.querySelectorAll('.nodeLabel, .edgeLabel, .cluster-label, text, math')].flatMap(n => {
        const spans = [...n.querySelectorAll('tspan')];
        return spans.length ? spans : [n];
      });
      const lineHeight = () => {
        const hs = labels.map(n => n.getBoundingClientRect().height).filter(v => v > 0);
        return hs.length ? Math.min(...hs) : 0;
      };
      // 넓힌 뒤 다시 재서 모자라면 한 번 더 넓힌다. '폭을 두 배로 하면 글자도 두 배'가 아니기 때문이다 —
      // 브라우저는 아주 작은 글자를 어느 선 아래로는 줄이지 않아, 줄어든 그림에서 잰 글자는 실제보다
      // 크게 나온다(실측: 폭 268px에서 4px로 보이던 글자가 2.25배 넓힌 603px에서 9px이 아니라 7px).
      // 그 어긋남을 한 번의 계산으로 맞출 수는 없으므로 실제 결과를 보고 좁힌다. 늘 몇 번 안에 끝난다:
      // 글자가 커질수록 비례에 가까워지고, 제 크기(natural)에 닿으면 더 넓힐 것이 없다.
      for (let i = 0; i < 4; i++) {
        const h = lineHeight();
        if (!h || h >= MIN_LABEL_PX) break;
        const now = el.getBoundingClientRect().width;
        const want = Math.min(natural, Math.ceil(now * (MIN_LABEL_PX / h)));
        if (want <= now) break; // 제 크기까지 넓혀도 모자란다 — 여기까지가 최선이다
        el.style.minWidth = `${want}px`;
      }
    };
    fit();
    if (typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(fit);
    ro.observe(box);
    return () => ro.disconnect();
  }, [svg]);
  // 그리는 동안과 실패했을 때는 원문 코드. 실패는 모델의 문법 실수가 대부분이라 원문이 곧 설명이다.
  if (svg === null) return <pre><code>{text}</code></pre>;
  return <>
    <div className="mermaid" ref={boxRef} dangerouslySetInnerHTML={{ __html: svg }} />
    {rendered.mathErrors.map((source, index) => <details className="math-error katex-error" key={index}>
      <summary>그림의 수식 오류 {index + 1} · 원문 보기</summary>
      <pre><code>{source}</code></pre>
    </details>)}
  </>;
}
