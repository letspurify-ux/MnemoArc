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
  partial: "추가 확인 필요",
  blocked: "확인 필요",
  cancelled: "중지됨",
};
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
  capability_inventory: "화면·서버 기능 목록",
  documentation_coverage: "기능 문서 누락 점검",
  task_plan: "할 일 목록 관리",
  history: "원문 조회",
  source_lookup: "기존 출처 조회",
  checkpoint_complete: "체크포인트 확인",
  tool_catalog: "도구 조회",
  tool_select: "도구 선택",
};

export function sessionErrorLabel(error = "") {
  if (error.startsWith("documentation_coverage_pending:"))
    return "기능 목록과 문서에 확인할 항목이 남아 있어 보완 작업을 진행합니다.";
  if (error.startsWith("documentation_coverage_no_progress:"))
    return "같은 누락 항목이 해결되지 않아 부분 결과와 보완 목록을 보존했습니다. 진행 패널의 항목을 확인한 뒤 이어서 진행할 수 있습니다.";
  if (error.startsWith("documentation_coverage_audit:"))
    return "기능 문서 점검을 마치지 못했습니다. 작업 기록은 보존되어 있으며 도구 결과를 확인한 뒤 재개할 수 있습니다.";
  return error;
}
