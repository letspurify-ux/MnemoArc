// 차트 블록 하나를 그린다. 무엇을 그릴지는 chart.js parseChartBlock이 이미 정했고, 여기는 그 명세를
// Recharts 요소로 옮기는 일만 한다. App.jsx가 React.lazy로 부르므로 이 파일과 recharts는 첫 차트가
// 나올 때까지 내려받지 않는다 — 대부분의 답변은 글과 표뿐이다.
import { useId, useLayoutEffect, useMemo, useRef, useState } from 'react';
import {
  ResponsiveContainer, ComposedChart, PieChart, ScatterChart,
  Bar, Line, Area, Pie, Scatter, Cell, XAxis, YAxis, CartesianGrid, Tooltip,
} from 'recharts';
import { pieData, fmtScaledNum, chartNotes, fmtNum, pieLabelsOverflow, fitText } from './chart.js';

// 첫 색은 앱의 강조색(index.html --accent)을 따라가고, 나머지는 서로 구별되는 고정 팔레트다.
// 강조색은 이 모듈이 처음 실행될 때 한 번 읽는다 — 차트가 나올 시점에는 문서가 이미 그려져 있다.
const accent = (typeof getComputedStyle === 'function' && typeof document !== 'undefined'
  && getComputedStyle(document.documentElement).getPropertyValue('--accent').trim()) || '#4f46e5';
const PALETTE = [accent, '#0ea5e9', '#10b981', '#f59e0b', '#ef4444', '#8b5cf6', '#14b8a6', '#f97316', '#64748b'];
const color = i => PALETTE[i % PALETTE.length];

// 조각 위에 얹는 글자의 색. 팔레트에는 밝은 색(황색 #f59e0b, 주황 #f97316)이 있어 흰 글자를 얹으면
// 대비가 2:1도 되지 않는다 — 11px 글자는 그 위에서 사실상 보이지 않는다(잘린 라벨을 고치려다
// 있으나 마나 한 라벨을 얻는다). 바탕의 밝기를 재어 흰 글자와 진한 글자 중 잘 보이는 쪽을 쓴다.
// 색을 읽지 못하면(강조색이 hex가 아닐 수 있다) 밝기 0으로 보아 흰 글자 — 지금까지의 모습 그대로다.
// 진한 쪽이 순검정인 이유: 검정과 흰색 중 잘 보이는 쪽을 고르면 어떤 바탕에서도 대비가 4.58:1
// 아래로 내려가지 않는다(두 대비가 같아지는 밝기에서의 값). 덜 검은 회색으로 하면 붉은색·보라색
// 조각이 4.5:1을 넘지 못한다 — 11px 글자는 큰 글자로 쳐 주지 않는다.
const HEX_RE = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i;
const luminance = c => {
  const m = HEX_RE.exec(String(c).trim());
  if (!m) return 0;
  const h = m[1].length === 3 ? [...m[1]].map(x => x + x).join('') : m[1];
  const [r, g, b] = [0, 2, 4].map(i => {
    const v = parseInt(h.slice(i, i + 2), 16) / 255;
    return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
};
// 0.179: 흰 글자와 검은 글자의 대비가 뒤집히는 밝기다 (WCAG 대비식을 두 색에 대해 풀면 나온다).
const inkOn = c => (luminance(c) > 0.179 ? '#000' : '#fff');

// 범주가 이보다 많으면 가로 막대로 눕힌다 — 세로 막대의 x 라벨은 열둘을 넘기면 서로를 덮는다.
const HORIZONTAL_FROM = 13;
const HEIGHT = 260;

// 눕힌 막대의 범주 축(YAxis)이 가져갈 수 있는 상자 폭의 몫. 그 축은 width="auto"라 라벨이 긴 만큼
// 넓어지는데, 라벨은 조회 결과의 셀 값 그대로다(30자까지, chart.js MAX_LABEL_LEN) — 11px 글자로 330px이다.
// 그러면 좁은 화면에서는 축 하나가 상자를 통째로 먹고 그림이 통째로 사라진다: 실측으로 창 320px에서
// 막대도 축도 눈금선도 하나 없이 라벨만 열다섯 줄 남았고, 창 380px에서는 막대가 설 자리가 23px이었다
// (넓은 화면에서는 269px). 아무 오류도 나지 않는다 — 사용자에게는 그냥 차트가 없는 답변이다.
// 그래서 축에는 상자의 절반까지만 내주고, 넘치는 글자는 …로 줄인다(fitText). 줄인 이름의 온전한 값은
// 툴팁(full)과 차트 곁의 '표로 보기'에 그대로 있다 — 축 눈금을 30자로 묶어 둔 것과 같은 처방이다.
const CATEGORY_AXIS_MAX = 0.5;

const pad2 = n => String(n).padStart(2, '0');
const MIN = 60_000;
const HOUR = 60 * MIN;
const ymdOf = d => `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
const hmOf = d => `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
// 시간 축의 눈금과 그 글자. 글자의 해상도가 눈금 간격보다 거칠면 같은 글자가 잇따라 찍힌다 — 이틀치
// 날짜 자료에 Recharts는 여섯 시간 간격으로 눈금을 세우고, 그것을 날짜로만 찍으면 '03-01'이 넷이다.
// 그래서 형식은 눈금마다 다른 글자가 나오도록 고른다:
//   값이 전부 자정(날짜 자료)이면 눈금을 자료의 날짜 자체로 세우고 연-월-일로 찍는다. 열흘을 넘는
//   범위는 Recharts의 눈금(하루 이상 간격)에 맡겨도 날짜가 겹치지 않는다.
//   시각이 있는 자료는 범위가 열흘 이상이면 눈금 간격이 하루 이상이라 연-월-일, 십 분 안쪽이면 눈금이
//   분 아래로 내려가므로 초까지, 같은 날짜 안이면 시:분, 그 밖은 월-일 시:분(같은 시각이 다른 날짜에
//   되풀이된다). 값이 하나뿐이면 다 찍는다 — 비교할 이웃이 없다.
const timeAxis = rows => {
  const xs = rows.map(r => r.x);
  const span = xs.length > 1 ? xs[xs.length - 1] - xs[0] : 0;
  const dateOnly = xs.every(x => { const d = new Date(x); return d.getHours() === 0 && d.getMinutes() === 0 && d.getSeconds() === 0 && d.getMilliseconds() === 0; });
  const sameDay = ymdOf(new Date(xs[0])) === ymdOf(new Date(xs[xs.length - 1]));
  const tickFormatter = t => {
    const d = new Date(t);
    if (dateOnly || span >= 240 * HOUR) return ymdOf(d);
    const seconds = `${hmOf(d)}:${pad2(d.getSeconds())}`;
    // 짧은 범위의 눈금은 초 사이에도 선다. 초까지만 쓰면 서로 다른 눈금이 같은 시각으로 보인다.
    const time = span < 10_000 ? `${seconds}.${String(d.getMilliseconds()).padStart(3, '0')}`
      : span < 10 * MIN ? seconds : hmOf(d);
    if (xs.length < 2) return `${ymdOf(d)} ${time}`;
    return sameDay ? time : `${ymdOf(d).slice(5)} ${time}`;
  };
  const ticks = dateOnly && span < 240 * HOUR ? [...new Set(xs)] : undefined;
  return { tickFormatter, ticks };
};

// 축 눈금의 글자 크기. 상수인 이유는 그리는 쪽만의 값이 아니기 때문이다 — 눕힌 막대의 범주 눈금이
// 상자를 얼마나 가져가는지 재는 쪽(아래 Cartesian의 catTick)이 같은 크기로 글자를 재야 한다.
const AXIS_FONT_PX = 11;
const axisStyle = { fontSize: AXIS_FONT_PX, fill: '#6b7280' };
// 툴팁 상자. recharts의 기본 스타일은 white-space: nowrap이라 이름 하나가 길면 상자가 그만큼
// 옆으로 자란다 — 이름을 MAX_NAME_LEN으로 묶어도 60자면 한 줄이 700px이고, 좁은 화면에서는 그대로
// 창 밖이다(그리고 말풍선의 overflow-x: clip에 잘려 읽을 수 없다). 그래서 폭을 묶고 줄바꿈을 켠다.
// maxWidth 240px: 가장 좁은 화면(320px 창)의 그림 상자 안에도 들어가는 폭이라, recharts가
// 커서 반대쪽으로 뒤집어 두는 것만으로 툴팁이 늘 그림 안에 선다.
const tooltipStyle = { fontSize: 12, borderRadius: 8, border: '1px solid #e5e7eb', maxWidth: 240, whiteSpace: 'normal' };

// 툴팁의 제목은 잘린 라벨(label)이 아니라 원래 값(full)으로 보여준다.
const labelOf = (label, payload) => payload?.[0]?.payload?.full ?? label;

// 상자의 폭을 잰다. 그리는 쪽이 상자보다 넓어지려 할 때 무엇을 줄일지 정하려면 폭을 알아야 하고,
// 그 폭은 창 크기·말풍선 폭에 따라 바뀐다. 원그래프가 라벨 자리를 재는 것(아래 useLabelsFit)과 같은
// 처방이다 — 관찰자는 실제로 그 값을 쓰는 그래프에만 단다.
function useBoxWidth(enabled) {
  const ref = useRef(null);
  const [width, setWidth] = useState(0);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el || !enabled) return;
    // 그리기 전에 정해져야 한 프레임도 긴 라벨이 비치지 않는다(useLayoutEffect인 이유).
    const mark = () => setWidth(el.clientWidth);
    mark();
    if (typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(mark);
    ro.observe(el);
    return () => ro.disconnect();
  }, [enabled]);
  return [ref, width];
}

function Cartesian({ spec }) {
  const { type, xKind, series, rows } = spec;
  const hasRight = series.some(s => s.axis === 'right');
  // 범주가 많은 막대는 눕힌다. 오른쪽 축이 있으면 눕히지 않는다 — 눕힌 그래프의 '오른쪽 축'은 없다.
  const horizontal = type !== 'line' && type !== 'area' && xKind === 'category' && !hasRight && rows.length >= HORIZONTAL_FROM;
  const stacked = type === 'stacked-bar';
  const height = horizontal ? Math.max(HEIGHT, rows.length * 22 + 60) : HEIGHT;

  // 범례를 그림 상자 밖에 두므로(아래) 폭을 재는 상자는 늘 필요하다. 관찰자는 그 값을 실제로 쓰는
  // 눕힌 막대에서만 단다 — 세로 그래프의 축은 숫자라 라벨이 길어질 일이 없다.
  const [boxRef, boxW] = useBoxWidth(horizontal);
  // 눕힌 막대의 범주 눈금: 상자의 몫(CATEGORY_AXIS_MAX)을 넘는 글자는 …로 줄여 축이 그림을 먹지 못하게.
  // 폭은 실제로 그려질 글꼴·크기로 잰다(textMeasurer) — 글자 수로 어림하면 한글과 영문이 섞인 이름에서
  // 어긋나고, 그 어긋남은 축이 넓어지는 쪽으로도 난다. 자를 자리를 정하는 셈은 chart.js fitText다.
  const catTick = useMemo(() => {
    if (!horizontal || !(boxW > 0) || !boxRef.current) return undefined;
    const measure = textMeasurer(boxRef.current, AXIS_FONT_PX);
    const room = boxW * CATEGORY_AXIS_MAX;
    return v => fitText(String(v ?? ''), room, measure);
  }, [horizontal, boxW]);

  // x축: 범주는 라벨 그대로, 시간·숫자는 수치 축 위에 실제 값의 자리에 찍는다.
  // interval은 양끝을 지키는 쪽으로 — 기본값(preserveEnd)은 축의 왼쪽 끝에 선 첫 눈금의 글자가 축 밖으로
  // 삐져나오면 그 눈금을 지운다. 그러면 이틀치 자료의 축에 둘째 날 하나만 남는다.
  const xAxis = xKind === 'category'
    ? { dataKey: 'label', type: 'category', interval: horizontal ? 0 : 'preserveStartEnd' }
    : { dataKey: 'x', type: 'number', domain: ['dataMin', 'dataMax'], scale: xKind === 'time' ? 'time' : 'linear',
        interval: 'preserveStartEnd', ...(xKind === 'time' ? timeAxis(rows) : { tickFormatter: fmtNum }) };

  const items = series.map((s, i) => {
    const key = `${s.name}#${i}`;
    // dataKey가 함수인 이유: 행의 값은 배열(values)이라 열 이름이 'x'·'label' 같은 우리 키와 겹쳐도 안전하다.
    const common = { dataKey: d => d.values[i], name: s.name, yAxisId: s.axis, isAnimationActive: false };
    const c = color(i);
    // 오른쪽 축의 시리즈는 그래프 종류와 무관하게 선이다 — 막대 위에 겹친 다른 단위의 값은 선일 때만 읽힌다.
    if (s.axis === 'right' || type === 'line') return <Line key={key} {...common} type="monotone" stroke={c} strokeWidth={2} dot={rows.length <= 30} />;
    if (type === 'area') return <Area key={key} {...common} type="monotone" stroke={c} fill={c} fillOpacity={0.18} strokeWidth={2} />;
    // 둥근 모서리는 막대의 끝쪽 둘 — 눕힌 막대는 오른쪽 끝이다.
    return <Bar key={key} {...common} fill={c} stackId={stacked ? 'stack' : undefined} maxBarSize={48}
                radius={stacked ? 0 : horizontal ? [0, 3, 3, 0] : [3, 3, 0, 0]} />;
  });

  // Recharts 3은 툴팁 항목을 이름의 가나다순으로 늘어놓는다. 표의 열 순서(=쌓인 순서)를 따라야 읽는 사람이 헷갈리지 않는다.
  const byColumn = item => { const i = series.findIndex(s => s.name === item.name); return i < 0 ? series.length : i; };

  return (
    <div ref={boxRef}>
    <ResponsiveContainer width="100%" height={height}>
      <ComposedChart data={rows} layout={horizontal ? 'vertical' : 'horizontal'} margin={{ top: 8, right: hasRight ? 8 : 16, bottom: 4, left: 0 }}>
        {/* 눈금선은 값 축에만. 가로 막대에서는 값 축이 X이므로 세로선을 긋는다.
            yAxisId를 적어 주는 이유는 아래 툴팁의 axisId와 같다: Recharts 3의 격자도 자기 축을 id로
            찾고 기본값은 0인데, 이 화면의 Y축은 전부 이름 붙은 축("left"·"right")이라 0번 축이 없다.
            그러면 격자는 눈금을 하나도 찾지 못하고 상자의 위·아래 경계선 두 줄만 긋는다 — 값 축의
            눈금선이 통째로 사라지는 셈이라, 막대·선의 값을 눈으로 읽을 수 없다(실측: 눈금 0·3·6·9·12에
            선은 맨 위와 맨 아래 둘뿐이었다). X축은 id 없는 축이라 기본값 0이 그대로 맞는다. */}
        <CartesianGrid strokeDasharray="3 3" stroke="#e5e7eb" horizontal={!horizontal} vertical={horizontal} yAxisId="left" />
        {horizontal
          ? <>
              <XAxis type="number" tick={axisStyle} tickFormatter={fmtNum} />
              <YAxis yAxisId="left" dataKey="label" type="category" width="auto" tick={axisStyle} interval={0}
                     tickFormatter={catTick} />
            </>
          : <>
              <XAxis {...xAxis} tick={axisStyle} minTickGap={12} />
              <YAxis yAxisId="left" width="auto" tick={axisStyle} tickFormatter={fmtNum} />
              {hasRight && <YAxis yAxisId="right" orientation="right" width="auto" tick={axisStyle} tickFormatter={fmtNum} />}
            </>}
        {/* 툴팁은 마우스 자리에서 '어느 행인가'를 범주 축으로 찾는데, Recharts 3은 그 축을 axisId로 고르고
            기본값은 0이다. 눕힌 그래프의 범주 축은 위의 YAxis(yAxisId="left")라 기본 축이 없어, 툴팁이 엉뚱한
            띠로 행을 나눴다(실측: 15행 중 1·4행은 툴팁이 없고 8행은 1행의 값, 11·15행은 2행의 값을 보였다).
            세로 그래프의 범주 축은 id 없는 XAxis라 기본값 그대로 맞는다. */}
        <Tooltip contentStyle={tooltipStyle} formatter={v => fmtNum(v)} labelFormatter={labelOf} itemSorter={byColumn}
                 axisId={horizontal ? 'left' : undefined} />
        {items}
      </ComposedChart>
    </ResponsiveContainer>
    {/* 시리즈가 하나면 제목이 곧 범례다. 제목도 없으면 이름을 보여줄 자리가 범례뿐이다.
        recharts의 <Legend>가 아니라 상자 밖의 평범한 목록인 이유는 원그래프와 같다(아래 PieView):
        그 범례는 260px 상자 '안'에 들어가 그래프 몫의 높이를 가져간다. 이름은 조회 결과의 열 이름
        그대로라 60자까지 오고(chart.js MAX_NAME_LEN) 시리즈는 여섯까지인데(MAX_SERIES), 그러면 범례가
        230px을 먹어 남는 높이가 0이 된다 — 막대도 축도 눈금선도 하나 없이 범례만 선 그림이 화면에
        남았다(실측: 창 1000px, 60자 × 6열에서 그려진 요소 0개. 막대·선·영역·쌓은 막대 모두 같았다).
        아무 오류도 나지 않아, 사용자에게는 그냥 차트가 그려지지 않은 답변이다.
        밖에 두면 그냥 아래로 자란다 — 잘리는 것도, 그래프를 먹는 것도 없다. 순서는 표의 열 순서
        그대로다(<Legend>에 itemSorter를 걸어 되돌려야 했던 것을 여기서는 그냥 쓰면 된다). */}
    {(series.length > 1 || !spec.title) && (
      <ul className="chart-legend">
        {series.map((s, i) => <li key={i}><i style={{ background: color(i) }} />{s.name}</li>)}
      </ul>
    )}
    </div>
  );
}

// 원그래프의 치수. 라벨을 밖에 둘 자리가 있는지를 재는 쪽(아래 useLabelsFit → chart.js pieLabelsOverflow)이
// 그리는 쪽과 같은 값을 봐야 하므로 한 번만 적는다 — 반지름을 여기서 바꾸고 재는 쪽이 옛 값을 보면
// 판정이 실제 그림과 어긋나 잘리는 라벨이 되살아난다.
const PIE_MARGIN = 4;         // PieChart의 네 변 여백
const PIE_RADIUS = 0.72;      // 바깥 라벨일 때의 반지름 (짧은 변의 절반에 대한 비율)
const PIE_RADIUS_INSIDE = 0.62; // 안쪽 라벨(비율만)일 때 — 범례가 아래 붙으므로 조금 작게
const LABEL_FONT_PX = 11;

// 조각 라벨: 이름과 비율. Recharts가 라벨 자리(x·y·textAnchor)와 비율(percent)을 계산해 넘겨 준다.
// 글자는 아래 labelTextOf와 같아야 한다 — 그쪽이 이 글자의 폭을 재어 자리를 판정한다.
const labelTextOf = (name, percent) => `${name} ${(percent * 100).toFixed(1)}%`;
const sliceLabel = ({ x, y, textAnchor, name, percent }) => (
  <text x={x} y={y} textAnchor={textAnchor} dominantBaseline="central" fontSize={LABEL_FONT_PX} fill="#374151">
    {labelTextOf(name, percent)}
  </text>
);

// 바깥에 둘 자리가 없을 때의 라벨. 원 밖에 이름을 적으면 SVG 경계에 그대로 잘린다 — 창을 좁히면 소리
// 없이 글자가 사라진다(실측: 창 360px에서 이름이 최대 37px, 320px에서 50px 잘렸다). 그래서 자리가
// 없으면 비율만 조각 안에 적고 이름은 범례로 내린다. 글자가 들어갈 수 없는 얇은 조각은 비운다 — 그
// 조각의 이름과 값은 범례와 툴팁에 그대로 있다.
const insideLabel = ({ cx, cy, midAngle, outerRadius, percent, index }) => {
  if (percent < 0.06) return null;
  const rad = (-midAngle * Math.PI) / 180; // Recharts의 각도는 도(°)이고 화면 y는 아래로 자란다
  const r = outerRadius * 0.62;
  return (
    <text x={cx + r * Math.cos(rad)} y={cy + r * Math.sin(rad)} textAnchor="middle"
          dominantBaseline="central" fontSize={LABEL_FONT_PX} fontWeight="600" fill={inkOn(color(index))}>
      {`${(percent * 100).toFixed(0)}%`}
    </text>
  );
};

// 글자의 폭을 재는 함수를 만든다. SVG의 글자와 같은 글꼴로 canvas에 재면 실제로 그려지는 폭과 같다
// (같은 글꼴 엔진이다). 글꼴은 그림이 들어갈 요소에서 읽는다 — 라벨은 글꼴을 따로 정하지 않고 문서의
// 것을 물려받는다(index.html body). canvas가 글꼴 줄을 받아 주지 않으면(이름을 못 읽는 오래된 환경)
// 기본 글꼴(10px sans-serif)로 재어 실제보다 좁게 나오고, 그러면 자리가 있다고 믿고 밖에 두어 잘린다 —
// 그때는 글자 수로 어림한다: 한글·한자는 한 글자가 글자 크기만큼, 나머지는 그 6할.
// (조각 라벨은 LABEL_FONT_PX, 축 눈금은 AXIS_FONT_PX로 부른다 — 재는 글자와 그려지는 글자의
//  크기가 다르면 '들어가는가'의 답도 달라진다)
const CJK_CHAR = /[ᄀ-ᇿ　-鿿가-힯]/;
function textMeasurer(el, px = LABEL_FONT_PX) {
  const ctx = typeof document !== 'undefined' && document.createElement('canvas').getContext?.('2d');
  if (ctx) {
    ctx.font = `${px}px ${getComputedStyle(el).fontFamily}`;
    if (ctx.font.startsWith(`${px}px`)) return t => ctx.measureText(t).width;
  }
  return t => [...t].reduce((w, c) => w + (CJK_CHAR.test(c) ? 1 : 0.6) * px, 0);
}

// 바깥 라벨(이름과 비율)을 둘 자리가 있는가. 상자 폭 하나로 가르던 때에는(380px 아래에서만 안으로)
// 데스크톱 폭에서도 스무 자 이름이 양끝에서 잘렸다(실측: 폭 574px 상자에서 왼쪽 24px·오른쪽 3px) —
// 라벨은 조회 결과의 셀 값이라 폭이 아니라 '이 글자들이 이 자리에 들어가는가'로 정해야 한다. 판정은
// chart.js pieLabelsOverflow(순수 함수라 회귀 테스트가 붙는다)가 하고, 여기서는 글자의 폭을 재고
// 상자 폭이 바뀔 때마다 다시 묻는다. 글자의 폭은 조각이 바뀔 때만 잰다.
// 재는 자리는 원그래프뿐이다 — 막대·선·산점도는 이 값을 읽지 않으므로 관찰자도 달지 않는다.
function useLabelsFit(data, total) {
  const ref = useRef(null);
  const [inside, setInside] = useState(false);
  const labels = useMemo(
    () => data.map(d => labelTextOf(d.name, total > 0 ? d.value / total : 0)), [data, total]);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const measure = textMeasurer(el);
    const widths = labels.map(measure);
    const values = data.map(d => d.value);
    const mark = () => setInside(pieLabelsOverflow({
      width: el.clientWidth, height: HEIGHT, margin: PIE_MARGIN, radiusRatio: PIE_RADIUS, values, widths,
    }));
    mark();
    if (typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(mark);
    ro.observe(el);
    return () => ro.disconnect();
  }, [labels, data]);
  return [ref, inside];
}

function PieView({ spec }) {
  // 조각 고르기(큰 것 남기고 나머지는 '기타')는 chart.js pieSlices — 표 순서와 무관하게 값으로 고른다.
  // 행이 바뀔 때만 고른다: 폭이 바뀔 때마다(useLabelsFit) 다시 렌더되는데, 그때마다 100행을
  // 다시 훑고 정렬할 이유가 없다.
  const { data, scale } = useMemo(() => pieData(spec.rows), [spec.rows]);
  const total = data.reduce((a, d) => a + d.value, 0);
  const pct = v => (total > 0 ? `${((v / total) * 100).toFixed(1)}%` : '');
  const [boxRef, inside] = useLabelsFit(data, total);
  return (
    <div ref={boxRef}>
    <ResponsiveContainer width="100%" height={HEIGHT}>
      <PieChart margin={{ top: PIE_MARGIN, right: PIE_MARGIN, bottom: PIE_MARGIN, left: PIE_MARGIN }}>
        <Pie data={data} dataKey="value" nameKey="name" outerRadius={`${(inside ? PIE_RADIUS_INSIDE : PIE_RADIUS) * 100}%`}
             isAnimationActive={false}
             label={inside ? insideLabel : sliceLabel} labelLine={inside ? false : { stroke: '#9ca3af' }}>
          {data.map((d, i) => <Cell key={i} fill={color(i)} />)}
        </Pie>
        {/* 이름은 조각(범주)의 것이다 — 시리즈 이름은 제목이 이미 말하고, 툴팁이 답해야 할 것은 '어느 조각인가'다.
            조각 라벨은 잘린 것(name)이고 원래 값은 full에 있다. */}
        <Tooltip contentStyle={tooltipStyle} formatter={(v, name, item) => [`${fmtScaledNum(v, scale)} (${pct(v)})`, item?.payload?.full ?? name]} />
      </PieChart>
    </ResponsiveContainer>
    {/* 이름을 조각 곁에 둘 자리가 없을 때만 범례를 단다 — 자리가 있으면 라벨이 이미 이름을 말한다.
        recharts의 <Legend>가 아니라 평범한 목록인 이유: 그 범례는 260px 상자 '안'에 들어가 그래프 몫의
        높이를 가져간다. 이름이 길고 조각이 많으면(최대 12개 × MAX_NAME_LEN자) 좁은 폭에서 열 줄 넘게 감겨 남는
        높이가 0 아래로 내려가고, 그러면 원의 중심과 반지름이 엉뚱한 값이 되어 그래프가 상자 밖으로
        밀려 잘린다. 밖에 두면 그냥 아래로 자란다 — 잘리는 것도, 그래프를 먹는 것도 없다.
        이름은 축 눈금(30자)보다 긴 쪽(full)으로 적는다 — 이 목록은 아래로 감기므로 눈금만큼 줄일
        이유가 없고, 이름이 잘려 사라지는 것을 막으려 단 범례가 다시 눈금만큼 잘린 이름을 보여줄 수는
        없다. 그 길이는 chart.js가 MAX_NAME_LEN으로 이미 묶어 두었다(여기서 다시 자르지 않는다 —
        같은 이름을 두 자리가 다른 길이로 자르면 범례와 툴팁이 다른 글자를 말한다). */}
    {inside && (
      <ul className="chart-legend">
        {data.map((d, i) => (
          <li key={i}><i style={{ background: color(i) }} />{String(d.full ?? d.name)}</li>
        ))}
      </ul>
    )}
    </div>
  );
}

function ScatterView({ spec }) {
  const { rows, series, xKind, xName } = spec;
  const data = rows.map(r => ({ x: r.x, y: r.values[0], full: r.full }));
  const xAxis = xKind === 'time' ? timeAxis(rows) : { tickFormatter: fmtNum };
  const xFmt = xAxis.tickFormatter;
  return (
    <ResponsiveContainer width="100%" height={HEIGHT}>
      <ScatterChart margin={{ top: 8, right: 16, bottom: 4, left: 0 }}>
        <CartesianGrid strokeDasharray="3 3" stroke="#e5e7eb" />
        <XAxis dataKey="x" type="number" name={xName} domain={['dataMin', 'dataMax']} tick={axisStyle} {...xAxis}
               scale={xKind === 'time' ? 'time' : 'linear'} interval="preserveStartEnd" />
        <YAxis dataKey="y" type="number" name={series[0]?.name} width="auto" tick={axisStyle} tickFormatter={fmtNum} />
        <Tooltip contentStyle={tooltipStyle} cursor={{ strokeDasharray: '3 3' }}
                 formatter={(v, name) => [name === xName ? (xKind === 'time' ? rows.find(r => r.x === v)?.full ?? xFmt(v) : fmtNum(v)) : fmtNum(v), name]} />
        <Scatter data={data} fill={color(0)} isAnimationActive={false} />
      </ScatterChart>
    </ResponsiveContainer>
  );
}

// 제목 + 그래프. 표는 App.jsx가 <details>로 아래에 붙인다 — 이 컴포넌트는 그리는 것만 안다.
export default function Chart({ spec }) {
  const id = useId();
  const View = spec.type === 'pie' ? PieView : spec.type === 'scatter' ? ScatterView : Cartesian;
  return (
    <figure className="chart" aria-labelledby={spec.title ? id : undefined}>
      {spec.title && <figcaption id={id}>{spec.title}</figcaption>}
      <View spec={spec} />
      {/* 표에는 있는데 그림에서 빠진 행(행 상한·못 읽은 x·0 이하 조각)을 밝힌다. 문구는 chart.js가
          만든다 — 순수 함수라 회귀 테스트가 붙는다(chartNotes 주석). 문구끼리는 앞머리가 서로 달라
          겹치지 않으므로 문구 자체가 key가 된다. */}
      {chartNotes(spec).map(n => <div key={n} className="chart-note">{n}</div>)}
    </figure>
  );
}
