// 구분자 없는 수식 판정과 KaTeX 호환 변환이 같은 환경 목록·여닫는 짝을 사용한다.
const ENVIRONMENTS = new Set([
  'gather', 'gather*', 'gathered', 'split', 'align', 'align*', 'aligned',
  'alignat', 'alignat*', 'alignedat', 'equation', 'equation*', 'flalign', 'flalign*',
  'multline', 'multline*', 'multiline', 'multiline*', 'subequations', 'subequation',
  'matrix', 'pmatrix', 'bmatrix', 'Bmatrix', 'vmatrix', 'Vmatrix', 'smallmatrix',
  'cases', 'dcases', 'rcases', 'drcases', 'array',
  'matrix*', 'pmatrix*', 'bmatrix*', 'Bmatrix*', 'vmatrix*', 'Vmatrix*', 'CD',
  'darray', 'subarray', 'multlined', 'eqnarray', 'eqnarray*',
]);
const DISPLAY_ENVIRONMENTS = new Set(['gather', 'gather*', 'align', 'align*', 'alignat', 'alignat*',
  'equation', 'equation*', 'split', 'CD']);

// \verb 안의 TeX 표기는 명령이 아니라 원문이다. 정규식 치환으로는 이 경계를 알 수 없다.
export function verbEnd(tex, start) {
  const head = /^\\verb\*?([^A-Za-z\s])/.exec(tex.slice(start, start + 8));
  if (!head) return start;
  const end = tex.indexOf(head[1], start + head[0].length);
  return end < 0 ? tex.length - 1 : end;
}

// 완성된 TeX 인자의 좌표를 한 번의 순회로 계산한다. 미완성 여는 중괄호가
// 반복돼도 각 위치에서 닫는 기호를 다시 찾지 않는다. 모든 호출 경로에서
// 이스케이프·주석·verb가 같은 그룹 경계를 사용한다.
export function texGroupEnds(tex, start = 0, end = tex.length) {
  const stack = [], groups = new Map();
  for (let i = start; i < end; i++) {
    if (tex[i] === '\\') { i = Math.max(i + 1, verbEnd(tex, i)); continue; }
    if (tex[i] === '%') { while (i < end && !/[\r\n]/.test(tex[i])) i++; continue; }
    if (tex[i] === '{') stack.push(i);
    else if (tex[i] === '}' && stack.length) groups.set(stack.pop(), i);
  }
  return groups;
}

export function requiresDisplay(tex) {
  for (let i = 0; i < tex.length; i++) {
    if (tex[i] === '%') { while (i < tex.length && !/[\r\n]/.test(tex[i])) i++; continue; }
    if (tex[i] !== '\\') continue;
    const verb = verbEnd(tex, i);
    if (verb !== i) { i = verb; continue; }
    if (/^\\tag\*?\s*\{/.test(tex.slice(i, i + 32))) return true;
    const environment = /^\\begin\s*\{\s*([A-Za-z]+\*?)\s*\}/.exec(tex.slice(i, i + 80));
    if (environment && DISPLAY_ENVIRONMENTS.has(environment[1])) return true;
    i++;
  }
  return false;
}

// 원문 좌표를 유지한다. 정규식 하나로 끝을 찾으면 중첩 환경과 여러 수식의 경계가 섞인다.
// 백슬래시 이스케이프와 TeX 주석은 건너뛰며, 닫히지 않거나 이름이 다른 짝은 복구하지 않는다.
export function mathEnvironments(tex, { textContext = false, includeUnknown = false } = {}) {
  const roots = [];
  const stack = [];
  // 문서에서는 앞의 미완성 수식 때문에 뒤의 완성된 수식까지 숨기지 않는다.
  // TeX 내부 호환 변환에서는 불완전한 부모를 그대로 오류 처리한다.
  const recover = nodes => {
    if (textContext) for (const node of nodes) roots.push(...(node?.children ?? []));
  };
  const token = /\\(begin|end)\s*\{\s*([A-Za-z]+\*?)\s*\}/y;
  for (let i = 0; i < tex.length; i++) {
    if (tex[i] === '%' && (!textContext || stack.length)) {
      while (i < tex.length && !/[\r\n]/.test(tex[i])) i++;
      continue;
    }
    if (tex[i] !== '\\') continue;
    const verb = verbEnd(tex, i);
    if (verb !== i) { i = verb; continue; }
    token.lastIndex = i;
    const match = token.exec(tex);
    if (!match) { i++; continue; }
    const [, command, name] = match;
    if (command === 'begin') {
      if (stack.length >= 64) return []; // 비정상적으로 깊은 입력은 호환 변환 없이 KaTeX의 오류 처리로
      stack.push({ name, start: i, bodyStart: token.lastIndex, children: [] });
    } else {
      const node = stack.pop();
      if (!node || node.name !== name) { recover([...stack, node]); stack.length = 0; }
      else {
        node.bodyEnd = i;
        node.end = token.lastIndex;
        if (stack.length) stack[stack.length - 1].children.push(node);
        else if (includeUnknown || ENVIRONMENTS.has(name)) roots.push(node);
      }
    }
    i = token.lastIndex - 1;
  }
  recover(stack);
  return roots.sort((a, b) => a.start - b.start);
}

// 미지원 환경의 내용·정렬점·명시적 tag를 보존한다. multline의 양끝 정렬은 가운데 정렬로,
// subequations의 자동 (1a)/(1b) 번호 묶음은 일반 수식 묶음으로 표시한다. 번호를 추측하지 않는다.
export function compatibleEnvironments(tex) {
  const rewrite = node => {
    let body = '';
    let cursor = node.bodyStart;
    const subequations = /^subequations?$/.test(node.name);
    for (const [index, child] of node.children.entries()) {
      const gap = tex.slice(cursor, child.start);
      body += subequations && index > 0 && !gap.replace(/%[^\r\n]*/g, '').trim() ? gap + '\\\\\n' : gap;
      body += rewrite(child);
      cursor = child.end;
    }
    body += tex.slice(cursor, node.bodyEnd);
    let name = node.name;
    if (name.startsWith('flalign')) name = name.replace('flalign', 'align');
    if (name.startsWith('eqnarray')) name = name.replace('eqnarray', 'align');
    if (name === 'multlined') name = 'gathered';
    if (subequations) name = 'align*';
    if (/^multi?line\*?$/.test(name)) {
      const equation = name.endsWith('*') ? 'equation*' : 'equation';
      return `\\begin{${equation}}\\begin{gathered}${body}\\end{gathered}\\end{${equation}}`;
    }
    return `\\begin{${name}}${body}\\end{${name}}`;
  };
  let result = '';
  let cursor = 0;
  for (const node of mathEnvironments(tex)) {
    result += tex.slice(cursor, node.start) + rewrite(node);
    cursor = node.end;
  }
  return result + tex.slice(cursor);
}
