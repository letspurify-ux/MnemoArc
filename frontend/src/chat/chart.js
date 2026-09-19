// 답변 안의 차트 블록을 다루는 계약 전체 — 무엇을 차트로 받아들이는가(parseChartBlock)와,
// 그 블록을 대화 이력으로 되돌려 보낼 때 어떤 모양으로 줄이는가(chartBlocksToTables).
//
// 블록의 모양은 서버 SYSTEM_PROMPT(llm-openai.js)가 모델에게 가르치고, 서버 chart.js가 `data: step N`
// 참조를 실제 표로 채워 넘긴다. 셋이 같은 줄 문법을 봐야 하므로 여기 CONFIG_RE·표 판정을 바꾸면
// 그 둘도 함께 바꾼다.
//
//   ```chart
//   type: bar | stacked-bar | line | area | pie | scatter
//   title: 제목
//   x: 열이름            (없으면 첫 열)
//   y: 열, 열            (없으면 x를 뺀 숫자 열 전부)
//   y2: 열               (오른쪽 축에 선으로 겹쳐 그릴 열)
//   xtype: time | number | category
//   | 열 | 열 | … |       GFM 표 — 값은 조회 결과 그대로
//   ```
//
// 설정은 전부 '이름: 값' 한 줄이고 표는 GFM 그대로다. JSON을 쓰지 않는 이유는 answer 자체가 이미
// JSON 문자열 안에 실려 오기 때문이다 — 그 안에 따옴표·중괄호를 또 넣으면 이스케이프가 한 번만
// 어긋나도 답변 전체가 파싱에서 떨어진다. 이 모양은 따옴표가 한 개도 없다.
//
// 실패 방향은 한쪽으로만 열려 있어야 한다: 판정하지 못하면(ok:false) 화면에는 안의 표가 그대로
// 보인다. 반대로 어설프게 그리면 숫자가 아닌 것을 0으로 그리거나, 축이 뒤집힌 그래프가 '데이터'로
// 읽힌다. 그래서 여기서는 숫자로 읽히지 않는 값을 0이 아니라 빈칸으로 두고, 그릴 것이 하나도 없으면
// 차트를 포기한다.

// 그리기 상한. 서버는 한 조회에서 1000행(MAX_ROWS)까지 가져오지만 그리는 것은 100행까지다 — 가로 막대는
// 행마다 22px씩 키가 자라(Chart.jsx) 1000행이면 2만 px이고, 세로 막대 1000개는 1px 조각이 된다. 서버가
// `data: step N`으로 채우는 표도 같은 수까지만 싣는다(backend chart.js MAX_CHART_BLOCK_ROWS — 그 위는 그려지지
// 않는 채 답변만 키우고 다른 차트의 몫을 먹는다). 넘는 행은 차트 아래에 '처음 100행만 그렸습니다'로 밝히고,
// 조회된 행 전부는 trace 패널에 있다.
// 시리즈 6개는 범례가 한 줄에 읽히는 한계, 라벨 30자는 축 눈금이 서로를 덮지 않는 한계.
export const MAX_CHART_ROWS = 100;
export const MAX_SERIES = 6;
export const MAX_LABEL_LEN = 30;
export const MAX_TITLE_LEN = 80;
// 범례·툴팁에 적는 이름의 상한 — 열 이름(header)과 조각의 원래 이름(full). 축 눈금의 30자보다
// 넉넉하다: 두 자리 모두 아래로 감기므로 이름 하나가 두 줄이 되어도 그림을 밀어내지 않는다.
// 그래도 상한이 있어야 한다. 이 둘은 조회 결과의 셀·열 이름 그대로라(서버 MAX_CELL_LEN 200자)
// 자유 텍스트 한 문단이 그대로 온다 — 축 눈금(label)만 묶어 두었을 때, 240자짜리 범주 이름 하나가
// 툴팁을 2,513px 상자로 부풀려 1,000px 창의 오른쪽 1,653px 밖으로 나갔다(실측: 창 380px에서는
// 2,173px). 그렇게 나간 글자는 말풍선의 overflow-x: clip에 잘려 어디에서도 읽을 수 없다.
// 잘리지 않은 값은 차트 곁의 '표로 보기'에 늘 그대로 있다.
export const MAX_NAME_LEN = 60;
// 원그래프 조각 수. 그 뒤는 '기타' 한 조각으로 모은다(작은 조각 스무 개는 범례도 색도 읽히지 않는다).
export const MAX_PIE_SLICES = 12;
// 메시지 하나에 그릴 차트 수. 모델이 열 개를 내놓으면 열 개의 ResponsiveContainer가 리사이즈
// 관찰자를 달고 돌아간다 — 그 뒤는 표로 보여준다.
export const MAX_CHARTS_PER_MESSAGE = 4;
// 이력으로 되돌릴 때 남기는 표의 행 수. 서버가 프롬프트에 싣는 결과 행 수(MAX_RESULT_ROWS)와 같다.
const HISTORY_TABLE_ROWS = 20;

const TYPES = new Set(['bar', 'stacked-bar', 'line', 'area', 'pie', 'scatter']);
// 모델이 실제로 쓰는 변형들. 모르는 이름은 막대다 — 표만 남기는 것보다 낫고, 막대는 무엇이든 담는다.
// 평범한 객체가 아니라 Map인 이유: 객체의 [] 조회는 프로토타입까지 올라간다. `type: constructor`
// 한 줄이면 TYPE_ALIASES['constructor']가 Object 함수를 돌려주고, ?? 는 그것을 '아는 이름'으로
// 받아들여 spec.type이 문자열이 아닌 함수가 된다 — '모르는 이름은 막대'라는 이 표의 계약이
// 모델이 쓴 글자 하나로 깨진다. Map은 자기가 담은 것만 안다.
const TYPE_ALIASES = new Map(Object.entries({
  column: 'bar', columns: 'bar', bars: 'bar', histogram: 'bar',
  stacked: 'stacked-bar', stackedbar: 'stacked-bar', 'stacked-column': 'stacked-bar',
  lines: 'line', spline: 'line', trend: 'line',
  areas: 'area', 'stacked-area': 'area',
  donut: 'pie', doughnut: 'pie',
  scatterplot: 'scatter', points: 'scatter', bubble: 'scatter',
}));

// 설정 줄. 콜론 뒤는 값이며 따옴표가 있어도 벗기지 않는다(모델이 따옴표를 쓰면 그것까지 제목이다 —
// 벗기기 시작하면 어디까지 벗길지가 또 하나의 규칙이 된다). `data:` 는 서버가 채우고 지우는 줄이라
// 여기까지 살아오면 서버가 처리하지 못한 것이다 — 값은 무시하되 줄은 설정으로 먹는다.
const CONFIG_RE = /^\s*(type|title|x|y|y2|xtype|data)\s*:\s*(.*?)\s*$/i;
// GFM 표의 구분 줄(|---|:--:|). 있으면 건너뛰고, 없어도 표로 받는다 — 모델이 빼먹는 일이 있고,
// 우리에게 필요한 것은 머리글과 값뿐이다.
const SEP_ROW_RE = /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/;

// 표 한 줄을 칸으로 쪼갠다. GFM처럼 `\|` 는 칸 안의 파이프, `\\` 는 역슬래시 하나다 — 되돌려야
// '표로 보기'(GFM이 그린다)와 차트의 라벨이 같은 글자가 된다(서버 escapeCell이 이렇게 적는다).
// 파이프·역슬래시만이 아니다: 값에 든 강조·코드·링크·취소선·HTML·엔터티 표기도 그대로 두면 GFM이 해석해
// 값이 조용히 바뀌므로 서버가 짝이 있을 때만 막아 보낸다(backend/src/chart.js escapeCell). 그 목록을 여기서도
// 같이 되돌린다 — 한쪽만 늘리면 차트 라벨에 백슬래시가 남는다. 그 밖의 역슬래시는 글자다(`C:\dir`).
// 양끝 파이프는 벗긴다.
const CELL_ESCAPABLE = '\\|`*~[]<_&$';
function splitRow(line) {
  const cells = [];
  let cur = '';
  for (let i = 0; i < line.length; i++) {
    const ch = line[i];
    if (ch === '\\' && CELL_ESCAPABLE.includes(line[i + 1])) { cur += line[i + 1]; i++; continue; }
    if (ch === '|') { cells.push(cur.trim()); cur = ''; continue; }
    cur += ch;
  }
  cells.push(cur.trim());
  if (cells.length && cells[0] === '' && /^\s*\|/.test(line)) cells.shift();
  if (cells.length && cells[cells.length - 1] === '' && /\|\s*$/.test(line)) cells.pop();
  return cells;
}

// 열 이름 비교는 대소문자·양끝 공백을 무시한다 (서버 constants.nameKey와 같은 규칙).
const nameKey = s => String(s ?? '').trim().toLowerCase();
const splitNames = v => String(v ?? '').split(/[,;]/).map(s => s.trim()).filter(Boolean);

// 셀 하나를 숫자로. 조회 결과가 표에 실리는 동안 붙는 것들 — 앞의 통화 기호, 뒤의 %, 세 자리씩 묶는
// 쉼표·공백 — 은 벗긴다. 그 밖의 글자가 남으면 숫자가 아니다('12건'은 12가 아니라 빈칸이다 — 단위를 떼기
// 시작하면 '1.2k'와 '2024-01'을 어디서 멈출지 정할 수 없다). 빈칸과 대시는 결측이다.
// 구분자는 자리가 맞을 때만 벗긴다: 공백·쉼표를 무조건 지우면 '2024 01'이 202401, '1,2'가 12로 읽혀
// 글자 열이 숫자 열로 둔갑한다(실측). '1 000'·'1,000,000'은 묶음이고 '10 20'·'1,2'는 아니다.
const isMissing = s => s === '' || s === '-' || s === '–' || s === '—' || s === 'null' || /^n\/?a$/i.test(s);
const CURRENCY_RE = /^([-+]?)\s*[₩$€¥]\s*/;
// 되참조(\1)로 첫 구분자와 같은 것만 받는다 — 잡아만 두고 쓰지 않으면 '1 234,567'처럼 섞인 표기가
// 통과해 1234567이 된다(실측). 그러면 글자 열이 숫자 열로 둔갑해 없는 값이 그려진다.
const GROUPED_RE = /^[-+]?\d{1,3}(?:([, ])\d{3})(?:\1\d{3})*(?:\.\d*)?$/;
const PLAIN_RE = /^[-+]?(?:\d+\.?\d*|\.\d+)(?:e[-+]?\d+)?$/i;
export function toNumber(cell) {
  let s = String(cell ?? '').trim();
  if (isMissing(s)) return null;
  s = s.replace(CURRENCY_RE, '$1').replace(/\s*%$/, '');
  if (GROUPED_RE.test(s)) s = s.replace(/[, ]/g, '');
  if (!PLAIN_RE.test(s)) return null;
  const n = Number(s);
  return Number.isFinite(n) ? n : null;
}

// 셀 하나를 시각(ms)으로. 구분자가 있는 날짜만 받는다 — 20240101 같은 숫자는 코드일 수도 있어
// xtype: time 을 명시했을 때만 날짜로 읽는다. 시각은 지역 시간으로 만든다: 축의 눈금 글자도 지역
// 시간으로 찍으므로 왕복이 맞아야 '2024-01-01'이 '2023-12-31'로 보이지 않는다.
// Z·+09:00처럼 시간대가 명시되면 그 오프셋을 적용한다. 시계 표시가 같아도 서로 다른 순간일 수 있다.
// 원래 날짜 표기는 rows.full과 표에 남기고, 축에서는 실제 순간을 브라우저 지역 시간으로 표시한다.
const DATE_RE = /^(\d{4})[-./](\d{1,2})(?:[-./](\d{1,2}))?(?:[ T](\d{1,2}):(\d{2})(?::(\d{2})(?:\.(\d+))?)?)?(?:\s*(Z|[-+]\d{2}:?\d{2}))?$/;
const COMPACT_DATE_RE = /^(\d{4})(\d{2})(\d{2})?$/;
export function toTime(cell, explicit = false) {
  const s = String(cell ?? '').trim();
  let m = DATE_RE.exec(s);
  if (!m && explicit) m = COMPACT_DATE_RE.exec(s) || (/^\d{4}$/.test(s) ? [s, s, '1'] : null);
  if (!m) return null;
  const [, y, mo, d, h, mi, sec, fraction = '', zone] = m;
  // 시각도 범위를 넘으면 날짜가 아니다. Date는 12:99를 13:39로 조용히 넘겨 버려(실측), 잘못 적힌
  // 시각이 축의 엉뚱한 자리에 찍힌다 — 아래 날짜 확인은 하루를 넘길 때만 걸린다.
  if (+(h ?? 0) > 23 || +(mi ?? 0) > 59 || +(sec ?? 0) > 59) return null;
  let offset = 0;
  if (zone && zone !== 'Z') {
    const digits = zone.slice(1).replace(':', '');
    const hours = +digits.slice(0, 2);
    const minutes = +digits.slice(2);
    if (hours > 23 || minutes > 59) return null;
    offset = (hours * 60 + minutes) * (zone[0] === '-' ? -1 : 1);
  }
  // 연·월·일을 함께 놓아 0~99년도 그 해의 윤년으로 판정한다.
  const t = new Date(0);
  const mode = zone ? 'UTC' : '';
  t[`set${mode}FullYear`](+y, +mo - 1, +(d ?? 1));
  // Date의 해상도인 밀리초까지 보존한다. 소수 초를 버리면 서로 다른 x가 중복된다.
  t[`set${mode}Hours`](+(h ?? 0), +(mi ?? 0), +(sec ?? 0), +fraction.slice(0, 3).padEnd(3, '0'));
  // 2024-13-45 같은 값은 Date가 조용히 다음 달로 넘겨 버린다 — 넘긴 것은 날짜가 아니었다.
  if (t[`get${mode}Month`]() !== +mo - 1 || t[`get${mode}Date`]() !== +(d ?? 1) || +mo < 1 || +mo > 12) return null;
  // 지역 시간에는 '없는 시각'이 있다 — 서머타임이 시작되는 날에 통째로 빠지는 한 시간(어떤 곳은 30분)이다.
  // Date는 그것도 거절하지 않고 조용히 다음 시각으로 옮긴다: 실측(America/New_York) '2024-03-10 02:30'이
  // 03:30이 되어, 같은 표에 03:30 행이 있으면 '같은 x에 행이 여럿'으로 차트가 통째로 표가 됐고, 그 행
  // 하나뿐일 때는 더 조용했다 — 표와 툴팁에는 02:30인데 축에는 03:30으로 찍힌다. 위 12:99를 막는 것과
  // 같은 부류라 같은 자리에서 막는다: 적힌 것과 다른 자리에 찍느니 읽지 못한 행으로 두는 편이 낫다
  // (그 행 수는 차트 아래에 'x를 시간으로 읽지 못한 N행'으로 밝혀진다 — chartNotes).
  // 조회 결과가 브라우저와 같은 시간대의 값일 이유는 없다: 한국 DB의 평범한 '2024-03-10 02:30' 한 줄이
  // 서머타임을 쓰는 PC에서 이 자리에 걸린다.
  // 시(時)만 보면 안 된다 — 30분씩 옮기는 시간대(Lord Howe)에서는 02:15가 02:45가 되어 시는 그대로다.
  // 시각을 적지 않은 날짜(2024-03-10)에는 걸지 않는다. 자정에 시계를 옮기는 시간대에서는 그날 자정이
  // 없어 하루가 통째로 빠지는데, 날짜만 적힌 값에서 하루 안의 어느 순간인가는 뜻이 없다(축의 라벨도
  // 정렬도 그대로다). 시간대를 명시한 값(mode가 UTC)은 지역 시간을 거치지 않아 늘 이 검사를 지난다.
  if (h !== undefined && (t[`get${mode}Hours`]() !== +h || t[`get${mode}Minutes`]() !== +mi)) return null;
  return t.getTime() - offset * 60_000;
}

// 글자를 n자까지 자른다. 경계에서 서로게이트 쌍(이모지 등)을 반으로 쪼개지 않는다 — 짝 잃은
// 코드유닛은 화면에서 U+FFFD가 되고, 이력으로 나가면 프롬프트 인코딩에서 같은 일이 일어난다.
// 자르는 곳이 둘이라(라벨·범례는 아래 clip, 이력은 App.jsx clipTurn) 경계 규칙만 여기 한 번 적는다.
export const sliceSafe = (s, n) => {
  if (n <= 0) return '';
  const cut = s.slice(0, n);
  const last = cut.charCodeAt(cut.length - 1);
  return last >= 0xd800 && last <= 0xdbff ? cut.slice(0, -1) : cut;
};
// 넘치면 끝을 …로 대신한다 (그 한 글자 자리를 비워 둔다).
export const clip = (s, n) => {
  if (s.length <= n) return s;
  // 자리가 한 글자뿐이면 …를 붙일 자리도 없다 — 붙이면 상한을 넘긴다.
  return n <= 1 ? sliceSafe(s, n) : `${sliceSafe(s, n - 1)}…`;
};
// 글자를 주어진 폭(px) 안으로 줄인다. 넘치는 만큼은 clip과 같이 …로 대신한다.
// 폭을 재는 일은 부르는 쪽(글꼴을 아는 자리, Chart.jsx textMeasurer)이 하고 여기서는 '몇 자까지
// 남길 것인가'만 정한다 — 순수 함수라 회귀 테스트가 붙는다(pieLabelsOverflow와 같은 가름이다).
// 자를 자리를 이분법으로 찾는 이유: 눈금 열다섯 개 × 서른 자를 한 글자씩 재면 창을 끌 때마다
// 수백 번의 측정이 된다.
export function fitText(s, room, measure) {
  if (!(room > 0)) return '';
  if (measure(s) <= room) return s;
  let lo = 0;          // clip(s, lo)는 들어간다 (0자는 빈 글자다)
  let hi = s.length;   // clip(s, hi)는 넘친다 (바로 위에서 확인했다)
  while (hi - lo > 1) {
    const mid = (lo + hi) >> 1;
    if (measure(clip(s, mid)) <= room) lo = mid; else hi = mid;
  }
  return clip(s, lo);
}

const EMPTY_LABEL = '(빈값)';

// 블록 본문(펜스 안의 글자)을 설정과 표 줄로 가른다. 설정은 표가 시작되기 전까지만 읽는다 —
// 표 뒤에 'type: …' 같은 줄이 오면 그것은 모델이 표 아래 붙인 설명이지 설정이 아니다.
// 줄 끝은 markdown과 같은 규칙으로 본다 — 홀로 선 \r도 줄 끝이다(펜스 안의 글자는 원문 그대로 오므로
// 여기서 가르지 않으면 두 줄이 한 줄로 붙고, 표만 떼어 다시 렌더할 때는 markdown이 거기서 줄을 가른다).
export function splitBlock(text) {
  const config = {};
  const table = [];
  let inTable = false;
  for (const raw of String(text ?? '').split(/\r\n?|\n/)) {
    if (!raw.trim()) continue;
    if (!inTable) {
      const m = CONFIG_RE.exec(raw);
      if (m) { config[m[1].toLowerCase()] = m[2]; continue; }
    }
    if (raw.includes('|')) { inTable = true; table.push(raw); }
    // 파이프도 설정도 아닌 줄(설명 문장 등)은 버린다 — 표를 깨뜨리지 않는 것이 우선이다.
  }
  return { config, table };
}

// 표 줄들을 머리글과 값 행으로. 머리글이 두 칸 미만이면 표가 아니다. 구분 줄은 GFM처럼 둘째 줄에서만
// 찾는다 — 그 아래의 `| - | - |`는 값이 전부 결측인 행이지 구분 줄이 아니다.
export function parseTable(lines) {
  if (!lines.length) return null;
  const header = splitRow(lines[0]);
  if (header.length < 2) return null;
  const body = lines.slice(1).filter((l, i) => !(i === 0 && SEP_ROW_RE.test(l))).map(splitRow);
  return { header, rows: body };
}

const normalizeType = t => {
  const k = nameKey(t).replace(/[\s_]+/g, '-');
  return TYPES.has(k) ? k : (TYPE_ALIASES.get(k) ?? 'bar');
};

// 열 이름 목록을 열 번호로. 없는 이름은 버린다(있는 것만으로 그린다 — 하나가 틀렸다고 전부 표로
// 돌아가면, 모델이 열 이름의 대소문자 하나 틀린 값으로 차트 전체를 잃는다).
const columnIndex = (name, header) => {
  const exact = header.indexOf(name);
  return exact >= 0 ? exact : header.findIndex(h => nameKey(h) === nameKey(name));
};
const resolveColumns = (names, header, exclude) =>
  names.map(n => columnIndex(n, header))
    .filter((i, at, arr) => i >= 0 && !exclude.has(i) && arr.indexOf(i) === at);

// 열 하나가 '숫자 열'인가: 값이 있는 칸이 하나 이상이고 그 전부가 숫자로 읽혀야 한다(결측 표시는
// 값이 아니다). 숫자와 글자가 섞인 열(상태 코드 등)은 그리지 않는다.
const isNumericColumn = (rows, i) => {
  let seen = false;
  for (const r of rows) {
    const c = String(r[i] ?? '').trim();
    if (isMissing(c)) continue;
    if (toNumber(c) === null) return false;
    seen = true;
  }
  return seen;
};

// 블록 본문 → 그릴 수 있는 명세. 그릴 수 없으면 { ok:false, reason } — reason은 개발자용 문자열이며
// 화면에는 표만 보인다.
// block: 부르는 쪽이 이미 splitBlock한 것이 있으면 그것을 준다 — 화면은 같은 블록을 표로도 그리므로
// 두 번 훑을 이유가 없다 (없으면 여기서 가른다).
export function parseChartBlock(text, block) {
  const { config, table } = block ?? splitBlock(text);
  if (config.data !== undefined && !table.length) return { ok: false, reason: 'data 참조가 채워지지 않음' };
  const parsed = parseTable(table);
  if (!parsed) return { ok: false, reason: '표 없음' };
  const { header, rows } = parsed;
  if (!rows.length) return { ok: false, reason: '행 없음' };

  const type = normalizeType(config.type);
  const title = clip(String(config.title ?? '').trim(), MAX_TITLE_LEN);

  // x 열: 지정된 이름이 있으면 그 열, 없거나 못 찾으면 첫 열.
  let xi = config.x ? columnIndex(config.x, header) : 0;
  if (xi < 0) xi = 0;

  // 시리즈: y가 있으면 그 열들(숫자 열만), 없거나 하나도 못 찾으면 x 밖의 숫자 열 전부. y2는 오른쪽 축.
  // y2로 적힌 열은 그리지 못하는 것(숫자 아님·셋째 이후)까지 전부 '쓴 열'로 표시한다 — 그러지 않으면
  // y가 비었을 때의 채움(아래)이 그 열을 왼쪽 축에 그린다(오른쪽 축에 두라던 열이 왼쪽에 서는 조용한 오답).
  // 이름이 표에 있는데 그 열을 하나도 그릴 수 없으면(전부 결측이거나 글자) 차트를 포기한다 — 다른 숫자 열로
  // 바꿔 그리면 제목은 '매출'인데 그래프는 건수다(실측). 채움은 이름이 표에 없을 때(오타)만이다.
  const used = new Set([xi]);
  const y2Named = config.y2 ? resolveColumns(splitNames(config.y2), header, used) : [];
  y2Named.forEach(i => used.add(i));
  const y2Numeric = y2Named.filter(i => isNumericColumn(rows, i));
  const yNamed = config.y ? resolveColumns(splitNames(config.y), header, used) : [];
  let y = yNamed.filter(i => isNumericColumn(rows, i));
  // pie·scatter는 값 하나만 그리므로 y2는 애초에 그리지 않는다 — 그 설정이 숫자 열이 아니라고
  // 그릴 수 있는 그래프까지 포기하면, 쓰이지도 않는 줄 하나 때문에 표만 남는다.
  const single = type === 'pie' || type === 'scatter';
  if ((yNamed.length && !y.length) || (!single && y2Named.length && !y2Numeric.length)) return { ok: false, reason: '지정한 열이 숫자 열이 아님' };
  const y2 = single ? [] : y2Numeric.slice(0, 2);
  if (!y.length) y = header.map((_, i) => i).filter(i => !used.has(i) && isNumericColumn(rows, i));
  // 왼쪽에 그릴 것이 없고 오른쪽만 있으면(y2만 적은 블록) 그것을 왼쪽에 그린다 — 축 하나면 오른쪽일
  // 이유가 없다. pie·scatter에서는 y2를 비워 두므로(위 single) 그 이름들을 여기서 되찾아야 한다 —
  // 그러지 않으면 'y2: 건수'라고만 적은 원그래프가 그릴 열을 하나도 찾지 못한다(실측).
  if (!y.length && (y2.length || y2Numeric.length)) y = y2.length ? y2.splice(0) : y2Numeric.slice(0, 1);
  if (!y.length) return { ok: false, reason: '숫자 열 없음' };
  // pie·scatter는 값 하나만 그린다 — 둘째 시리즈부터는 겹쳐 그릴 자리가 없다.
  y = y.slice(0, single ? 1 : MAX_SERIES - y2.length);

  // x의 종류. 명시(xtype)가 우선이고, 없으면 시간·숫자 축이 뜻이 있는 그래프에서만 추론한다.
  // 막대·원은 언제나 범주다 — 날짜 라벨의 막대는 범주로 그려도 옳고, 숫자 축으로 그리면 막대 폭이
  // 날짜 간격에 따라 제각각이 된다. 다만 xtype: time 이 명시된 막대는 시간순으로 줄은 세운다.
  const explicit = nameKey(config.xtype);
  const continuous = type === 'line' || type === 'area' || type === 'scatter';
  let xKind = 'category';
  if (explicit === 'time' || explicit === 'number' || explicit === 'category') xKind = explicit;
  else if (continuous) {
    if (rows.every(r => toTime(r[xi]) !== null)) xKind = 'time';
    else if (type === 'scatter' && rows.every(r => toNumber(r[xi]) !== null)) xKind = 'number';
  }
  const sortByTime = xKind === 'time';
  if (!continuous) xKind = 'category';
  // 산점도는 x가 수치여야 한다 — 범주 x의 점들은 그냥 세로줄이다.
  if (type === 'scatter' && xKind === 'category') return { ok: false, reason: '산점도의 x가 수치가 아님' };

  // 이름은 여기서 한 번만 묶는다(MAX_NAME_LEN) — 그리는 쪽이 자리마다 다시 자르면 범례와 툴팁이
  // 다른 글자를 쓰게 되고, 툴팁 항목의 순서를 이름으로 맞추는 쪽(Chart.jsx byColumn)이 짝을 잃는다.
  const series = [
    ...y.map(i => ({ name: clip(header[i], MAX_NAME_LEN), col: i, axis: 'left' })),
    ...(single ? [] : y2.map(i => ({ name: clip(header[i], MAX_NAME_LEN), col: i, axis: 'right' }))),
  ];

  const data = [];
  // 시간·숫자 축인데 x를 읽지 못한 행 수. 추론한 축에서는 0이고(전부 읽혀야 추론한다) 명시한 xtype에서만
  // 생긴다 — 조용히 빠지면 '합계' 행이나 '2024-Q1' 라벨이 없어진 것을 알 길이 없어 차트 아래에 밝힌다.
  let skipped = 0;
  // 원그래프가 그리지 못해 뺀 행 수(값이 0 이하) — 아래 dropped 참고.
  let dropped = 0;
  for (const r of rows) {
    let label = String(r[xi] ?? '').trim();
    const t = sortByTime ? toTime(label, explicit === 'time') : null;
    let x = label;
    if (xKind === 'time') { if (t === null) { skipped++; continue; } x = t; }
    else if (xKind === 'number') { x = toNumber(label); if (x === null) { skipped++; continue; } }
    // 범주가 비어 있는 행(GROUP BY의 NULL 그룹 등)도 그린다 — 표에는 있는 행이 그래프에서만 빠지면 합계가 어긋나 보인다.
    else if (!label) label = x = EMPTY_LABEL;
    const values = series.map(s => toNumber(r[s.col]));
    // 원그래프에서 값이 없거나 음수인 조각은 그릴 수 없다(Recharts는 음수 조각을 0으로 뭉갠다).
    // 다른 그래프에서는 값이 전부 빈 행도 남긴다 — 그 범주가 축에 있어야 '값이 없다'가 보인다.
    // 뺀 행은 세어 둔다: 표에는 있는 행이 그림에서만 없어지면 비율의 분모가 달라진 것을 알 길이 없다
    // (x를 읽지 못해 뺀 행을 skipped로 밝히는 것과 같은 이유다 — 조용히 빠지는 행이 있어서는 안 된다).
    if (type === 'pie' && !(values[0] > 0)) { dropped++; continue; }
    // full은 '축 눈금보다는 긴 이름'이지 '자르지 않은 이름'이 아니다 — 범례와 툴팁이 이것을 쓰고,
    // 그 둘도 화면 안에 들어가야 한다(MAX_NAME_LEN). 온전한 값은 '표로 보기'에 있다.
    data.push({ x, label: clip(label, MAX_LABEL_LEN), full: clip(label, MAX_NAME_LEN), values, t });
  }
  if (!data.length) return { ok: false, reason: '그릴 행 없음' };
  // 선·영역은 x 하나에 값이 하나여야 한다. 피벗되지 않은 결과(일자×상태×건수)를 `x: 일자`로 그리면 같은
  // 시각에 점이 여럿 서서 선이 수직으로 오르내리고, 그것이 추세로 읽힌다(실측) — 그리지 않고 표를 보인다.
  // 산점도는 같은 x의 점 여럿이 정상이고, 범주 축의 막대는 행마다 자기 자리가 있다.
  if (continuous && type !== 'scatter' && new Set(data.map(d => d.x)).size < data.length) return { ok: false, reason: '같은 x에 행이 여럿' };
  // 시간·숫자 축은 정렬돼 있어야 선이 되돌아가지 않는다. 시간순 막대는 날짜를 전부 읽었을 때만 세운다.
  if (xKind === 'number' || xKind === 'time') data.sort((a, b) => a.x - b.x);
  else if (sortByTime && data.every(d => d.t !== null)) data.sort((a, b) => a.t - b.t);
  for (const d of data) delete d.t;

  const clipped = data.length > MAX_CHART_ROWS;
  return {
    ok: true,
    spec: {
      type, title, xKind,
      xName: clip(header[xi], MAX_NAME_LEN),
      series: series.map(({ name, axis }) => ({ name, axis })),
      rows: clipped ? data.slice(0, MAX_CHART_ROWS) : data,
      clipped,
      total: data.length,
      skipped,
      dropped,
    },
  };
}

// 원그래프의 조각. MAX_PIE_SLICES를 넘으면 '값이 큰' 것들을 남기고 나머지를 '기타' 하나로 모은다 — 표 순서의
// 꼬리를 모으면 `data: step N`(쿼리 정렬 그대로, 이름순일 때가 많다)에서 큰 조각이 기타에 묻히고 1짜리가
// 조각으로 남는다(실측). 남긴 조각은 표 순서를 지키고 기타는 맨 뒤다. Chart.jsx가 아니라 여기 있는 이유는
// 순수 함수라 테스트가 붙기 때문이다.
export function pieSlices(rows, max = MAX_PIE_SLICES) {
  const data = rows.map(r => ({ name: r.label, full: r.full, value: r.values[0] }));
  if (data.length <= max) return data;
  const keep = new Set(data.map((d, i) => i).sort((a, b) => data[b].value - data[a].value).slice(0, max - 1));
  const rest = data.filter((_, i) => !keep.has(i));
  // full은 '잘리지 않은 이름'이고 범례·툴팁이 그것을 쓴다 — 모아 놓은 조각에서는 개수까지가 이름이다.
  const other = `기타 (${rest.length})`;
  return [...data.filter((_, i) => keep.has(i)), { name: other, full: other, value: rest.reduce((a, d) => a + d.value, 0) }];
}

// 개별 값이 유한해도 합계는 넘칠 수 있다. 그룹화와 Recharts의 비율 계산보다 먼저
// 같은 배율로 줄인다. 보통 값은 그대로 두고, 툴팁은 scale로 원래 단위를 복원한다.
export function pieData(rows) {
  const total = rows.reduce((sum, row) => sum + row.values[0], 0);
  const scale = Number.isFinite(total) ? 1 : Math.max(...rows.map(row => row.values[0]));
  return {
    data: pieSlices(scale === 1 ? rows : rows.map(row => ({ ...row, values: [row.values[0] / scale] }))),
    scale,
  };
}

export function fmtScaledNum(value, scale) {
  const product = value * scale;
  if (Number.isFinite(product)) return fmtNum(product);
  // '기타' 자체가 Number 범위를 넘을 때도 Infinity 대신 원래 크기를 지수로 적는다.
  const exponent = Math.floor(Math.log10(scale));
  const [mantissa, extra] = (value * (scale / 10 ** exponent)).toExponential(2).split('e');
  return `${mantissa}e+${exponent + Number(extra)}`;
}

// 원그래프의 바깥 라벨(이름과 비율을 조각 곁에 적는 것)이 그림 상자를 넘는가. 넘으면 그리는 쪽(Chart.jsx
// PieView)은 비율만 조각 안에 적고 이름은 상자 아래 범례로 내린다 — SVG 밖으로 나간 글자는 소리 없이
// 잘리기 때문이다. 상자 폭 하나로 가르던 때에는(380px 아래에서만 안으로) 데스크톱 폭에서도 스무 자
// 이름이 양끝에서 잘렸다(실측: 폭 574px 상자에서 왼쪽 24px·오른쪽 3px). 라벨의 길이는 조회 결과의 셀
// 값이라(30자까지, MAX_LABEL_LEN) 폭이 아니라 '이 글자들이 이 자리에 들어가는가'로 정해야 한다.
// 자리는 Recharts가 정하는 그대로 따라 센다(recharts polar/Pie): 조각의 가운데 각도에서 반지름에
// 20px(offsetRadius)을 더한 점에 글자가 서고, 그 점이 중심의 오른쪽이면 오른쪽으로, 왼쪽이면 왼쪽으로
// 뻗는다(textAnchor). 각도는 0°가 3시 방향이고 반시계로 돈다. 조각은 값의 비율만큼이고 사이 틈은 없다.
// 반지름은 여백을 뺀 상자의 짧은 변의 절반에 비율(radiusRatio, Recharts의 outerRadius '72%')을 곱한 것.
// 글자의 폭(widths)은 부르는 쪽이 재어 준다 — 순수 함수로 두어 회귀 테스트가 붙게 하려는 것이고,
// 실제 폭은 폰트에 달려 있어 여기서 알 수 없다.
export function pieLabelsOverflow({ width, height, margin, radiusRatio, values, widths, offsetRadius = 20 }) {
  if (!(width > 0) || !(height > 0)) return false;
  const inner = Math.min(width - 2 * margin, height - 2 * margin);
  const reach = radiusRatio * (inner / 2) + offsetRadius;
  const cx = width / 2;
  const total = values.reduce((a, v) => a + (v > 0 ? v : 0), 0);
  let angle = 0;
  return values.some((v, i) => {
    const delta = total > 0 && v > 0 ? (v / total) * 360 : 0;
    const mid = angle + delta / 2;
    angle += delta;
    const x = cx + Math.cos((-mid * Math.PI) / 180) * reach;
    // 오른쪽으로 뻗으면 상자의 오른쪽 끝까지, 왼쪽이면 왼쪽 끝까지가 자리다. 정확히 중심(12시·6시)이면
    // Recharts는 가운데 정렬로 그리는데, 그 판정은 부동소수점의 마지막 자리에 달려 있어 여기서 같은 답을
    // 낸다고 장담할 수 없다 — 그래서 중심에서는 양쪽 중 좁은 쪽을 자리로 친다(어느 쪽으로 그려지든
    // 들어간다). 그만큼은 넉넉히 보는 셈이고, 그 손해는 상자의 반보다 넓은 라벨 하나가 안으로 가는 것뿐이다.
    const room = x > cx ? width - x : x < cx ? x : Math.min(x, width - x);
    return widths[i] > room;
  });
}

// 차트에 적는 숫자(축 눈금·툴팁). 값이 없는 칸(null)은 빈 글자다 — 그리는 쪽은 결측을 0으로 그리지 않는다.
// 소수 두 자리로 자르면 0.0012 같은 값이 '0'으로 나온다. 축 눈금이 모두 '0'이 되고, 막대에 손을 얹은
// 사람은 값이 0이라는 답을 듣는다 — 값이 있는데 없다고 말하는 셈이라, 이 파일이 처음부터 막으려던
// 조용한 오답('숫자로 읽히지 않는 값을 0으로 그린다')이 표기 쪽 문으로 되돌아온 것이다(실측: 비율
// 열 0.0012·0.0034가 축도 툴팁도 전부 '0'이었다). 그래서 두 자리로 담을 수 없는 값에서는 자릿수가
// 아니라 유효숫자로 센다. 0.01 이상은 지금까지의 표기 그대로다(천 단위 쉼표, 소수 두 자리).
// 아주 작은 값은 지수로 적는다 — 유효숫자로만 세면 모델이 쓴 표의 1e-300 한 칸이 눈금 하나를
// 수백 자로 만든다(라벨을 clip으로 묶어 두는 것과 같은 이유다).
// Chart.jsx가 아니라 여기 있는 이유는 아래 chartNotes와 같다: 순수 함수라 회귀 테스트가 붙는다.
// 표기 결함은 오류를 남기지 않아 테스트가 유일한 방어선인데, 그 함수가 JSX 안에 살면 방어선을 세울 수 없다.
// 아주 큰 값도 지수로 적는다 — 아래쪽(1e-6)과 같은 이유이고, 이쪽이 손해가 더 크다. 자릿수 표기는
// 값이 커질수록 길이가 끝없이 자라는데(1e308 한 칸이 쉼표까지 410자다) 그 글자가 서는 자리는 값 축의
// 눈금이고, 그 축은 width="auto"라 눈금이 넓은 만큼 그림에서 폭을 가져간다(Chart.jsx). 그래서 큰 값
// 하나가 그래프를 밀어내다 아예 없앤다 — 실측(창 1000px, 그림 상자 574px): 1e21 눈금 169px, 1e60
// 474px, 1e75에서는 막대도 눈금선도 하나 없이 눈금 글자만 상자 밖으로 나갔다(그러고는 말풍선의
// overflow-x: clip에 잘려 읽지도 못한다). 아무 오류도 나지 않아 사용자에게는 그냥 차트가 없는 답변이다.
// 경계를 1e21에 두는 이유: 자바스크립트 자신이 수를 글자로 옮길 때 지수로 넘어가는 자리가 거기고
// (String(1e21) === '1e+21'), 그 아래는 정수부가 21자리를 넘지 않아 표기가 31자 안에서 묶인다 —
// 조·경 단위의 실제 조회 값은 지금까지와 글자 하나 다르지 않다.
export function fmtNum(v) {
  if (typeof v !== 'number' || !Number.isFinite(v)) return '';
  const a = Math.abs(v);
  if (a >= 1e21) return v.toExponential(2);
  if (a === 0 || a >= 0.01) return v.toLocaleString('ko-KR', { maximumFractionDigits: 2 });
  return a >= 1e-6 ? v.toLocaleString('ko-KR', { maximumSignificantDigits: 3 }) : v.toExponential(2);
}

// 차트 아래에 붙이는 안내 문구 — '표에는 있는데 그림에서 빠진 행'을 사용자가 알 수 있는 유일한
// 단서다. 그리는 쪽(Chart.jsx)이 아니라 여기 있는 이유는 trace.js의 건수 문구와 같다: 순수 함수라
// node:test로 회귀 테스트가 붙는다. 문구 결함은 오류를 남기지 않아 테스트가 유일한 방어선인데,
// 문자열이 JSX 안에 살면 그 방어선을 세울 수 없다 — 실제로 skipped 문구가 조사를 붙인 채
// ('숫자' + '으로' = '숫자으로') 화면에 나가고 있었고, 회귀 테스트가 없어 아무도 보지 못했다.
//   clipped — 행 상한(MAX_CHART_ROWS)에 걸려 앞부분만 그렸다. 그래프만 보면 그것이 전부로 읽힌다.
//   skipped — 명시한 xtype으로 읽지 못해 뺀 행('합계' 행, 다른 꼴의 날짜). 조용히 빠지면 그 행이
//             없어진 것을 알 길이 없다.
//   dropped — 원그래프가 조각으로 만들 수 없어 뺀 행(값이 없거나 0 이하). 표에는 있는 행이라
//             밝히지 않으면 비율의 분모가 달라진 것을 사용자가 알 수 없다.
export function chartNotes(spec) {
  const notes = [];
  if (spec.clipped) notes.push(`처음 ${spec.rows.length}행만 그렸습니다 (전체 ${spec.total}행).`);
  if (spec.skipped > 0) notes.push(`x를 ${spec.xKind === 'time' ? '시간으로' : '숫자로'} 읽지 못한 ${spec.skipped}행은 그리지 않았습니다.`);
  if (spec.dropped > 0) notes.push(`값이 없거나 0 이하인 ${spec.dropped}행은 조각으로 그리지 않았습니다.`);
  return notes;
}

// 블록 안의 표만 markdown으로. 차트를 그리지 못할 때·그리기 전에·'표로 보기'에 그대로 렌더한다.
// 구분 줄이 빠졌거나 칸 수가 머리글과 다른 표는 GFM이 표로 인정하지 않는다(파이프 글자가 문단으로
// 그대로 보인다) — 머리글 칸 수에 맞춰 넣어 준다. 위 splitBlock·parseTable은 어느 쪽이든 읽으므로
// 차트는 그려지는데 '표로 보기'만 깨지는 일이 없어야 한다.
// 표 줄들을 GFM이 표로 인정하는 모양으로: 머리글 · 구분 줄 · 값 줄들. 구분 줄이 없거나 칸 수가
// 머리글과 다르면 채워 넣는다 — 모델이 빼먹는 일이 있다. 화면과 이력이 같은 함수를 쓰는 이유는,
// 한쪽만 고쳐 두면 화면에는 표인 것이 모델에게는 파이프 글자 묶음으로 가기 때문이다(실측).
// 읽는 규칙(SEP_ROW_RE)과 내보내는 규칙(아래 GFM_SEP_RE)은 일부러 다르다. 읽을 때는 넓게 봐야 한다 —
// 구분 줄로 쓰인 것이 분명한 줄은 값 행으로 세지 않아야 차트에 `---` 행이 생기지 않는다. 내보낼 때는
// 좁게 봐야 한다 — 그대로 내보낸 줄을 GFM이 구분 줄로 읽지 못하면 표가 통째로 파이프 글자가 된다.
// 좁은 쪽이 요구하는 둘: ① 공백은 스페이스·탭뿐이다(micromark와 같은 규칙 — 같은 파일 OPEN_LINE_RE·
// maskLiteralFences가 `[ \t]`만 보는 것과 같은 이유다. `\s`는 NBSP·전각공백·얇은공백·VT·FF까지 공백으로
// 세는데 micromark는 아니다) ② 줄이 파이프로 시작한다.
const GFM_SEP_RE = /^[ \t]*\|[ \t]*:?-+:?[ \t]*(\|[ \t]*:?-+:?[ \t]*)*\|?[ \t]*\r?$/;
// 줄 앞에 파이프가 없으면 넣는다(들여쓰기는 그대로). 넣는 이유는 그 첫 글자다: 파이프 없이 시작하는
// 줄은 markdown이 표보다 먼저 다른 블록으로 읽는다 — `- | -` 구분 줄은 목록 항목이 되고(실측: '표로
// 보기'가 표 대신 글머리표 하나와 파이프 글자 묶음이 됐다), 머리글의 첫 칸이 `#`이면 제목이, `*`·`+`이면
// 목록이 되어 그 표는 화면에서도 이력에서도 표가 아니다. 파이프 하나가 그 모든 갈래를 닫는다.
// 넣을지 말지는 splitRow가 '앞의 파이프'로 보는 것과 같은 규칙(`\s*\|`)으로 가른다 — 여기만 좁게 보면
// 전각공백으로 들여쓴 줄에 파이프가 하나 더 붙어 칸이 하나 늘고(splitRow는 그 공백을 앞의 빈 칸으로
// 세지 않는다) 머리글과 구분 줄의 칸 수가 어긋난다. 그러면 표를 세우려던 것이 표를 깨뜨린다.
const withPipe = line => (/^\s*\|/.test(line) ? line : line.replace(/^([ \t]*)/, '$1|'));
export function normalizeTable(table) {
  if (!table.length) return null;
  const cols = splitRow(table[0]).length;
  const isSep = table.length > 1 && SEP_ROW_RE.test(table[1]);
  // 채워 넣는 구분 줄은 머리글과 같은 들여쓰기로 — 이력에서는 원문의 줄 모양을 그대로 두기 때문에
  // (목록 안의 표는 항목 폭만큼 들여 온다) 이 줄만 왼쪽 끝에 붙으면 그 표가 목록에서 떨어져 나간다.
  const indent = /^\s*/.exec(table[0])[0];
  // 원문의 구분 줄은 GFM이 그대로 읽어 줄 때만 그대로 둔다(정렬 표시가 살아 있게). 아니면 새로 만든다.
  const keep = isSep && GFM_SEP_RE.test(table[1]) && splitRow(table[1]).length === cols;
  const sep = keep ? table[1] : `${indent}|${' --- |'.repeat(cols)}`;
  return { header: withPipe(table[0]), sep, rows: table.slice(isSep ? 2 : 1).map(withPipe) };
}

// 이미 가른 블록에서 바로. 화면은 한 블록을 두 번(제목 있는 표·'표로 보기') 그리므로, 부르는 쪽이
// splitBlock을 한 번만 하고 그 결과를 돌려쓸 수 있어야 한다.
export function chartTableMarkdownFrom(block) {
  // 줄 앞의 공백은 벗긴다 — 펜스 안에서는 뜻이 없던 들여쓰기가 표만 떼어 렌더하면 4칸부터 코드블록이다.
  const t = normalizeTable(block.table.map(l => l.trim()));
  return t ? [t.header, t.sep, ...t.rows].join('\n') : '';
}
export const chartTableMarkdown = text => chartTableMarkdownFrom(splitBlock(text));

// 답변 본문에서 ```chart 펜스를 찾는다. markdown 파서(react-markdown)가 펜스로 읽는 것과 같은 범위를
// 잡아야 한다 — 이쪽이 놓친 블록은 화면에서는 차트인데 이력에서는 설정 줄째 남고, 서버(backend chart.js)의
// 같은 정규식이 놓치면 `data:` 참조가 채워지지 않은 채 화면에 온다. 그래서 들여쓰기는 칸 수를 따지지 않고
// (목록 안의 펜스는 항목 번호 폭만큼 들여 온다 — `10. `이면 4칸), 언어 뒤의 덧말(```chart 월별)과
// 3개 넘는 백틱으로 닫는 펜스, CRLF도 받는다. 닫는 펜스가 없는 블록(토큰 한도로 잘린 응답)은 잡히지 않고 그대로 남는다.
// 펜스 글자도 markdown이 받는 대로 — 백틱이든 물결(~~~chart)이든 셋 이상 몇 개든 받는다. 백틱 셋만 받던 때에는
// ~~~chart·````chart 블록이 화면에서는 차트인데 이력에는 설정 줄째 그대로 실려 갔다(실측). 닫는 펜스는 markdown과
// 같이 여는 펜스(\2)에 그 글자(\3)가 더 붙은 것까지다 — ````chart 안의 ``` 줄은 끝이 아니라 내용이고, ```~처럼
// 글자가 섞인 줄도 끝이 아니다. 그룹 1이 여는 펜스의 들여쓰기, 2가 펜스, 3이 펜스 글자, 본문은 그룹 4다
// (서버 backend chart.js FENCE_RE와 같은 그룹 배치다 — 들여쓰기는 아래 chartBlocksToTables가 쓴다).
// 본문은 없을 수도 있다(```chart 바로 아래 ```) — 그것도 markdown에게는 닫힌 블록이다. 본문 한 줄을 요구하던
// 때에는 빈 블록의 여는 펜스가 다음 차트 블록의 닫는 펜스와 짝이 되어 그 사이의 문장까지 한 블록으로 삼켰다
// (실측: 빈 블록 뒤의 설명 문장이 이력에서 사라졌다). 빈 본문을 먼저 시도해야(??) 그 짝짓기가 생기지 않는다.
export const CHART_FENCE_RE = /^([ \t]*)((`|~)\3{2,})[ \t]*chart(?:[ \t]+[^\r\n]*)?\r?\n(?:([\s\S]*?)\r?\n)??[ \t]*\2\3*[ \t]*\r?$/gim;

// 위 정규식을 줄 단위로 다시 쓴 것 — 답변 하나를 훑는 비용이 길이에 비례하게. 정규식은 여는 펜스마다 닫는 펜스를
// 끝까지 찾아 닫히지 않은 펜스가 많으면 '여는 줄 수 × 길이'가 된다(실측: 답변 상한 안의 '```chart' 7,500줄에 487ms,
// 길이를 두 배로 하면 네 배). 이력 변환(chartBlocksToTables)은 그 답변이 최근 여섯 턴에 남아 있는 동안 매 전송마다
// 다시 도므로, 퇴화한 답변 하나가 그 뒤 세 번의 질문을 저마다 반초씩 멈추게 했다 — preview.js가 같은 이유로
// 줄 단위 상태 기계가 된 것과 같은 결이다. 찾는 규칙은 정규식과 정확히 같아야 한다(회귀 테스트가 둘을 대조한다):
//   여는 줄: 들여쓰기·같은 글자 셋 이상의 펜스·chart(대소문자 무관)·덧말, 그리고 그 뒤에 줄바꿈이 있어야 한다.
//   닫는 줄: 여는 펜스와 같은 글자가 그 수 이상, 앞뒤 공백뿐. 여는 줄 다음부터 처음 나오는 그런 줄이다.
//   본문: 그 사이의 줄들. 닫는 줄 바로 앞의 \r 하나는 정규식의 \r?\n이 그렇듯 본문에 넣지 않는다.
//   닫히지 않는 여는 줄은 블록이 아니고, 그다음 줄부터 다시 찾는다. 찾은 블록의 닫는 줄 다음부터 다시 찾는다.
// 비용이 비례하는 이유: 같은 글자·같은 길이 이상의 닫는 펜스가 어느 줄부터 끝까지 없다는 것을 한 번 알면(dead),
// 그 뒤의 같은 모양 여는 줄은 다시 훑지 않는다 — 퇴화한 답변은 같은 여는 줄의 반복이라 한 번만 끝까지 간다.
const OPEN_LINE_RE = /^([ \t]*)((`|~)\3{2,})[ \t]*chart(?:[ \t]+[^\r\n]*)?\r?$/i;
const CLOSE_LINE_RE = /^[ \t]*((`|~)\2{2,})[ \t]*\r?$/;
export function chartFences(md) {
  const lines = String(md ?? '').split('\n');
  const blocks = [];
  const dead = { '`': Infinity, '~': Infinity };
  for (let i = 0; i < lines.length - 1; i++) {
    const m = OPEN_LINE_RE.exec(lines[i]);
    if (!m) continue;
    const [, indent, fence, ch] = m;
    if (fence.length >= dead[ch]) continue;
    let j = i + 1;
    for (; j < lines.length; j++) {
      const c = CLOSE_LINE_RE.exec(lines[j]);
      if (c && c[2] === ch && c[1].length >= fence.length) break;
    }
    if (j === lines.length) { dead[ch] = Math.min(dead[ch], fence.length); continue; }
    const body = j === i + 1 ? undefined : lines.slice(i + 1, j).join('\n').replace(/\r$/, '');
    blocks.push({ start: i, end: j, indent, fence, ch, body });
    i = j;
  }
  return { lines, blocks };
}

// 펜스 탐색 전에 일반 코드블록을 가린다. 그 안에 적힌 ```chart는 문법 예시이며
// 화면에서도 코드 그대로다. 줄 수를 유지해 변환하지 않은 원문은 그대로 이어 붙인다.
// 공백으로 치는 글자는 markdown과 같이 스페이스·탭뿐이다. `.trim()`·`\s`는 NBSP·전각공백·얇은공백·
// VT·FF까지 공백으로 세는데, micromark(=화면)는 그것을 공백으로 보지 않는다 — 그 어긋남이 그대로
// '화면과 이력이 다른 글을 말한다'가 된다(실측, 여는 꼬리·닫는 꼬리 10종 전수):
//   닫는 줄 ```+NBSP — 화면은 리터럴 블록이 안 닫혀 뒤까지 통째로 코드인데, 여기서는 닫힌 것으로 보아
//     뒤를 가리지 않는다. 그러면 그 안의 ```chart가 이력에서만 표로 바뀌어, 사용자가 코드로 본 글이
//     모델에게는 표로 간다(펜스·설정 줄·설명 문장이 함께 사라진다).
//   여는 줄 ```chart+NBSP — 화면의 언어는 'chart\u00a0…'이라 그냥 코드블록인데, 여기서는 chart 블록으로
//     보아 가리지 않는다. 그 안에 적힌 ```chart 예시가 이력에서만 표로 바뀐다.
// 같은 파일의 OPEN_LINE_RE·CLOSE_LINE_RE는 처음부터 `[ \t]`만 본다 — 셋이 같은 규칙이어야 한다.
const FENCE_TAIL_RE = /[^ \t]/;
function maskLiteralFences(lines) {
  let open = null;
  return lines.map(line => {
    const m = /^[ \t]*(`{3,}|~{3,})([^\r\n]*)\r?$/.exec(line);
    if (open) {
      const result = open.literal ? '' : line;
      if (m && m[1][0] === open.ch && m[1].length >= open.len && !FENCE_TAIL_RE.test(m[2])) open = null;
      return result;
    }
    if (m && !(m[1][0] === '`' && m[2].includes('`'))) {
      open = { ch: m[1][0], len: m[1].length, literal: !/^[ \t]*chart(?:[ \t]|$)/i.test(m[2]) };
      if (open.literal) return '';
    }
    return line;
  });
}

// 대화 이력으로 보낼 때 차트 블록을 평범한 표로 되돌린다. 모델의 다음 턴에 필요한 것은 '무슨 값을
// 보여줬는가'이지 그것을 어떻게 그렸는가가 아니다 — 펜스와 설정 줄을 그대로 돌려보내면 이력
// 상한(HISTORY_LEN)의 일부를 그 글자가 먹고, 모델은 그 모양을 답변마다 흉내 낸다.
// 표는 20행까지만 남기고 나머지는 건수로 적는다.
// 우리가 새로 적는 줄(제목·건수)은 여는 펜스의 들여쓰기를 따른다. 표 줄들은 원문의 들여쓰기를 그대로
// 두는데(normalizeTable이 채우는 구분 줄도 머리글을 따른다) 이 줄들만 왼쪽 끝에 붙으면, 목록 안에 있던
// 블록의 제목이 목록 밖의 문단이 되어 항목이 거기서 끊기고 뒤의 표는 목록에서 떨어져 나간다(실측:
// `1. 항목` 아래 4칸 들여 쓴 블록의 이력이 `목록 안\n    | a | b |…`로 나가 목록이 끝났다).
export function chartBlocksToTables(md) {
  const lines = String(md ?? '').split('\n');
  const { blocks } = chartFences(maskLiteralFences(lines).join('\n'));
  if (!blocks.length) return String(md ?? '');
  const out = [];
  let at = 0;
  for (const { start, end, indent, body = '' } of blocks) {
    out.push(...lines.slice(at, start));
    at = end + 1;
    const { config, table } = splitBlock(body);
    const t = normalizeTable(table);
    // 표도 `data:` 참조도 없는 블록은 차트가 아니다 — 모델이 펜스를 다른 용도로 쓴 것이고, 화면은
    // 그 글자를 원문 그대로 코드로 보인다(App.jsx ChartTable도 같은 이유로 그렇게 한다).
    // 여기서만 지우면 사용자가 보고 있는 글을 모델만 보지 못한다 — 화면과 이력이 다른 것을 말하면
    // 다음 질문의 '## 최근 대화'는 있지도 않은 침묵을 모델의 지난 턴으로 싣는다(실측: ```chart
    // 안에 쓴 문장 하나가 이력에서 통째로 사라졌고, 화면에는 그대로 남아 있었다).
    // 본문의 들여쓰기는 원문 그대로 둔다 — 아래에서 우리가 새로 적는 줄만 여는 펜스를 따른다.
    if (!t && config.data === undefined) { out.push(body); continue; }
    const title = String(config.title ?? '').trim();
    const rep = [];
    if (title) rep.push(indent + title);
    if (t) {
      rep.push(t.header, t.sep, ...t.rows.slice(0, HISTORY_TABLE_ROWS));
      if (t.rows.length > HISTORY_TABLE_ROWS) rep.push(`${indent}(외 ${t.rows.length - HISTORY_TABLE_ROWS}행)`);
    }
    out.push(rep.join('\n'));
  }
  out.push(...lines.slice(at));
  return out.join('\n');
}
