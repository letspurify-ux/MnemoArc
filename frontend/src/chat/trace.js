// 화면 trace 패널(실행된 쿼리와 조회된 행)의 계약 — 열 고르기·셀 표기·CSV 만들기.
// App.jsx가 아니라 여기 있는 이유는 chart.js와 같다: 순수 함수라 node:test로 회귀 테스트가 붙는다.
// CSV는 사용자가 결과를 다른 도구로 가져가는 출구다 — 셀 하나가 조용히 옆 열로 밀리면 그 파일로
// 한 일이 전부 틀어지고, 그것을 알려 주는 오류는 없다.

// 한 스텝의 '건수' 자리에 놓을 문구. 실행되지 못한 스텝은 건수가 아니라 오류를 말한다 —
// 그 갈래를 화면에서 가르면 오류 줄만 임자 없는 글자로 남아, 검사도 픽스처도 그것을 베껴 적게 된다
// (그러면 문구를 다듬은 날 앱은 맞는데 검사가 깨지거나, 앱이 내지도 않는 글자를 검사가 보증한다).
// 스텝은 있어야 한다. 없는 것을 받으면 여기서 무슨 말인지 밝히고 멈춘다 — 조용히 넘기면 아래
// countLabel이 없는 값을 만지다 'rows를 읽을 수 없다'로 죽고, 그 말은 임자(없는 스텝을 집은 부르는
// 쪽)가 아니라 이 파일을 가리킨다. 실제로 픽스처가 스텝 하나를 find로 집어 온다(test/ui/fixtures.js).
export const stepLabel = t => {
  if (!t) throw new TypeError('stepLabel: 스텝이 없습니다 — 부르는 쪽이 없는 스텝을 집었습니다');
  if (isSearchStep(t)) return searchLabel(t);
  return t.error ? `오류: ${t.error}` : countLabel(t);
};

// ===== 검색 항목 =====
// 서버 trace에는 검색 기록이 쿼리 기록과 같은 배열에 섞여 온다 (backend result.js clientTrace — {search, targets,
// hits, failed?}). 답을 기다리는 동안 흘러오는 진행 이벤트(search·search_done)도 같은 재료다. 문구는 한 곳에서
// 만든다 — 진행 줄과 패널이 같은 검색을 다른 말로 하면 사용자는 둘을 다른 일로 읽는다.
// 대상 이름과 적중 수의 키는 서버가 정한다 (backend constants.js SEARCH_TARGETS, agent.js HIT_KEY).
export const TARGET_LABEL = { knowledge: '지식', qa_method: '처리방법', query: '쿼리' };
const HIT_KEY = { knowledge: 'knowledge', qa_method: 'qaMethods', query: 'queries' };
const TARGETS = Object.keys(TARGET_LABEL);

export const isSearchStep = t => !!t && typeof t === 'object' && typeof t.search === 'string';

// 대상 목록의 표기 — '지식·처리방법'. 모르는 이름은 그대로 보인다(감추면 무엇을 찾았는지 알 수 없다).
// 표는 자기가 담은 이름만 안다 — 객체의 [] 조회는 프로토타입까지 올라가서, 'constructor' 같은 이름이 오면
// Object 함수가 '아는 이름'으로 잡혀 화면에 'function Object() { [native code] }'가 섰다(실측). 지금 서버는
// 대상을 정규화해 보내지만(llm.js normalizeSearchTargets) 이 파일은 배포가 어긋난 서버의 값도 받는 문이다.
const labelOf = t => (Object.prototype.hasOwnProperty.call(TARGET_LABEL, t) ? TARGET_LABEL[t] : cellText(t));
export const targetsLabel = targets =>
  (Array.isArray(targets) ? targets : []).map(labelOf).filter(Boolean).join('·') || '전체';

// 검색 결과 문구 — '지식 2건 · 처리방법 검색 불가'. 검색 불가는 0건과 다른 말이다: 임베딩 서버가 없어
// 찾아보지 못한 것이지 등록이 없는 것이 아니다 (서버 프롬프트도 같은 말을 쓴다). 찾지 않은 대상은 나오지 않는다.
export function searchLabel(t) {
  const failed = new Set(Array.isArray(t?.failed) ? t.failed : []);
  const hits = t?.hits && typeof t.hits === 'object' ? t.hits : {};
  const parts = TARGETS.map(k => {
    if (failed.has(k)) return `${TARGET_LABEL[k]} 검색 불가`;
    const n = hits[HIT_KEY[k]];
    return typeof n === 'number' ? `${TARGET_LABEL[k]} ${n}건` : null;
  }).filter(Boolean);
  return parts.length ? parts.join(' · ') : '결과 없음';
}

// 패널 머리띠의 문구 — '검색 1회 · 실행된 쿼리 2건'. 검색이 없으면 예전 그대로 '실행된 쿼리 N건'이다.
export function traceSummary(trace) {
  const list = Array.isArray(trace) ? trace : [];
  const searches = list.filter(isSearchStep).length;
  const queries = list.length - searches;
  const parts = [];
  if (searches > 0) parts.push(`검색 ${searches}회`);
  if (queries > 0 || searches === 0) parts.push(`실행된 쿼리 ${queries}건`);
  return parts.join(' · ');
}

// ===== 답을 기다리는 동안의 진행 줄 =====
// 서버가 흘려보내는 이벤트(backend agent.js emit)를 화면의 줄 목록으로 접는다. 시작 이벤트가 줄을 세우고
// 끝 이벤트가 그 줄에 결과를 붙인다 — 같은 종류의 '아직 안 끝난' 마지막 줄을 찾아서. 순수 함수라 여기 둔다:
// 이벤트 순서가 어긋나거나(끝이 먼저 온다) 모르는 종류가 와도 목록이 깨지지 않아야 하는데, 그 경우는 화면에서
// 재현하기 어렵다.
const strings = v => (Array.isArray(v) ? v.filter(x => typeof x === 'string') : []);

// 끝 이벤트를 '그 줄'에 붙인다. 미완 줄이 여럿일 수 있으므로(일괄 조회는 조회 여럿이 동시에 돈다 —
// backend agent.js runBatch) 짝을 찾아야 한다. 마지막 미완 줄에 붙이던 때에는 가운데 조회가 먼저 끝나는
// 순간 줄이 뒤바뀌었다: A·B·C가 도는 중에 B가 끝나면 C의 줄이 'B → 2건'이 되어 'B'가 두 줄로 보이고
// C는 화면에서 사라졌다(실측).
// 짝은 서버가 매기는 번호(id)로 짓는다 — 이름으로는 지을 수 없다: 같은 쿼리를 다른 값으로 두 번 부르는
// 배치가 정당하고, 대상 DB의 철자도 시작 이벤트(모델이 적은 것)와 끝 이벤트(등록 철자)가 다를 수 있다.
// 번호를 주지 않는 서버·옛 배포에서는 이름과 대상 DB로 물러서되 철자 차이는 흡수한다(nameKey).
// 짝을 못 찾았을 때 할 일이 두 경우에 다르다:
//   번호로 찾았는데 없다 — 그 번호의 시작을 못 받았거나 끝이 두 번 왔다는 뜻이다. 남의 줄에 붙이면
//     그 줄이 다른 조회의 이름과 건수를 뒤집어쓰고(한 조회는 화면에서 사라진다) 진짜 끝이 나중에 와서
//     세 번째 줄을 만든다 — 새 줄로 세우는 편이 잃는 것이 없다.
//   이름으로 물러섰는데 없다 — 애초에 무엇과 짝인지 알 수 없는 옛 배포다. 마지막 미완 줄에 붙인다.
// 미완 줄이 아예 없으면 어느 쪽이든 새 줄로 세운다 (시작 없이 끝만 온 이벤트).
const settle = (list, kind, done, same, strict = false) => {
  const pending = [];
  list.forEach((x, i) => { if (x.kind === kind && x.pending) pending.push(i); });
  if (!pending.length) return [...list, done];
  const found = pending.find(j => same(list[j]));
  if (found === undefined && strict) return [...list, done];
  const i = found ?? pending[pending.length - 1];
  // 짝지은 줄의 값만 갈아 끼운다. id는 시작 줄의 것을 남긴다 — 끝 이벤트가 번호를 주지 않는 경우에도
  // 그 줄이 자기 번호를 잃지 않게(뒤이은 다른 끝 이벤트가 그 줄을 다시 집지 않는다).
  return list.map((x, j) => (j === i ? { ...x, ...done, id: x.id ?? done.id } : x));
};

// 이름 비교 키 — 대소문자·앞뒤 공백을 흡수한다 (backend constants.js nameKey와 같은 규칙).
const nameKey = v => cellText(v).trim().toLowerCase();

export function applyProgress(list, e) {
  const cur = Array.isArray(list) ? list : [];
  if (!e || typeof e !== 'object') return cur;
  switch (e.type) {
    case 'search':
      return [...cur, { kind: 'search', text: cellText(e.text), targets: strings(e.targets), pending: true }];
    case 'search_done': {
      const done = {
        kind: 'search', text: cellText(e.text), targets: strings(e.targets),
        hits: e.hits && typeof e.hits === 'object' ? e.hits : {}, failed: strings(e.failed), pending: false,
      };
      return settle(cur, 'search', done, x => x.text === done.text);
    }
    case 'run_query':
      return [...cur, { kind: 'query', id: e.id, query_name: cellText(e.query_name), targetDb: cellText(e.targetDb), pending: true }];
    case 'run_query_done': {
      const done = {
        kind: 'query', id: e.id, query_name: cellText(e.query_name), targetDb: cellText(e.targetDb),
        rowCount: typeof e.rowCount === 'number' || typeof e.rowCount === 'string' ? e.rowCount : undefined,
        error: errorText(e.error), pending: false,
      };
      const byId = done.id !== undefined;
      const same = byId
        ? x => x.id === done.id
        : x => nameKey(x.query_name) === nameKey(done.query_name) && nameKey(x.targetDb) === nameKey(done.targetDb);
      return settle(cur, 'query', done, same, byId);
    }
    default:
      return cur;
  }
}

// 조회 건수의 표기 — 서버는 상한에 걸린 결과를 '1000+'처럼 준다(backend result.js). 진행 줄과 패널이
// 그것을 다르게 풀면 같은 조회가 몇 초 사이에 '1000+건'이었다가 '1000건 이상'이 된다 — 이 파일이
// 문구를 한곳에서 만드는 이유가 그것이다(머리말). 진행 줄에는 아직 몇 행을 실을지가 정해지지 않았으므로
// 패널 문구(countLabel)의 앞부분만 쓴다.
const rowCountLabel = v => {
  const s = String(v ?? 0);
  return s.endsWith('+') ? `${s.slice(0, -1)}건 이상` : `${s}건`;
};

// 진행 줄 하나의 글자. 끝나지 않은 줄은 결과 없이 끝난다(기다림 표시는 CSS가 붙인다).
export function progressText(it) {
  if (it.kind === 'search') {
    const head = `검색 "${it.text}" (${targetsLabel(it.targets)})`;
    return it.pending ? head : `${head} → ${searchLabel(it)}`;
  }
  const head = `조회 ${it.query_name || '(이름 없음)'}${it.targetDb ? `@${it.targetDb}` : ''}`;
  if (it.pending) return head;
  return `${head} → ${it.error ? `오류: ${it.error}` : rowCountLabel(it.rowCount)}`;
}

// 조회 건수 문구. 조회 건수와 실린 행 수는 다를 수 있다 — 몇 건을 보고 있는지 밝히지 않으면
// 사용자가 실린 것을 전부로 읽는다 (서버 result.js clientTrace가 omittedRows·capped를 준다).
export function countLabel(t) {
  const n = t.rows?.length ?? 0;
  // rowCount는 서버가 늘 준다(result.js clientTrace). 없으면 실린 행 수로 말한다 — 'undefined건'은
  // 화면에 나가서는 안 되는 글자이고, 이 패널의 다른 값들도 모두 없을 때를 정해 두고 있다.
  if (t.rowCount === undefined) return `${n}건`; // 몇 건 중 몇 건인지 말할 근거가 없다
  // 상한에 걸린 것과 실린 행이 일부인 것은 함께 올 수 있다(서버 result.js) — 그때 둘 중 하나만
  // 말하면 '왜 이만큼뿐인가'의 절반이 사라진다.
  // 상한에 걸린 rowCount는 서버가 '1000+'처럼 준다(result.js) — 같은 패널에서 '건 이상'과 '+건'이
  // 섞이지 않게 여기서도 같은 말로 푼다.
  if (t.capped && t.omittedRows) return `${String(t.rowCount).replace(/\+$/, '')}건 이상 — 조회 상한에 걸렸고, 아래는 그중 ${n}건입니다`;
  if (t.omittedRows) return `${t.rowCount}건 (아래는 그중 ${n}건)`;
  if (t.capped) return `${n}건 이상 — 조회 상한에 걸려 처음 ${n}건만 가져왔습니다`;
  return `${t.rowCount}건`;
}

// 서버가 준 trace를 화면이 그릴 수 있는 모양으로 맞춘다. 이 값의 모양은 우리가 정하지 못한다 —
// 배포가 어긋난 서버, 중간에 낀 프록시의 응답, 앞으로 늘어날 필드가 모두 이 문으로 들어온다.
// 모양이 어긋나면 그리는 쪽이 렌더 도중에 던지고, 그것은 이 화면에서 '대화가 통째로 사라진다'와
// 같은 말이다(이력은 메모리에만 있다). 실측으로 확인한 세 자리: trace가 문자열이면 trace.map에서,
// 원소가 null이면 t.rows에서, query_name이 객체면 React의 '객체는 자식이 될 수 없다'에서 백지가 됐다.
// 그리는 자리마다 방어를 흩뿌리는 대신 들어오는 문 하나에서 맞춘다 — 순수 함수라 회귀 테스트가 붙는다.
// (App.jsx의 말풍선 경계가 마지막 그물이지만, 그물에 걸린 말풍선은 원문만 남는다 — 여기서 맞출 수
//  있는 것은 맞춰서 제대로 보이는 편이 낫다.)
//   배열이 아니면 없는 것으로 본다 — 패널을 아예 그리지 않는다.
//   스텝이 객체가 아니면 버린다: 무엇을 실행했는지조차 없는 항목이라 보여줄 것이 없다.
//   query_name·targetDb는 글자로. 둘 다 화면의 자식이 되고 CSV 파일 이름이 된다.
//   rows는 배열일 때만 행으로 본다 — 아니면 실행되지 못한 스텝과 같이 표도 CSV 단추도 없다.
//   검색 항목(search가 글자)은 검색어·대상·적중 수만 남긴다 — 표도 CSV 단추도 없는 한 줄이다.
export function normalizeTrace(trace) {
  if (!Array.isArray(trace)) return [];
  return trace
    .filter(t => t && typeof t === 'object' && !Array.isArray(t))
    .map(t => (isSearchStep(t) ? searchStep(t) : queryStep(t)));
}
const searchStep = t => ({
  ...(stepNo(t.step) !== undefined && { step: stepNo(t.step) }),
  search: t.search, targets: strings(t.targets), hits: t.hits && typeof t.hits === 'object' ? t.hits : {}, failed: strings(t.failed),
});
function queryStep(t) {
  // 건수와 오류는 글자가 되는 값이다 — 진행 줄(applyProgress run_query_done)이 받는 모양과 같아야 한다.
  // 그대로 통과시키던 동안에는 객체가 오면 패널이 '[object Object]건'·'오류: [object Object]'를 그렸다 —
  // 죽지는 않지만 뜻 없는 글자가 화면에 나간다. 모양이 어긋난 값은 키째로 뗀다(없는 것과 같은 모양이다).
  const { rowCount, error, ...rest } = t;
  const count = typeof rowCount === 'number' || typeof rowCount === 'string' ? rowCount : undefined;
  const err = errorText(error);
  return {
    ...rest,
    step: stepNo(t.step),
    query_name: cellText(t.query_name),
    targetDb: cellText(t.targetDb),
    rows: Array.isArray(t.rows) ? t.rows : undefined,
    ...(count !== undefined && { rowCount: count }),
    ...(err !== undefined && { error: err }),
  };
}

// 오류 문구의 모양. 서버는 글자를 주지만(result.js clientTrace) 글자가 아닌 것이 와도 '오류였다'는 사실은
// 남겨야 한다 — 버리면 실행되지 못한 스텝이 '0건'으로 읽혀 조회 성공처럼 보인다. 비었으면 오류가 아니다.
// 진행 줄(applyProgress)과 패널(normalizeTrace)이 같은 함수를 쓴다 — 같은 스텝이 몇 초 사이에 다른 말을 하지 않게.
const errorText = v => (v ? cellText(v) : undefined);

// 이력의 절대 순번. 화면의 자식이 되는 값이라 여기서 모양을 맞춘다 — 객체가 그대로 통과하면 React가
// '객체는 자식이 될 수 없다'로 던지고, 이 파일이 문 앞에서 값을 맞추기로 한 이유가 정확히 그것이다.
// 정수가 아니면 없는 것으로 본다: 번호가 없으면 화면이 번호를 그리지 않을 뿐이다.
const stepNo = v => (Number.isInteger(v) && v > 0 ? v : undefined);

// 행들의 열 이름 — 첫 등장 순서. 드라이버가 준 행은 열이 모두 같지만(oracle.js normalizeCells),
// 다른 출처의 trace가 섞여도 한 행에만 있는 열이 빠지지 않게 합집합으로 모은다.
export function columnsOf(rows) {
  const cols = [];
  const seen = new Set();
  for (const r of rows ?? []) {
    if (!r || typeof r !== 'object') continue;
    for (const k of Object.keys(r)) if (!seen.has(k)) { seen.add(k); cols.push(k); }
  }
  return cols;
}

// 열의 합집합에 있어도 이 행에 없는 셀은 빈칸이다. __proto__ 등 상속된 값은 조회 결과가 아니다.
export const cellValue = (row, column) => row != null && Object.prototype.hasOwnProperty.call(row, column)
  ? row[column] : undefined;

// 셀의 표기. null·undefined는 빈칸(문자열 'null'이 값처럼 보이지 않게), 숫자는 그대로,
// 객체가 오면(드라이버 경계가 막지만) JSON으로 — [object Object]로 뭉개지지 않게.
export function cellText(v) {
  if (v === null || v === undefined) return '';
  if (typeof v === 'string') return v;
  if (typeof v === 'number' || typeof v === 'boolean' || typeof v === 'bigint') return String(v);
  try { return JSON.stringify(v); } catch { return String(v); }
}

// 스프레드시트가 수식으로 읽는 첫 글자(= @ 탭 CR + -)로 시작하는 문자열은 앞에 '를 붙여 글자로 고정한다 —
// DB의 자유 텍스트(VOC 본문 등)가 파일을 연 사람의 PC에서 수식으로 실행되지 않게. 숫자 셀은 문자열이
// 아니므로 -5 같은 값은 건드리지 않고, 문자열이라도 통째로 숫자 표기인 것('-5', 16자리 넘어 문자열로 온
// NUMBER)과 '-' 하나뿐인 값('없음' 표시로 흔하다)은 그대로 둔다 — 스프레드시트도 그것은 수식이 아니라
// 숫자·글자로 읽는다. '+82-10-1234-5678' 같은 전화번호는 예외가 아니다: '+'로 시작하는 문자열을 엑셀은
// 수식으로 읽어 82-10-1234-5678 = -6840을 보여주고, '-abc'는 #NAME? 오류가 된다.
const FORMULA_START = /^[=@\t\r+-]/;
const NOT_FORMULA = /^(?:[+-]?(?:\d+\.?\d*|\.\d+)(?:e[+-]?\d+)?|[+-])$/i;
const guardFormula = (v, s) => typeof v === 'string' && FORMULA_START.test(s) && !NOT_FORMULA.test(s) ? `'${s}` : s;

// RFC 4180: 쉼표·따옴표·줄바꿈이 든 값은 따옴표로 감싸고 따옴표는 두 번 쓴다. 앞에 BOM을 붙인다 —
// 엑셀은 BOM 없는 UTF-8 CSV를 로캘 인코딩으로 읽어 한글을 깨뜨린다.
export function toCsv(rows, cols = columnsOf(rows)) {
  const field = v => {
    const s = guardFormula(v, cellText(v));
    return /[",\r\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
  };
  const line = vals => vals.map(field).join(',');
  return '﻿' + [line(cols), ...(rows ?? []).map(r => line(cols.map(c => cellValue(r, c))))].join('\r\n') + '\r\n';
}

// 내려받을 파일 이름. 쿼리 이름은 등록자가 정한 문자열이라 파일 이름에 못 쓰는 글자가 섞일 수 있다.
export function csvFileName(name, targetDb) {
  const base = `${name || 'result'}${targetDb ? `@${targetDb}` : ''}`.replace(/[\\/:*?"<>|\x00-\x1f]+/g, '_').trim();
  return `${base || 'result'}.csv`;
}
