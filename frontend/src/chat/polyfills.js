// 빌드 타깃(vite.config.js: chrome87·safari14 등) 안의 브라우저에 없는 런타임 API를 채운다.
//
// vite(esbuild)는 문법만 그 타깃으로 낮출 뿐 런타임 API는 채워 주지 않는다 — App.jsx가 AbortSignal.timeout
// 대신 AbortController를 쓰는 이유와 같다. 그런데 의존성은 그 사정을 모른다: react-markdown(hast-util-to-jsx-runtime)과
// recharts가 Object.hasOwn(Chrome 93·Safari 15.4부터)을 렌더 경로에서 부른다. 그 API가 없는 브라우저에서는
// <ReactMarkdown>이 렌더 도중에 던지고, 말풍선 경계(App.jsx Boundary)가 그것을 잡아 '이 답변을 그리지 못했습니다 —
// 원문을 그대로 보입니다'로 바꾼다 — 답변마다, 표도 차트도 실행 과정 패널도 없이(실측: Object.hasOwn을 지운 Chrome에서
// 그렇게 됐다). 오류로는 콘솔에만 남고, 지원한다고 적어 둔 브라우저에서 화면이 통째로 퇴화한다.
//
// 그래서 진입점(main.jsx)이 무엇보다 먼저 이 파일을 읽는다. 있는 브라우저에서는 아무 일도 하지 않는다.
// 명세(ECMA-262 Object.hasOwn)대로: 대상은 객체로 바꾸되 null·undefined는 던지고, 키는 프로퍼티 키로 바꾼다.
// 상속받은 프로퍼티는 false다 — hasOwnProperty를 프로토타입에서 직접 부르므로 대상이 그 이름을 덮어써도(모델이 쓴
// 열 이름 'hasOwnProperty' 같은 것) 흔들리지 않는다.
if (typeof Object.hasOwn !== 'function') {
  Object.defineProperty(Object, 'hasOwn', {
    value: function hasOwn(target, key) {
      if (target === null || target === undefined) throw new TypeError('Object.hasOwn called on null or undefined');
      return Object.prototype.hasOwnProperty.call(Object(target), key);
    },
    writable: true, configurable: true, enumerable: false,
  });
}

// Array.prototype.at·String.prototype.at (Chrome 92·Safari 15.4부터). 우리 코드는 쓰지 않지만 mermaid가 클래스·상태·ER
// 다이어그램과 markdown 라벨의 파서에서 부른다 — 없는 브라우저에서는 그 종류만 '문법 오류'처럼 원문 코드로 남고
// 흐름도·시퀀스·간트는 그려진다(실측: Chrome 87~91 흉내). 한 답변 안에서 그림 종류에 따라 되고 안 되고가 갈리면
// 사용자는 모델이 틀린 것으로 읽는다. 명세대로: 정수로 바꾼 index가 음수면 뒤에서 세고, 범위 밖이면 undefined.
// (structuredClone을 쓰는 자리는 아래에서 채운다 — 'pie 하나뿐'이라고 적어 두었던 것이 틀렸다. 그 아래 주석 참고.)
const at = function at(index) {
  const o = Object(this);
  const len = Math.min(Math.max(Math.trunc(Number(o.length)) || 0, 0), Number.MAX_SAFE_INTEGER);
  const rel = Math.trunc(Number(index)) || 0;
  const k = rel >= 0 ? rel : len + rel;
  return k < 0 || k >= len ? undefined : o[k];
};
for (const proto of [Array.prototype, String.prototype]) {
  if (typeof proto.at !== 'function') Object.defineProperty(proto, 'at', { value: at, writable: true, configurable: true, enumerable: false });
}

// URL.canParse (Chrome 120·Safari 17·Firefox 115부터). 우리 코드는 쓰지 않지만 mermaid가 그림 안의 링크를
// 정화하는 길에서 부른다: formatUrl → @braintree/sanitize-url → isValidUrl → URL.canParse. securityLevel이
// 'loose'가 아니면(우리는 'strict'다 — Mermaid.jsx) http(s) 링크는 늘 그 길을 지난다.
// 없는 브라우저에서는 그 한 줄이 던져 mermaid.render가 거부되고, 링크가 든 그림만 원문 코드로 남는다
// (실측: `click A "https://…"` 흐름도와 classDiagram의 `link`가 'URL.canParse is not a function'으로 코드가 됐고,
// 링크 없는 그림은 멀쩡했다). 오류는 콘솔에만 남으므로, 지원한다고 적어 둔 브라우저에서 '어떤 그림만'
// 소리 없이 안 그려진다 — Object.hasOwn·Array.prototype.at 때와 같은 부류다.
// 명세(WHATWG URL): new URL(url, base)가 성공하면 true, 던지면 false. base를 넘길지 말지를 가른다 —
// 빈 문자열 base는 '없음'이 아니라 파싱 실패이고(네이티브도 false), undefined는 '넘기지 않은 것'이다.
// enumerable이 위의 둘과 다르다: Object.hasOwn·at은 ECMAScript 내장이라 열거되지 않지만 URL.canParse는
// WebIDL의 정적 연산이라 열거된다(실측: 네이티브에서 propertyIsEnumerable이 true). 폴리필이 그것까지
// 같아야 새 브라우저와 옛 브라우저에서 URL의 모양이 갈리지 않는다.
// 맞추지 않은 것 둘: 인자 없이 부르면 네이티브는 TypeError를 던지지만(WebIDL 인자 수) 여기서는 false다.
// 심벌을 넘겼을 때도 네이티브는 던지고 여기서는 false다. 부르는 자리(sanitize-url)는 늘 문자열 하나를
// 넘기므로 닿지 않는 갈래이고, 그것까지 흉내 내면 읽기만 어려워진다.
if (typeof URL.canParse !== 'function') {
  Object.defineProperty(URL, 'canParse', {
    value: function canParse(url, base) {
      try {
        if (base === undefined) new URL(url);
        else new URL(url, base);
        return true;
      } catch { return false; }
    },
    writable: true, configurable: true, enumerable: true,
  });
}

// structuredClone (Chrome 98·Safari 15.4·Firefox 94부터). 우리 코드는 쓰지 않지만 mermaid가 그림을 그리는
// 길에서 부른다. 앞서 '쓰는 자리가 pie 하나뿐이라 원문 코드로 남는 쪽을 택했다'고 적어 두었는데 그것이
// 틀렸다 — 번들을 다시 훑어 보니 네 청크가 부르고, 그중 하나가 **흐름도의 자기 고리**다:
//   dagre(흐름도 배치) — 자기 자신을 가리키는 화살표(`A --> A`)의 모서리를 넷으로 복제하는 자리
//   mermaid 코어 — 관계 끝 라벨의 자리 계산(calcTerminalLabelPosition)
//   pieDiagram · radar · cytoscape(architecture)
// 실측(structuredClone을 지운 Chrome): 평범한 흐름도·시퀀스·간트·ER·클래스·상태는 그려지는데
// `A --> A` 한 줄이 든 흐름도와 원그래프만 'structuredClone is not defined'로 원문 코드가 됐다.
// 흐름도는 이 화면에서 가장 흔한 그림이고(서버 프롬프트가 절차·흐름을 흐름도로 그리게 한다) 자기 고리는
// 재시도·반복을 그릴 때 모델이 자연스럽게 쓴다 — 한 답변 안에서 그림 종류가 아니라 '그 안에 무엇이 있는가'로
// 되고 안 되고가 갈리면 사용자는 모델이 틀린 것으로 읽는다. Object.hasOwn·Array.prototype.at·URL.canParse와
// 같은 부류이므로 같은 자리에서 채운다.
//
// 명세(HTML StructuredSerialize/Deserialize)를 따르되 옮길 수 없는 것은 명세와 같이 거절한다:
//   함수·심벌·DOM 노드는 DataCloneError. 원형(prototype)은 옮기지 않는다 — 클래스의 인스턴스도 평범한
//   객체가 된다(명세가 그렇다). 속성 서술자·getter/setter도 옮기지 않고 값만 읽는다. 심벌 키는 빠진다.
//   순환 참조는 같은 자리를 가리키게 한다(memo). transfer 옵션은 다루지 않는다 — 부르는 자리가 쓰지 않는다.
// 실패 방향은 한쪽이다: 여기서 옮기지 못하는 값을 만나면 던지고, 그림은 지금과 같이 원문 코드로 남는다.
if (typeof structuredClone !== 'function') {
  const fail = what => { throw new DOMException(`${what} could not be cloned.`, 'DataCloneError'); };
  const TYPED = ['Int8Array', 'Uint8Array', 'Uint8ClampedArray', 'Int16Array', 'Uint16Array',
    'Int32Array', 'Uint32Array', 'Float32Array', 'Float64Array', 'BigInt64Array', 'BigUint64Array'];
  // 대입은 __proto__ setter를 부르므로 데이터가 사라지거나 순환 원형 오류가 난다.
  // 네이티브처럼 원형을 건드리지 않고 자신의 데이터 속성으로 만든다.
  const put = (out, key, value) => Object.defineProperty(out, key, {
    value, writable: true, enumerable: true, configurable: true,
  });
  const clone = (v, seen) => {
    if (v === null || typeof v !== 'object') {
      if (typeof v === 'function') fail('A function');
      if (typeof v === 'symbol') fail('A symbol');
      return v;
    }
    if (seen.has(v)) return seen.get(v);
    // 문서의 요소는 옮길 수 없다 (명세의 DataCloneError). typeof Node로 확인하는 이유는 이 파일이
    // 브라우저 밖(단위 검사)에서도 읽히기 때문이다.
    if (typeof Node === 'function' && v instanceof Node) fail('A DOM node');
    const tag = Object.prototype.toString.call(v).slice(8, -1);
    let out;
    if (tag === 'Date') return seen.set(v, out = new Date(v.getTime())), out;
    // lastIndex는 명세가 옮기지 않는다
    if (tag === 'RegExp') return seen.set(v, out = new RegExp(v.source, v.flags)), out;
    if (tag === 'ArrayBuffer') {
      out = new ArrayBuffer(v.byteLength);
      new Uint8Array(out).set(new Uint8Array(v));
      return seen.set(v, out), out;
    }
    if (tag === 'DataView') {
      out = new DataView(clone(v.buffer, seen), v.byteOffset, v.byteLength);
      return seen.set(v, out), out;
    }
    if (TYPED.includes(tag)) {
      out = new globalThis[tag](clone(v.buffer, seen), v.byteOffset, v.length);
      return seen.set(v, out), out;
    }
    if (tag === 'Boolean' || tag === 'Number' || tag === 'String') return seen.set(v, out = Object(v.valueOf())), out;
    if (tag === 'Error') {
      // 이름이 표준 오류면 그 종류로, 아니면 Error로. message·stack·cause만 옮긴다(명세의 목록).
      const Ctor = typeof globalThis[v.name] === 'function' && globalThis[v.name].prototype instanceof Error ? globalThis[v.name] : Error;
      out = new Ctor(v.message);
      seen.set(v, out);
      if (v.stack !== undefined) out.stack = v.stack;
      if ('cause' in v) out.cause = clone(v.cause, seen);
      return out;
    }
    if (Array.isArray(v)) {
      out = new Array(v.length);
      seen.set(v, out);
      for (const k of Object.keys(v)) put(out, k, clone(v[k], seen));
      return out;
    }
    if (tag === 'Map') {
      out = new Map();
      seen.set(v, out);
      v.forEach((val, key) => out.set(clone(key, seen), clone(val, seen)));
      return out;
    }
    if (tag === 'Set') {
      out = new Set();
      seen.set(v, out);
      v.forEach(val => out.add(clone(val, seen)));
      return out;
    }
    // 옮길 수 없다고 명세가 못 박은 것들 — 조용히 빈 객체로 만들면 부르는 쪽이 그것을 값으로 믿는다.
    if (tag === 'WeakMap' || tag === 'WeakSet' || tag === 'Promise' || tag === 'Symbol') fail(`A ${tag}`);
    // 그 밖은 평범한 객체로 (클래스의 인스턴스도 원형 없이 값만 — 명세가 그렇다)
    out = {};
    seen.set(v, out);
    for (const k of Object.keys(v)) put(out, k, clone(v[k], seen));
    return out;
  };
  Object.defineProperty(globalThis, 'structuredClone', {
    value: function structuredClone(value) { return clone(value, new Map()); },
    writable: true, configurable: true, enumerable: true,
  });
}
