import { createElement } from 'react';

// 미리보기에서는 아직 채워지지 않은 참조와 미완성 흐름도를 자리 표시로 그린다.
// 원문에서 펜스를 따로 찾으면 들여쓴 코드 예시까지 지우고, 목록 밖으로 나온 문장도
// 열린 펜스의 일부로 삼킨다. 최종 답변과 같은 markdown 파서가 확정한 코드 노드만 판정한다.
export const PLACEHOLDER_TEXT = '(표·차트를 준비하고 있습니다)';
export const PLACEHOLDER = `_${PLACEHOLDER_TEXT}_`;
export const isPreviewBlock = language => /^(?:chart|table|mermaid)$/i.test(language ?? '');

export function PreviewPre({ node, children, ...props }) {
  const code = node?.children?.[0];
  const classes = code?.type === 'element' && code.tagName === 'code' ? code.properties?.className : [];
  const target = (Array.isArray(classes) ? classes : [classes])
    .some(cls => isPreviewBlock(/^language-(.+)$/i.exec(String(cls ?? ''))?.[1]));
  return target
    ? createElement('p', null, createElement('em', null, PLACEHOLDER_TEXT))
    : createElement('pre', props, children);
}
