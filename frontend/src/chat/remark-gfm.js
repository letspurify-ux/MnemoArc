import remarkGfm from 'remark-gfm';

// GFM의 자동 링크 후보는 모든 영숫자에서 text 이벤트를 나눈다. 미완성 수식처럼
// 이스케이프와 짧은 단어가 반복되면 micromark의 data 병합(splice)이 제곱 비용이다.
// 원문상 성립할 수 없는 후보만 previous 단계에서 제외해 불필요한 분할을 막는다.
// 실제 링크의 파싱·정화는 기존 GFM이 그대로 담당한다.
export default function remarkGfmBounded(options) {
  remarkGfm.call(this, options);
  const parse = this.parser;
  if (typeof parse !== 'function') return;
  let source = '', emailRanges = [];
  const emailPossible = offset => {
    let low = 0, high = emailRanges.length;
    while (low < high) {
      const mid = (low + high) >>> 1;
      if (emailRanges[mid][1] <= offset) low = mid + 1;
      else high = mid;
    }
    return low < emailRanges.length && emailRanges[low][0] <= offset;
  };
  const guards = {
    emailAutolink: emailPossible,
    protocolAutolink: offset => /^https?:\/\//i.test(source.slice(offset, offset + 8)),
    wwwAutolink: offset => source.slice(offset, offset + 4).toLowerCase() === 'www.',
  };
  const extension = this.data('micromarkExtensions').at(-1);
  extension.text = Object.fromEntries(Object.entries(extension.text).map(([key, constructs]) => [key,
    (Array.isArray(constructs) ? constructs : [constructs]).map(construct => guards[construct.name] ? {
      ...construct,
      previous(code) {
        // 확장을 별도 fromMarkdown 구조 분석에 넘기면 this.parser를 거치지 않는다.
        // 원문 문맥이 없는 호출에서는 기본 GFM 판정을 그대로 사용한다.
        return (!source || guards[construct.name](this.now().offset)) &&
          (!construct.previous || construct.previous.call(this, code));
      },
    } : construct),
  ]));
  this.parser = (...args) => {
    const previousSource = source, previousRanges = emailRanges;
    source = String(args[0]);
    emailRanges = [];
    if (source.includes('@')) for (const word of source.matchAll(/\S+/g)) {
      const at = word[0].lastIndexOf('@');
      if (at > 0) emailRanges.push([word.index, word.index + at]);
    }
    try { return parse(...args); }
    finally { source = previousSource; emailRanges = previousRanges; }
  };
}
