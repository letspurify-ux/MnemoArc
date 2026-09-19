import { useState, useRef, useEffect, useLayoutEffect, useMemo, useId, memo, lazy, Suspense, Component, createContext, useContext } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
// 수식 표기의 계약(무엇이 수식인가 + 그것을 어떻게 그리는가)은 전부 math.js에 있다.
import { REMARK_PLUGINS, REHYPE_PLUGINS } from './math.js';
// 차트 블록의 계약(무엇을 차트로 받는가 + 이력으로 되돌릴 때의 모양)은 chart.js에 있다.
import { parseChartBlock, splitBlock, chartTableMarkdownFrom, chartBlocksToTables, sliceSafe, clip, MAX_CHARTS_PER_MESSAGE, MAX_TITLE_LEN } from './chart.js';
// trace 패널의 계약(열·셀 표기·CSV)은 trace.js에 있다.
import { columnsOf, cellValue, cellText, toCsv, csvFileName, stepLabel, normalizeTrace, isSearchStep, targetsLabel, traceSummary,
  applyProgress, progressText } from './trace.js';
// 응답 스트림 읽기는 stream.js에, 미리보기 코드블록 표시는 preview.js에 있다.
import { PreviewPre } from './preview.js';
// 답변 속 주소를 어떻게 다룰지의 판정은 markdown.js에 있다 (순수 함수라 회귀 테스트가 붙는다).
import { linkTarget, imageTarget, mdProps, scopeMarkdownIds } from './markdown.js';
import InlineMath from './InlineMath.jsx';

const NO_REHYPE = [];
function useMarkdownPlugins(base = NO_REHYPE) {
  const id = useId();
  return useMemo(() => [...base, [scopeMarkdownIds, `md-${id}-`]], [base, id]);
}

// 그리는 쪽(recharts·mermaid)은 첫 차트·흐름도가 나올 때 내려받는다 — 둘을 합치면 앱 본체의 몇 배라,
// 글과 표뿐인 대부분의 대화가 그 값을 치를 이유가 없다. 내려받는 동안과 실패했을 때는 표·코드가 보인다.
const Chart = lazy(() => import('./Chart.jsx'));
const Mermaid = lazy(() => import('./Mermaid.jsx'));

// 서버(agent.js normalizeChat)가 실제로 쓰는 상한과 같은 값. 서버 쪽 제한은 본문을 파싱한 뒤에
// 적용되므로 요청 크기를 실제로 묶어두는 것은 이쪽뿐이다 — 넘기면 express의 본문 크기 제한에 걸려
// 이후 모든 요청이 같은 이유로 실패한다(이력은 줄지 않으므로 대화가 복구되지 않는다).
const HISTORY_TURNS = 6;
const HISTORY_LEN = 1500;

// 단순 slice는 경계의 서로게이트 쌍(이모지 등)을 반으로 쪼개 짝 잃은 코드유닛을 남기고,
// 그 값은 서버를 거쳐 LLM 프롬프트로 가는 인코딩 단계에서 U+FFFD로 조용히 훼손된다.
// 경계에 걸린 상위 서로게이트 하나를 떼어 항상 온전한 문자열만 보낸다 (서버 constants.clipText와 같은 방식).
const clipTurn = s => sliceSafe(String(s ?? ''), HISTORY_LEN);

// 요청 상한. 서버 최악 = 루프 진입 예산 180초(agent.js MAX_LOOP_MS) + 마지막 LLM 호출 120초
// + 강제 답변 120초 ≈ 420초이므로 그보다 뒤에 둔다. 짧게 잡으면 서버가 답을 만들어 보내는 중에
// 클라이언트가 먼저 끊어 "서버와 통신하지 못했습니다"로 뭉개진다.
// 이게 없으면 반대로 서버가 응답하지 않을 때 타이핑 표시가 영원히 돈다.
const REQUEST_TIMEOUT_MS = 450_000;
// 답변 조각을 화면에 올리는 간격(ms). 눈에는 연속으로 보이면서 markdown 파싱은 초당 여덟 번을 넘지 않는다.
const PREVIEW_FLUSH_MS = 120;

const EXAMPLES = [
  'SPACE 시스템이 뭐야?',
  'VM Agent Dashboard 현황 알려줘',
  '너는 어떤 일을 할 수 있어?',
];

// 차트 블록 안의 표만 렌더할 때의 파이프라인. 값은 조회 결과 그대로라 수식 처리는 필요 없다.
const TABLE_PLUGINS = [remarkGfm];
// '표로 보기'의 표도 본문과 같은 링크 규칙이다(NewTabLink, 아래) — 셀의 URL은 GFM이 자동 링크로 만들고,
// 그것이 같은 탭에서 열리면 대화가 사라진다.
// 그림도 본문과 같은 규칙이다 (AltImage, 아래) — 셀 안의 ![](주소)도 저절로 불려 나가서는 안 된다.
// urlTransform과 img는 한 벌로 받는다(markdown.js mdProps) — 그림 src의 기본 검사를 걷어내는 쪽과
// 그 값을 <img>로 만들지 않는 쪽이라, 새 <ReactMarkdown> 하나가 둘 중 하나를 잊으면 조용히 뚫린다.
const TABLE_MD = mdProps({ a: NewTabLink, img: AltImage });

// 차트 블록의 표를 평범한 표로. 차트를 그리지 못할 때·내려받는 동안·'표로 보기'가 모두 이것이다.
// 차트를 그리지 않을 때(파싱 실패·예산 초과)는 제목까지 표 위에 남긴다 — 차트 밑 '표로 보기'에서는 제목이 이미 보인다.
// 제목은 markdown에 섞지 않고 글자 그대로 놓는다 — 모델이 쓴 제목의 `*`·`_`·`[`가 강조·링크로 읽히면 안 된다.
// 표가 없는 블록: `data:` 참조가 남아 있으면 서버가 채우지 못한 것이다(서버가 못 알아본 펜스 등) —
// 설정 줄을 코드로 보여줘 봐야 사용자는 읽을 수 없으니 서버가 쓰는 것과 같은 안내 문장으로 바꾼다.
// 참조도 표도 없는 블록은 무엇인지 모르므로(모델이 펜스를 다른 용도로 썼을 수 있다) 원문 그대로 둔다.
// block은 부르는 쪽(ChartBlock)이 한 번 갈라 둔 것이다 — 같은 블록을 표로 두 번 그리므로
// 여기서 다시 가르면 한 답변에 같은 글자를 몇 번씩 훑게 된다.
function ChartTable({ text, block, withTitle = false }) {
  const rehypePlugins = useMarkdownPlugins();
  const md = chartTableMarkdownFrom(block);
  // 제목은 그리는 쪽과 같은 길이로 묶는다(chart.js MAX_TITLE_LEN). 여기서만 자르지 않으면, 같은
  // 블록이 그려질 때는 80자짜리 제목을 달고 그리지 못할 때는 모델이 쓴 글이 통째로 제목이 된다 —
  // 실측: 600자 제목이 그대로 말풍선에 폈다(그리는 쪽 figcaption은 80자였다). 이 값도 조회 결과가
  // 섞인 모델의 글자라 길이의 상한이 없고, 상한이 어느 갈래에만 있으면 그것은 상한이 아니다.
  const title = clip(String(block.config.title ?? '').trim(), MAX_TITLE_LEN);
  if (!md) {
    if (block.config.data === undefined) return Object.keys(block.config).length
      ? <p><em>{title ? `'${title}' ` : ''}차트를 그리지 못했습니다: 표시할 데이터가 없습니다</em></p>
      : <pre><code>{text}</code></pre>;
    return <p><em>{title ? `'${title}' ` : ''}차트를 그리지 못했습니다: 조회 결과를 채우지 못했습니다</em></p>;
  }
  return (
    <>
      {withTitle && title && <p><strong>{title}</strong></p>}
      <ReactMarkdown remarkPlugins={TABLE_PLUGINS} rehypePlugins={rehypePlugins} {...TABLE_MD}>{md}</ReactMarkdown>
    </>
  );
}

// 렌더 도중에 던지면 React는 그것을 잡아 줄 경계를 찾고, 없으면 앱 전체를 내린다 — 이 화면의
// 대화는 메모리에만 있으므로 그 순간 대화가 통째로 사라진다. 던지는 값은 모두 우리가 만든 것이
// 아니다: 모델이 쓴 답변 글자, 서버가 준 trace, 그것을 읽는 라이브러리.
// try/catch로는 막을 수 없다 — 렌더 중의 던짐은 React가 가로채기 때문이다. 경계가 그 자리의
// try/catch이고, 걸린 자리만 폴백으로 바꾼 뒤 나머지는 그대로 둔다.
// 폴백 자체는 절대 던지지 않아야 한다: 같은 경계는 자기 폴백의 오류를 다시 잡지 못해, 그러면
// 결국 앱이 내려간다. 그래서 폴백에는 markdown도 차트도 두지 않는다.
// what은 콘솔에 남길 이름이다 — 어느 자리가 걸렸는지 모르면 고칠 수도 없다.
class Boundary extends Component {
  state = { failed: false };
  static getDerivedStateFromError() { return { failed: true }; }
  componentDidCatch(e) { console.warn(`[${this.props.what}] render failed:`, e?.message ?? e); }
  render() { return this.state.failed ? this.props.fallback : this.props.children; }
}

// 말풍선 하나가 그려지다 던졌을 때 그 자리에 놓는 것. 답변의 글자만으로도 렌더는 던진다 —
// 실측: '>'가 3천 번 겹친 답변 하나가 markdown 파서의 스택을 넘겨(RangeError) 화면을 백지로 만들었다.
// 서버 상한(MAX_ANSWER_LEN 70,000자) 안에서 얼마든지 올 수 있는 글자다.
// 던진 자리를 빈칸으로 두지 않고 원문을 그대로 보인다 — 흐름도·차트가 실패했을 때와 같은 처방이다.
// 이 안에서는 무엇도 던지면 안 되므로 글자만 놓고, 글자로 만드는 일도 어떤 값이든 받아 주는
// cellText에 맡긴다(text가 문자열이 아닐 수 있는 마지막 경우까지 여기서 끝난다).
function BrokenMessage({ role, text }) {
  const who = role === 'user' ? 'user' : 'assistant';
  return (
    <div className={`row ${who}`}>
      <div className={`bubble ${who}`}>
        <div className="md">
          <p><em>이 답변을 그리지 못했습니다 — 원문을 그대로 보입니다.</em></p>
          <pre><code>{cellText(text)}</code></pre>
        </div>
      </div>
    </div>
  );
}

// 메시지 하나에 그리는 차트 수의 예산. 블록은 자기가 몇 번째인지 모르므로 메시지가 렌더될 때마다
// 새 카운터를 내려 주고 블록이 차례로 가져간다. 렌더 순서가 곧 문서 순서라 앞의 넷이 차트가 된다.
// (Message는 memo라 본문이 그대로면 다시 렌더되지 않고, 다시 렌더되면 카운터도 새것이다.)
const ChartBudget = createContext(null);

function ChartBlock({ text }) {
  const budget = useContext(ChartBudget);
  const block = splitBlock(text);
  const parsed = parseChartBlock(text, block);
  const draw = parsed.ok && budget && budget.n++ < MAX_CHARTS_PER_MESSAGE;
  const titled = <ChartTable text={text} block={block} withTitle />;
  if (!draw) return titled;
  const table = <ChartTable text={text} block={block} />;
  // 그리다 던지면 '그리지 않은 블록'과 같은 모양(제목 + 표)으로 — '표로 보기'까지 경계 안에 두어 표가 두 번 남지 않게 한다.
  return (
    <Boundary what="chart" fallback={titled}>
      <Suspense fallback={table}>
        <Chart spec={parsed.spec} />
      </Suspense>
      {/* 차트는 값을 읽는 데 한계가 있다(정확한 수치·잘린 라벨·그리지 않은 열). 표는 늘 곁에 둔다. */}
      <details className="chart-table"><summary>표로 보기</summary>{table}</details>
    </Boundary>
  );
}

function MermaidBlock({ text }) {
  const code = <pre><code>{text}</code></pre>;
  return (
    <Boundary what="mermaid" fallback={code}>
      <Suspense fallback={code}>
        <Mermaid text={text} />
      </Suspense>
    </Boundary>
  );
}

// 코드펜스의 언어 표시(```chart → class="language-chart")를 hast 노드에서 읽는다. react-markdown은
// <pre> 컴포넌트에 node를 넘겨 주고, 그 첫 자식이 <code>다. 언어가 chart·mermaid면 우리가 그리고,
// 그 밖은 원래대로 코드블록이다. 대소문자는 가리지 않는다(```Chart 도 온다).
const codeOf = node => {
  const code = node?.children?.[0];
  if (code?.type !== 'element' || code.tagName !== 'code') return null;
  const cls = code.properties?.className;
  const lang = (Array.isArray(cls) ? cls : [cls]).map(c => /^language-(.+)$/i.exec(String(c ?? ''))?.[1]).find(Boolean);
  const text = (code.children ?? []).map(c => (c.type === 'text' ? c.value : '')).join('').replace(/\n$/, '');
  return { lang: lang?.toLowerCase(), text };
};
function PreOrBlock({ node, children, ...props }) {
  const code = codeOf(node);
  if (code?.lang === 'chart') return <ChartBlock text={code.text} />;
  if (code?.lang === 'mermaid') return <MermaidBlock text={code.text} />;
  return <pre {...props}>{children}</pre>;
}
// 답변 속 링크는 새 탭에서 연다 — 같은 탭에서 열리면 대화가 통째로 사라진다(이력은 서버에 없다).
// 페이지 안 앵커(#…)만 제자리에서 연다. noopener는 새 탭이 이 창(window.opener)을 만지지 못하게,
// noreferrer는 사내 URL이 링크 대상에 referer로 새지 않게 한다.
// javascript: 같은 위험한 주소는 react-markdown이 걸러 href=""로 넘기는데, 빈 href는 '현재 문서'라
// 누르면 페이지가 다시 읽혀 대화가 사라진다 — 그런 것은 href 없는 글자로만 남긴다.
// 링크 안인가. 그 안에서는 <a>를 또 열 수 없다 — 중첩 앵커는 DOM이 받아들이지 않고(React가 경고를
// 내며 그대로 그린다) 바깥 링크가 눌리지 않게 된다. 그림(AltImage)이 이것을 보고 글자로만 남는다.
const InLink = createContext(false);
function MarkdownSpan({ node, ...props }) {
  const inLink = useContext(InLink);
  return props.className?.split(' ').includes('katex')
    ? <InlineMath {...props} inLink={inLink} /> : <span {...props} />;
}
function NewTabLink({ node, href, children, ...props }) {
  // 열 주소와 얹을 속성을 한 번에 받는다(markdown.js linkTarget) — 걷어낸 값으로 판정해 놓고
  // 걷어내기 전 주소를 href에 쓰는 어긋남을 만들 수 없게. 걷어내면 아무것도 남지 않는 주소는
  // 글자로만 남긴다: 빈 href는 '현재 문서'라 누르면 페이지가 다시 읽혀 대화가 사라진다.
  const { url, attrs } = linkTarget(href);
  const link = url === ''
    ? <a {...props}>{children}</a>
    : <a href={url} {...props} {...attrs}>{children}</a>;
  return <InLink.Provider value={true}>{link}</InLink.Provider>;
}
// 답변 속 그림(![글자](주소))은 자동으로 불러오지 않는다. 그 주소는 모델이 쓴 것이고, 모델이 보는
// 재료에는 조회 결과(자유 텍스트가 섞인다)가 들어 있다 — 브라우저는 사용자가 누르기도 전에 그 주소를
// 부르므로, 사내 화면이 밖으로 신호를 보내는(그것도 주소에 값을 실어) 유일한 통로가 그것이다.
// 링크에 noreferrer까지 붙여 사내 주소가 새지 않게 하는 이 화면에서, 저절로 나가는 요청은 앞뒤가 맞지 않는다.
// 이 앱의 답변에 그림이 실릴 자리도 없다 — 차트도 흐름도도 우리가 그린다. 그래서 주소는 링크로만 남긴다:
// 무엇을 가리키는지 보이고, 열지 말지는 사람이 정한다. (그림을 정말 띄워야 하는 날이 오면 여기만 되돌린다)
function AltImage({ node, src, alt, title }) {
  const label = String(alt || title || '').trim();
  // 판정(kind)과 열 주소(url)·속성(attrs)을 한 번에 받는다 — 셋이 같은 값에서 나온다.
  const { url, kind, attrs } = imageTarget(src, useContext(InLink));
  if (kind === 'text') {
    // 열어 주지 않더라도 무엇을 가리키는지는 보여야 한다 — 링크 안의 그림처럼 주소가 아예 닿을 수
    // 없는 자리에서는 이것이 유일한 단서다. 아주 긴 주소(data: 등)는 끝을 줄인다.
    const where = url ? ` (${clip(url, 60)})` : '';
    return <em>🖼 {label || '이미지'}{where}</em>;
  }
  // 글자가 없으면 주소로 대신하되 여기서도 끝을 줄인다 — 조회 결과가 섞여 들어간 주소는 수천 자가
  // 되기도 하고, 그대로 두면 답변 한 줄이 주소의 벽에 묻힌다(주소 전부는 href에 그대로 남는다).
  return (
    <a href={url} title={title || undefined} {...attrs}>🖼 {label || clip(url, 60)}</a>
  );
}

// 플러그인 배열과 마찬가지로 모듈 상수여야 한다 — 새 객체를 넘기면 매 렌더가 파이프라인 재구축이다.
// (urlTransform과 img를 한 벌로 묶는 이유는 위 TABLE_MD 참고)
const MAIN_MD = mdProps({ pre: PreOrBlock, a: NewTabLink, img: AltImage, span: MarkdownSpan });
const PREVIEW_MD = mdProps({ pre: PreviewPre, a: NewTabLink, img: AltImage, span: MarkdownSpan });

// 미리보기 본문도 memo 뒤에 둔다 — 완성된 답을 Message(memo)로 감싼 것과 같은 이유이고, 다른 것은
// 이쪽이 '살아 있는 화면'이라 값을 치르는 순간이 하필 사용자가 무언가를 하고 있을 때라는 점뿐이다.
// react-markdown은 렌더마다 markdown 전체를 다시 파싱한다(v10 Markdown()은 runSync(parse(file))를
// 그대로 부른다 — 안에 memo도 useMemo도 없다). 이 자리는 App의 렌더 안에 있으므로, 조각이 하나도
// 오지 않아도 App이 다시 렌더될 때마다 그 파싱이 한 번씩 돈다. App을 다시 렌더시키는 것 중 사용자가
// 답을 기다리는 동안 실제로 하는 일이 하나 있다: 다음 질문을 입력창에 치는 것(setInput)이다.
// 그래서 글자 하나마다 미리보기 전체가 다시 파싱되고 입력이 그만큼 멈춘다 — 실측(Chrome, 키 입력
// 하나의 동기 비용): 미리보기 6.6k자 25ms, 20k자 67ms, 63k자 200ms. 답변 상한이 70,000자
// (backend MAX_ANSWER_LEN)이므로 긴 답을 기다리는 동안에는 글자마다 0.2초씩 얼어붙는다.
// 한글은 조합 중에도 input 이벤트가 자모마다 오므로 그만큼 더 자주 치른다.
// (같은 글이 done 뒤 memo된 말풍선에 들어가면 같은 키 입력이 0~2ms다 — 비용은 markdown 파싱이지
//  화면 크기가 아니다.)
// text·rehypePlugins가 그대로면 memo가 파싱까지 함께 막는다 — 플러그인 배열은 useMarkdownPlugins가
// useMemo로 붙들고 있고(위 NO_REHYPE 참고), 나머지는 모듈 상수다.
const PreviewBody = memo(function PreviewBody({ text, rehypePlugins }) {
  return (
    <div className="md preview">
      <ChartBudget.Provider value={{ n: 0 }}>
        <ReactMarkdown remarkPlugins={REMARK_PLUGINS} rehypePlugins={rehypePlugins} {...PREVIEW_MD}>{text}</ReactMarkdown>
      </ChartBudget.Provider>
    </div>
  );
});

// 입력창 타이핑마다 전체 대화가 다시 렌더되지 않도록 메시지 하나를 분리해 memo한다
// (assistant 답변은 markdown 파싱 비용이 있어 대화가 길어질수록 체감된다)
// 조회된 행을 CSV 파일로 내려준다. 클립보드가 아닌 파일인 이유: navigator.clipboard는 https·localhost
// 밖(사내망의 http 배포)에서는 없고, 수백 행짜리 결과는 어차피 다른 도구로 가져가 쓰는 것이다.
function downloadCsv(step) {
  const url = URL.createObjectURL(new Blob([toCsv(step.rows)], { type: 'text/csv;charset=utf-8' }));
  const a = document.createElement('a');
  a.href = url;
  a.download = csvFileName(step.query_name, step.targetDb);
  // 문서에 붙였다 뗀다 — 떠 있는 앵커의 click()은 브라우저에 따라 내려받기를 시작하지 않는다(구형 Firefox).
  document.body.appendChild(a);
  a.click();
  a.remove();
  // 즉시 해제하면 일부 브라우저가 내려받기를 시작하기 전에 URL을 잃는다 — 한 틱 뒤에 해제한다.
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

// 실행된 쿼리 한 건: 이름·대상 DB·바인드 한 줄 + 조회된 행 전부의 표.
// 모델은 결과를 20행까지만 보고 그중 몇 행만 답변에 옮겨 적으므로, 사용자가 조회 결과 전체를 보는
// 자리는 이 표뿐이다. 그래서 여기서는 행을 자르지 않는다(서버도 자르지 않는다 — result.js).
// showGrid: 표는 패널이 한 번 펼쳐진 뒤에만 만든다(Message가 준다). 한 스텝이 1000행 × 30열이면 셀
// 6만 개다 — 답변마다 펼치지도 않은 표를 DOM에 올리면 대화가 길어질수록 화면 전체가 무거워진다.
function TraceStep({ step: t, showGrid }) {
  const rows = t.rows ?? [];
  return (
    <div className="trace-step">
      <div className="trace-head">
        {/* 번호는 서버가 준 이력의 절대 순번이다 (result.js clientTrace) — 모델이 답변에서 "3번 조회"라고 말할 때
            사용자가 여기서 세는 번호와 같아야 한다. 남은 것만 다시 세면 걸러진 항목만큼 어긋난다. */}
        {t.step !== undefined && <span className="trace-no">{t.step}.</span>}
        {/* 대상 DB가 여럿인 쿼리는 쿼리 이름만으로 무엇을 조회했는지 알 수 없다.
            대상이 하나인 등록에서도 함께 보여준다 — 있고 없고가 등록 형태에 따라 갈리면
            같은 화면이 어떤 줄에서만 DB를 밝히게 되어 그 차이가 뜻으로 읽힌다.
            실행되지 않은 스텝(오류·미등록)에는 서버가 값을 주지 않을 수 있다. */}
        <code>{t.query_name}{t.targetDb ? `@${t.targetDb}` : ''} {JSON.stringify(t.params)}</code>
        <span className="trace-count">{stepLabel(t)}</span>
        {rows.length > 0 && <button type="button" className="trace-csv" onClick={() => downloadCsv(t)}>CSV 내려받기</button>}
      </div>
      {showGrid && rows.length > 0 && <TraceGrid rows={rows} />}
    </div>
  );
}

function TraceGrid({ rows }) {
  const cols = columnsOf(rows);
  const ref = useRef(null);
  // 이 표가 상자 밖으로 나가 있는가(세로든 가로든). 종이에는 스크롤이 없어 그만큼이 그냥 잘리므로,
  // 잘리는 표에만 인쇄용 안내를 붙인다 — 다 들어가는 표에까지 붙이면 거짓말이 된다.
  const [clipped, setClipped] = useState(false);
  // 세로 스크롤바가 실제로 생긴 표는 휠을 붙잡는다(overscroll-behavior: contain) — 표 위에서 굴린 휠은
  // 표만 움직이고, 끝에 닿아도 대화로 번지지 않는다. 그냥 두면 Chrome은 휠 제스처를 표에 걸어(latching)
  // 끝에 닿은 뒤로는 멈춘 듯하다가 잠깐 쉬면 그때부터 대화가 움직여, 같은 자리에서 굴려도 표가 움직일지
  // 대화가 움직일지 매번 다르다. 대화를 내리려면 표 밖(말풍선 옆 여백)에서 굴린다.
  // 스크롤바가 없는 표에 걸면 안 된다 — Chrome은 contain인 상자를 굴릴 것이 없어도 경계로 삼아,
  // 몇 줄짜리 표가 휠이 죽는 자리가 된다. 그래서 CSS가 아니라 여기서 실제로 넘치는지 재어 건다.
  // 숨은 부분이 보이는 높이의 절반도 안 되는 표도 걸지 않는다 — 몇 px 움직이고 멎는 표는 읽을 것이
  // 있는 스크롤 상자가 아니라 휠이 죽는 자리다. 그런 표는 원래 규칙대로 잠깐 쉬면 대화로 넘어간다.
  // 높이 상한이 55vh라 창 높이에 따라 넘치고 안 넘치고가 달라지므로 크기가 바뀔 때마다 다시 잰다
  // (패널이 접혀 있으면 높이가 0이라 걸리지 않고, 펼치면 크기가 바뀌어 다시 잰다).
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const mark = () => {
      el.style.overscrollBehavior = el.scrollHeight - el.clientHeight > el.clientHeight / 2 ? 'contain' : '';
      setClipped(el.scrollHeight - el.clientHeight > 1 || el.scrollWidth - el.clientWidth > 1);
    };
    // 한 번은 반드시 잰다. 관찰자가 없는 브라우저에서 그냥 돌아가면 휠 가두기도, 인쇄 안내도 영영
    // 붙지 않아 1000행짜리 표가 잘린 채 소리 없이 인쇄된다.
    mark();
    if (typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(mark);
    ro.observe(el);
    return () => ro.disconnect();
  }, [rows]);
  return (
    <>
    <div className="trace-grid" ref={ref}>
      <table>
        <thead>
          <tr><th className="idx">#</th>{cols.map(c => <th key={c}>{c}</th>)}</tr>
        </thead>
        <tbody>
          {rows.map((r, i) => (
            <tr key={i}>
              <td className="idx">{i + 1}</td>
              {cols.map(c => {
                const v = cellValue(r, c);
                return <td key={c} className={typeof v === 'number' ? 'num' : v == null ? 'null' : undefined}><div className="cell">{cellText(v)}</div></td>;
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
    {/* 화면에서는 감춰 두고 인쇄에서만 보인다 (index.html의 @media print) */}
    {clipped && <p className="trace-print-note">인쇄물에는 이 표의 화면에 보이던 부분만 담깁니다 — 전체 {rows.length}행은 ‘CSV 내려받기’로 저장하세요.</p>}
    </>
  );
}

// 검색 한 건: 검색어·대상·대상별 적중 수 한 줄. 답을 기다리는 동안 진행 줄로 보이던 것이 답이 온 뒤에는
// 여기 남는다 — 무엇을 찾아봤는지가 사라지면 '왜 이 답인가'의 절반이 사라진다. 표도 CSV 단추도 없다.
function TraceSearch({ step: t }) {
  return (
    <div className="trace-step trace-search">
      <div className="trace-head">
        {t.step !== undefined && <span className="trace-no">{t.step}.</span>}
        <code>🔎 검색 "{t.search}" ({targetsLabel(t.targets)})</code>
        <span className="trace-count">{stepLabel(t)}</span>
      </div>
    </div>
  );
}

// '⚡ 검색 N회 · 실행된 쿼리 M건' 패널. 펼침 상태를 Message가 아니라 여기 두는 이유: Message가 다시 렌더되면
// markdown을 다시 파싱하고 차트를 다시 그린다 — 패널을 여닫는 일이 그 비용을 내서는 안 된다.
// 검색 항목과 쿼리 항목은 서버가 준 순서 그대로다(그 번호가 모델이 본 스텝 번호다 — backend result.js).
function TracePanel({ trace }) {
  // 한 번이라도 펼쳤는가 — 그 뒤로는 접어도 표를 지우지 않는다(다시 펼칠 때 재생성 비용을 내지 않게).
  const [opened, setOpened] = useState(false);
  return (
    <details className="trace" onToggle={e => { if (e.currentTarget.open) setOpened(true); }}>
      <summary>⚡ {traceSummary(trace)}</summary>
      {trace.map((t, j) => (isSearchStep(t) ? <TraceSearch key={j} step={t} /> : <TraceStep key={j} step={t} showGrid={opened} />))}
    </details>
  );
}

// 답을 기다리는 동안의 진행 줄 — 검색·조회가 시작되면 바로 서고(서버가 흘려보내는 이벤트, backend agent.js),
// 끝나면 같은 줄에 결과가 붙는다. 답이 오면 이 목록은 사라지고 같은 내용이 답 아래 패널(TracePanel)에 남는다.
// 글자는 trace.js progressText가 만든다 — 패널과 같은 말을 쓰게.
function ProgressList({ items }) {
  return (
    <ul className="progress" aria-live="polite">
      {items.map((it, i) => (
        <li key={i} className={it.pending ? 'pending' : undefined}>
          <span className="progress-icon" aria-hidden="true">{it.kind === 'search' ? '🔎' : '⚡'}</span>
          <span className="progress-text">{progressText(it)}</span>
        </li>
      ))}
    </ul>
  );
}

const Message = memo(function Message({ role, text, trace, stopped }) {
  const rehypePlugins = useMarkdownPlugins(REHYPE_PLUGINS);
  // 말풍선 하나가 던져도 나머지 대화는 남는다 (Boundary 참고). 경계를 memo 안에 두는 이유는
  // 바깥에 두면 대화가 늘 때마다 경계가 다시 렌더되기 때문이다 — 여기 두면 memo가 함께 막는다.
  return (
    <Boundary what="message" fallback={<BrokenMessage role={role} text={text} />}>
      <div className={`row ${role}`}>
        <div className={`bubble ${role}`}>
          {role === 'assistant'
            ? <div className="md">
                {/* 플러그인 배열은 math.js의 상수를 그대로 쓴다 — react-markdown은 렌더마다 options로
                    파이프라인을 다시 조립하므로, 여기서 새 배열 리터럴을 만들면 매 렌더가 프로세서 재구축이 된다. */}
                <ChartBudget.Provider value={{ n: 0 }}>
                  <ReactMarkdown remarkPlugins={REMARK_PLUGINS} rehypePlugins={rehypePlugins}
                                 {...MAIN_MD}>{text}</ReactMarkdown>
                </ChartBudget.Provider>
              </div>
            : text}
          {/* 패널에 경계를 하나 더 두는 이유: 여기서 던졌다고 답변까지 원문으로 되돌릴 이유가 없다.
              trace는 조회 결과에서 온 구조라 답변 글자보다 모양이 어긋날 길이 많다(normalizeTrace가
              문 앞에서 맞추지만, 그 뒤로 늘어날 필드까지 미리 알 수는 없다). */}
          {trace?.length > 0 && (
            <Boundary what="trace" fallback={<div className="trace"><em>실행된 쿼리를 보여주지 못했습니다.</em></div>}>
              <TracePanel trace={trace} />
            </Boundary>
          )}
          {stopped && <p className="stopped-note">응답 생성이 중지되었습니다.</p>}
        </div>
      </div>
    </Boundary>
  );
});


// Extracted from llm_agent/frontend/src/App.jsx; session ownership belongs to MnemoArc.
export { Message };
export function StreamingMessage({text}) {
  const plugins = useMarkdownPlugins(REHYPE_PLUGINS);
  return <Boundary key={text ? 'text' : 'empty'} what="stream" fallback={<BrokenMessage role="assistant" text={text} />}><div className="row assistant"><div className="bubble assistant"><PreviewBody text={text} rehypePlugins={plugins} /></div></div></Boundary>;
}
