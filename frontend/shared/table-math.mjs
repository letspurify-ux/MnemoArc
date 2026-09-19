import { texGroupEnds } from './tex-environments.mjs';

// 완성한 수식은 호출자가 먼저 보호한다. 이 함수는 닫는 표시가 없는 명확한 TeX 입력만
// 표의 원문 셀에 가둔다. 수식의 빠진 내용이나 괄호를 추측해서 채우지 않는다.
export function incompleteTableMath(source, rows, protectedRanges, completed) {
  const occupied = [...protectedRanges, ...completed].sort((a, b) => a.start - b.start);
  const result = [];
  let groups;
  const inArgument = (span, start, end) => {
    groups ??= texGroupEnds(source);
    for (const [open, close] of groups)
      if (open >= start && open < span.start && close >= span.end && close < end) return true;
    return false;
  };
  let rangeIndex = 0;
  for (const row of rows) {
    while (rangeIndex < occupied.length && occupied[rangeIndex].end <= row.start) rangeIndex++;
    const chars = source.slice(row.start, row.end).split('');
    for (let n = rangeIndex; n < occupied.length && occupied[n].start < row.end; n++) {
      const range = occupied[n];
      chars.fill(' ', Math.max(0, range.start - row.start), Math.min(chars.length, range.end - row.start));
    }
    const masked = chars.join('');
    const pipes = [];
    for (let i = 0; i < masked.length; i++) {
      if (masked[i] === '\\') { i++; continue; }
      if (masked[i] === '|') pipes.push(i);
    }
    let end = masked.length;
    // 행의 외곽 파이프는 열 경계 수에 넣지 않는다. 보호한 수식을 빈 셀로 오인하지 않게 원문을 본다.
    if (pipes.length && !source.slice(row.start, row.start + pipes[0]).trim()) pipes.shift();
    if (pipes.length && !source.slice(row.start + pipes[pipes.length - 1] + 1, row.end).trim()) end = pipes.pop();
    const open = /\${1,2}(?!\$)|\\[([]/g;
    for (const match of masked.matchAll(open)) {
      const start = match.index;
      if (result.length && row.start + start < result[result.length - 1].end) continue;
      let slashes = 0;
      for (let i = start - 1; i >= 0 && masked[i] === '\\'; i--) slashes++;
      if (slashes % 2 || /[A-Za-z0-9$]/.test(masked[start - 1] ?? '')) continue;
      const firstPipe = pipes.find(i => i > start) ?? end;
      const head = masked.slice(start + match[0].length, firstPipe);
      // 통화·환경변수·단순히 닫히지 않은 $를 수식으로 단정하지 않는다.
      if (/^[A-Z_][A-Z0-9_]*(?:\/\S*)?$/.test(head.trim())) continue;
      if (!/\\[A-Za-z]+|&[A-Za-z]+;?|[=^_]/.test(head)) continue;
      const column = pipes.filter(i => i < start).length;
      const after = pipes.filter(i => i > start);
      const remaining = Math.max(0, row.width - column - 1);
      // 절댓값 |가 늘린 가짜 열을 이 셀의 원문으로 되돌린다.
      let cellEnd = remaining > 0 && after.length >= remaining ? after[after.length - remaining] : end;
      // 다음 수식의 시작을 알면 그 경계를 넘어 원문을 가져오지 않는다.
      const next = /\$[ \t]*(?:\\[A-Za-z]+|[A-Za-z][^|\r\n]*[=^_])|\\[([]/.exec(masked.slice(start + match[0].length));
      // 완성된 수식·코드·주소는 위에서 가렸으므로 masked에서는 다시 찾을 수 없다.
      // 아직 덜 받은 행의 열 수를 채우려고 그 원자를 앞 오류에 포함하지 않는다.
      // TeX 인자 안의 리터럴은 그 인자가 소유하므로 독립된 이웃으로 취급하지 않는다.
      const ownedStart = occupied.reduce((nearest, span) => span.start > row.start + start && span.start < row.end &&
        !inArgument(span, row.start + start, row.end) ? Math.min(nearest, span.start - row.start) : nearest, Infinity);
      const nextStart = Math.min(ownedStart, next ? start + match[0].length + next.index : Infinity);
      if (nextStart < Infinity) {
        const boundary = after.filter(i => i < nextStart).pop();
        if (boundary !== undefined) cellEnd = Math.min(cellEnd, boundary);
      }
      while (cellEnd > start && /[ \t]/.test(source[row.start + cellEnd - 1])) cellEnd--;
      if (cellEnd <= start) continue;
      result.push({ start: row.start + start, end: row.start + cellEnd,
        value: source.slice(row.start + start + match[0].length, row.start + cellEnd), incomplete: true });
    }
  }
  return result;
}
