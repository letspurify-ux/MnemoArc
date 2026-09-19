import { decodeString } from 'micromark-util-decode-string';

// 셀 안 블록의 구분자는 원문에 쓰인 문자만 사용한다. Markdown이 이미 풀어낸
// \-·&gt;·&#91;x&#93;를 목록·인용·작업 목록으로 한 번 더 실행하면 안 된다.
// 값과 같은 길이의 문법 문자열을 보관해 수식 토큰 복원·br 분할 뒤에도 출처를 유지한다.
const escapes = /\\[!-/:-@[-`{-~]|&(?:#(?:\d{1,7}|x[\da-f]{1,6})|[\da-z]{1,31});/gi;
export function markCellTextSyntax(tree, source) {
  const visit = (node, inCell = false) => {
    inCell ||= node.type === 'tableCell';
    if (inCell && node.type === 'text' && node.data?.cellSyntax === undefined) {
      const raw = node.position ? source.slice(node.position.start.offset, node.position.end.offset) : '';
      let decoded = '', syntax = '', at = 0;
      for (const match of raw.matchAll(escapes)) {
        decoded += raw.slice(at, match.index);
        syntax += raw.slice(at, match.index);
        const value = decodeString(match[0]);
        decoded += value;
        // 기존 직렬화 답변의 \> 인용 복구는 문서 밖과 같은 명시적 확장이다.
        syntax += value === match[0] || match[0] === '\\>' ? value : '\0'.repeat(value.length);
        at = match.index + match[0].length;
      }
      decoded += raw.slice(at);
      syntax += raw.slice(at);
      // 원문 좌표가 없는 합성 노드는 새 블록 구분자가 될 수 없다.
      node.data = { ...node.data, cellSyntax: decoded === node.value ? syntax : '\0'.repeat(node.value.length) };
    }
    for (const child of node.children ?? []) visit(child, inCell);
  };
  visit(tree);
}

export const sliceCellText = (node, start, end = node.value.length) => ({
  type: 'text', value: node.value.slice(start, end),
  ...(node.data?.cellSyntax !== undefined && { data: { ...node.data, cellSyntax: node.data.cellSyntax.slice(start, end) } }),
});
