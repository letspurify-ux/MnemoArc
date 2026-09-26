export async function api(path, options = {}) {
  const response = await fetch(`/api${path}`, {
    ...options,
    headers: {
      "Content-Type": "application/json",
      "X-MnemoArc-Client": "web",
      ...options.headers,
    },
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
  });
  const data = await response.json().catch(() => ({
    error: `서버 응답을 읽지 못했습니다 (${response.status}).`,
  }));
  if (!response.ok)
    throw new Error(data.error || `요청 실패 (${response.status})`);
  return data;
}
export const send = (path, body, method = "POST") =>
  api(path, { method, body });
export const statusLabel = {
  idle: "준비됨",
  running: "작업 중",
  complete: "완료",
  completed: "완료",
  complete_with_gaps: "완료 · 미확인 있음",
  partial: "추가 확인 필요",
  blocked: "확인 필요",
  cancelled: "중지됨",
};
// How a session's requests are handled; the user selects it per session.
export const workflowOptions = [
  ["answer", "질문 답변"],
  ["source_document", "소스 기반 문서 작성"],
  ["document_edit", "문서 편집"],
];
export const toolLabels = {
  document_inspect: "문서 구조 조회",
  document_audit: "문서 근거 점검",
  symbol_search: "심볼 검색",
  file_list: "파일 목록",
  file_read: "파일 읽기",
  source_search: "소스 검색",
  document_edit: "문서 편집",
  investigation: "조사·검증",
  memory_write: "기억 저장",
  memory_read: "기억 읽기",
  memory_find: "기억 검색",
  memory_manage: "기억 정리",
  task_state: "목표·진행 관리",
  task_plan: "할 일 목록 관리",
  history: "원문 조회",
  source_lookup: "기존 출처 조회",
  checkpoint_complete: "체크포인트 확인",
  tool_catalog: "도구 조회",
  tool_select: "도구 선택",
};
