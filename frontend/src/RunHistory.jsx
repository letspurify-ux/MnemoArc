import { statusLabel, workflowOptions } from "./api.js";

const reasons = {
  complete: "정상 완료",
  complete_with_gaps: "미확인 항목을 남기고 완료",
  cancelled: "실행 중지",
  run_budget_exhausted: "토큰 예산 부족",
  run_timeout: "실행 시간 한도 도달",
  closing_round_limit: "마감 단계 요청 한도 도달",
  budget: "남은 예산에 맞춰 마감",
  stall: "진행 정체로 마감",
  review_unrepaired: "검토 지적 미반영으로 마감",
  model_worker_panic: "모델 처리 오류",
  agent_worker_panic: "작업 처리 오류",
  checkpoint_retry_limit: "기억 정리 재시도 한도 도달",
  context_limit: "컨텍스트 한도 초과",
};
const stages = {
  question: "기존 작업 질문",
  preparing: "요청 준비",
  model: "모델 응답",
  tools: "도구 실행",
  document_review: "문서 검토",
  answer_review: "답변 검토",
  completion_review: "완료 조건 검증",
  checkpoint: "기억 정리",
  continuing: "답변 이어 쓰기",
};

export default function RunHistory({ records = [] }) {
  if (!records.length) return null;
  return (
    <section className="run-history" aria-label="실행 기록">
      <h4>실행 기록</h4>
      <p className="subtle">
        최근 20회 · 다음 질문 후에도 유지됩니다. 세션을 닫거나 앱을 종료하면
        사라집니다.
      </p>
      {[...records].reverse().map((run) => (
        <details key={run.id} className="run-record">
          <summary>
            <strong>
              {reasons[run.reason] || statusLabel[run.status] || run.status}
            </strong>
            <time dateTime={run.ended_at}>
              {new Date(run.ended_at).toLocaleString("ko-KR")}
            </time>
            <span className="run-request">{run.request || "실행"}</span>
          </summary>
          <dl>
            <dt>결과</dt>
            <dd>{statusLabel[run.status] || run.status}</dd>
            <dt>작업 방식</dt>
            <dd>
              {(run.workflow === "follow_up"
                ? "기존 작업 질문"
                : workflowOptions.find(([id]) => id === run.workflow)?.[1]) ||
                run.workflow}
            </dd>
            <dt>시작 시각</dt>
            <dd>{new Date(run.started_at).toLocaleString("ko-KR")}</dd>
            <dt>소요 시간</dt>
            <dd>{(run.elapsed_ms / 1000).toFixed(1)}초</dd>
            <dt>마지막 단계</dt>
            <dd>{stages[run.last_stage] || run.last_stage}</dd>
            <dt>모델 호출</dt>
            <dd>{run.rounds}회</dd>
            <dt>입력 토큰</dt>
            <dd>{run.input_tokens.toLocaleString()}</dd>
            <dt>출력 토큰</dt>
            <dd>{run.output_tokens.toLocaleString()}</dd>
            <dt>토큰 한도</dt>
            <dd>{run.token_limit.toLocaleString()}</dd>
            <dt>시간 한도</dt>
            <dd>{run.timeout_secs.toLocaleString()}초</dd>
          </dl>
          {run.usage_estimated && (
            <p className="subtle">사용량에 추정치가 포함되어 있습니다.</p>
          )}
          {run.checkpoint_pending && (
            <p>기억 정리를 완료하기 전에 종료됐습니다.</p>
          )}
          <pre className="run-error">{run.error || run.reason}</pre>
        </details>
      ))}
    </section>
  );
}
