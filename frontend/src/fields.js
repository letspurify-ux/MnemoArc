// Every serialized Config field is represented here; projects have a dedicated editor.
export const groups = [
  {
    id: "connection",
    label: "모델 연결",
    description: "응답을 생성할 서버와 모델을 연결합니다.",
    fields: [
      [
        "base_url",
        "API 주소",
        "url",
        "OpenAI 호환 서버의 기본 주소입니다. /v1까지 입력하세요.",
      ],
      ["model", "모델 이름", "text", "서버에서 사용하는 정확한 모델 ID"],
      [
        "api_key_env",
        "API 키 환경변수",
        "text",
        "직접 입력한 키가 없으면 이 환경변수 또는 .env에서 읽습니다.",
      ],
      ["proxy", "프록시 주소", "optional", "사용하지 않으면 비워 두세요."],
      [
        "model_context",
        "모델 최대 컨텍스트",
        "optionalNumber",
        "모델이 지원하는 입력 + 출력 토큰 한도",
      ],
      [
        "context_tokens",
        "요청 컨텍스트 예산",
        "number",
        "모델 최대 한도 안에서 사용합니다.",
      ],
      [
        "output_tokens",
        "요청 출력 한도",
        "number",
        "한 번의 응답에 사용할 최대 토큰",
      ],
      [
        "reasoning_effort",
        "추론 강도",
        "optional",
        "서버가 지원하는 값 (예: low, medium, high). 미지원이면 비워 두세요.",
      ],
      [
        "legacy_max_tokens",
        "구형 출력 한도 옵션",
        "boolean",
        "max_completion_tokens 대신 max_tokens 사용",
      ],
      ["source_answer_review", "소스 답변 검토", "boolean", "읽은 소스 근거로 답변을 한 번 검토합니다. 추가 모델 호출 비용이 발생합니다."],
      ["source_document_review", "소스 문서 검토", "boolean", "생성 문서를 소스·요구사항과 별도로 대조합니다. 작업당 최대 두 번의 모델 호출 비용이 발생합니다."],
      [
        "stream_usage",
        "스트리밍 사용량 요청",
        "boolean",
        "지원하지 않는 서버에서는 끄세요.",
      ],
    ],
  },
  {
    id: "memory",
    label: "기억과 보관",
    description: "기억과 원문을 얼마나 보관하고 보여줄지 정합니다.",
    fields: [
      ["memory_count", "최대 기억 개수", "number", "세션마다 적용됩니다."],
      [
        "recent_count",
        "최신 기억 목록",
        "number",
        "매 요청에 포함할 메타데이터 개수",
      ],
      [
        "related_count",
        "관련 기억 후보",
        "number",
        "키워드로 찾을 후보의 최대 개수",
      ],
      [
        "memory_reuse",
        "기억 검색·재사용",
        "boolean",
        "끄면 평가용으로 기억 재사용을 제한합니다.",
      ],
      [
        "memory_body_bytes",
        "기억 본문 한도",
        "kib",
        "기억 한 개의 본문 크기 (KiB)",
      ],
      [
        "memory_bytes",
        "전체 기억 보관량",
        "mib",
        "세션당 기억·출처 크기 (MiB)",
      ],
      ["history_bytes", "원문 보관량", "mib", "세션당 원문 기록 크기 (MiB)"],
      [
        "state_tokens",
        "목표·진행 상태 한도",
        "number",
        "상시 제공하는 상태 토큰",
      ],
      [
        "index_tokens",
        "기억 목록 한도",
        "number",
        "최신 목록은 항목당 최대 160 토큰 + 공통 128 토큰이 필요합니다.",
      ],
      ["result_tokens", "조회 결과 한도", "number", "도구 한 번의 결과 토큰"],
      [
        "batch_tokens",
        "도구 묶음 결과 한도",
        "number",
        "한 응답의 전체 도구 결과 토큰",
      ],
      [
        "checkpoint_tokens",
        "기억 정리 여유",
        "number",
        "컨텍스트 제외 전 정리를 위한 토큰",
      ],
      ["high_water", "정리 시작 비율", "percent", "입력 예산 사용률 (%)"],
      [
        "low_water",
        "정리 후 목표 비율",
        "percent",
        "시작 비율보다 작아야 합니다. (%)",
      ],
    ],
  },
  {
    id: "execution",
    label: "실행과 예산",
    description: "작업 시간, 동시 읽기와 전체 사용량을 제한합니다.",
    fields: [
      [
        "read_parallelism",
        "동시 읽기 개수",
        "number",
        "독립적인 읽기 도구에만 적용합니다.",
      ],
      ["request_timeout_secs", "LLM 요청 시간 제한", "number", "초"],
      ["tool_timeout_secs", "도구 시간 제한", "number", "초"],
      ["retries", "일시 오류 재시도", "number", "추가 시도 횟수"],
      ["run_timeout_secs", "작업 시간 예산", "number", "초"],
      [
        "run_tokens",
        "작업 토큰 예산",
        "number",
        "누적 입력 + 출력. 기억 정리와 재시도를 포함합니다.",
      ],
      [
        "writing_reserve_ratio",
        "작성·검증 예산 비율",
        "percent",
        "이 비율의 실행 예산이 남으면 초안 작성을 우선합니다 (%)",
      ],
      [
        "verification_reserve_ratio",
        "검증 예산 비율",
        "percent",
        "이 비율의 예산이 남으면 미검증 항목을 우선합니다 (작성 비율보다 작게)",
      ],
      [
        "repeated_read_limit",
        "동일 범위 반복 조회 제한",
        "number",
        "활성 대화에 동일 내용이 이 횟수 이상 있으면 본문 재전송을 생략합니다",
      ],
      [
        "stall_round_limit",
        "반복 작업 중단 횟수",
        "number",
        "동일 도구 작업이 진행 없이 반복되면 복구 가능한 중단으로 전환합니다",
      ],
      ["review_limit", "문서 검토 횟수", "number", "최종 검토의 최대 횟수"],
    ],
  },
];
export const fieldKeys = groups.flatMap((g) => g.fields.map((f) => f[0]));
export const unitScale = (type) =>
  type === "kib"
    ? 1024
    : type === "mib"
      ? 1048576
      : type === "percent"
        ? 0.01
        : 1;
export function inputValue(value, type) {
  return value == null
    ? ""
    : ["number", "optionalNumber", "kib", "mib", "percent"].includes(type)
      ? Number((value / unitScale(type)).toFixed(8))
      : value;
}
export function parseValue(value, type) {
  if (type === "boolean") return Boolean(value);
  if (type === "optional" || type === "optionalNumber") {
    if (value === "") return null;
  }
  if (["number", "optionalNumber", "kib", "mib", "percent"].includes(type))
    return value === "" ? "" : Number(value) * unitScale(type);
  return value;
}
