import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { Message, StreamingMessage } from "./chat/Message.jsx";
import { continuationMessages } from "./chat/continuation.js";
import { toolLabels, sessionErrorLabel } from "./api.js";

export default function Chat({
  session,
  busy,
  canRun,
  onSend,
  onCancel,
  onSettings,
  onOlder,
}) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (session?.status !== "running") return;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [session?.id, session?.status]);
  const elapsed = Math.max(
    0,
    Math.floor((now - (session?.activity?.started_at_ms || now)) / 1000),
  );
  const stage = session?.activity?.stage;
  const progress =
    stage === "completion_review"
      ? "실제 결과와 완료 조건 확인 중"
      : stage === "document_review"
      ? "문서와 소스 근거 대조 중"
      : stage === "answer_review"
      ? "소스 근거와 답변 대조 중"
      : stage === "tools"
      ? `${(session.activity.tools || []).map((name) => toolLabels[name] || name).join(" · ")} 실행 중`
      : session?.stream
        ? "답변 생성 중"
        : stage === "model"
          ? session?.continuation_pending
            ? "길이 제한으로 이어서 생성 중"
            : "모델 응답 대기 중"
          : stage === "continuing"
            ? "받은 답변을 보존하고 이어서 생성 중"
            : "요청 준비 중";
  const [input, setInput] = useState("");
  const [sending, setSending] = useState(false);
  const inputRef = useRef(null),
    lock = useRef(false),
    composing = useRef(false),
    chatRef = useRef(null),
    bottom = useRef(true);
  const fitInput = () => {
    const el = inputRef.current;
    if (!el) return;
    el.style.height = "auto";
    if (el.value) el.style.height = `${Math.min(el.scrollHeight, 180)}px`;
    el.style.overflowY = el.scrollHeight > 180 ? "auto" : "hidden";
  };
  useLayoutEffect(fitInput, [input]);
  useEffect(() => {
    addEventListener("resize", fitInput);
    return () => removeEventListener("resize", fitInput);
  }, []);
  useLayoutEffect(() => {
    const el = chatRef.current;
    if (el && bottom.current) el.scrollTop = el.scrollHeight;
  }, [session?.bundles, session?.stream]);
  useEffect(() => {
    const el = chatRef.current;
    if (!el || !globalThis.ResizeObserver) return;
    const ro = new ResizeObserver(() => {
      if (bottom.current) el.scrollTop = el.scrollHeight;
    });
    if (el.firstChild) ro.observe(el.firstChild);
    return () => ro.disconnect();
  }, [session?.id]);
  async function submit(text = input) {
    const message = text.trim();
    if (!message || busy || !canRun || lock.current) return;
    lock.current = true;
    setSending(true);
    try {
      await onSend(message);
      setInput("");
      bottom.current = true;
      inputRef.current?.focus();
    } finally {
      lock.current = false;
      setSending(false);
    }
  }
  const { messages, streamText } = continuationMessages(
    session?.bundles,
    session?.stream,
    session?.continuation_pending,
  );
  const haveText = messages.some(
    (m) => (m.role === "user" || m.role === "assistant") && m.content,
  );
  return (
    <section className="conversation" aria-label="채팅">
      <div
        className="chat-scroll"
        ref={chatRef}
        onScroll={(e) => {
          const el = e.currentTarget;
          bottom.current =
            el.scrollHeight - el.scrollTop - el.clientHeight < 60;
        }}
      >
        <div className="chat-content">
          {session?.previous && (
            <button
              className="text-button older"
              onClick={() => {
                bottom.current = false;
                onOlder();
              }}
            >
              이전 대화 더 보기
            </button>
          )}
          {session?.pruned_through && (
            <p className="subtle">
              오래된 원문 일부가 보관 한도에 따라 정리되었습니다. 저장된 기억은
              유지됩니다.
            </p>
          )}
          {!haveText && !session?.stream && (
            <div className="welcome">
              <div className="welcome-mark">
                m<span>·</span>
              </div>
              <span className="eyebrow">YOUR PROJECT, REMEMBERED</span>
              <h1>
                맥락을 기억하고,
                <br />
                작업을 이어갑니다.
              </h1>
              <p>
                프로젝트를 조사하고 근거를 기억하며
                <br />
                함께 문서를 완성해 보세요.
              </p>
              <div className="suggestions">
                {[
                  "프로젝트 구조와 주요 실행 흐름을 문서로 정리해줘",
                  "핵심 데이터 구조와 오류 처리를 조사해줘",
                  "지금까지의 발견과 미확인 사항을 알려줘",
                ].map((text, i) => (
                  <button key={text} onClick={() => setInput(text)}>
                    <span>0{i + 1}</span>
                    {text}
                    <b>↗</b>
                  </button>
                ))}
              </div>
            </div>
          )}
          {messages.map((m) =>
            m.role === "tool" ? (
              <ToolResult key={m.key} message={m} />
            ) : m.role === "assistant" && m.tool_calls?.length ? (
              <div className="tool-group" key={m.key}>
                {m.content && <Message role="assistant" text={m.content} />}
                <span className="tool-label">
                  {m.tool_calls
                    .map(
                      (c) => toolLabels[c.function?.name] || c.function?.name,
                    )
                    .join(" · ")}
                </span>
              </div>
            ) : m.content && ["user", "assistant"].includes(m.role) ? (
              <Message key={m.key} role={m.role} text={m.content} />
            ) : null,
          )}
          {streamText && <StreamingMessage text={streamText} />}
          {session?.status === "running" && (
            <div className="thinking" role="status">
              <span className="pulse" />
              {progress} · {elapsed}초
              {session?.activity?.round
                ? ` · ${session.activity.round}번째 모델 호출`
                : ""}
            </div>
          )}
          {session?.error && (
            <div
              className={session.error.startsWith("documentation_coverage_pending:") ? "setup-note" : "inline-error"}
              role={session.error.startsWith("documentation_coverage_pending:") ? "status" : "alert"}
            >
              {sessionErrorLabel(session.error)}
            </div>
          )}
        </div>
      </div>
      <div className="composer-wrap">
        {!canRun && (
          <div className="setup-note">
            <span>먼저 모델과 컨텍스트 한도를 설정하세요.</span>
            <button onClick={onSettings}>연결 설정 열기 →</button>
          </div>
        )}
        <form
          className="composer"
          onSubmit={(e) => {
            e.preventDefault();
            if (!composing.current) void submit().catch(() => {});
          }}
        >
          <textarea
            ref={inputRef}
            aria-label="메시지"
            placeholder="프로젝트에 대해 요청해 보세요…"
            rows={1}
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onCompositionStart={() => {
              composing.current = true;
            }}
            onCompositionEnd={() => {
              composing.current = false;
            }}
            onKeyDown={(e) => {
              if (
                e.key === "Enter" &&
                !e.shiftKey &&
                !e.altKey &&
                !e.nativeEvent.isComposing &&
                !composing.current &&
                e.keyCode !== 229
              ) {
                e.preventDefault();
                void submit().catch(() => {});
              }
            }}
          />
          <div className="composer-bottom">
            <span>
              <i className="small-dot" />
              세션 기억 사용 · Enter 전송 / Shift+Enter 줄바꿈
            </span>
            {session?.status === "running" ? (
              <button type="button" className="stop-button" onClick={onCancel}>
                ■ 중지
              </button>
            ) : (
              <button
                className="send-button"
                type="submit"
                aria-label="메시지 보내기"
                disabled={!input.trim() || busy || sending || !canRun}
              >
                ↑
              </button>
            )}
          </div>
        </form>
        <p className="composer-footnote">
          결과 문서는 프로젝트에 저장됩니다. 기억과 대화는 앱 실행 중에만
          유지됩니다.
        </p>
      </div>
    </section>
  );
}
function ToolResult({ message }) {
  let result;
  try {
    result = JSON.parse(message.content);
  } catch {
    result = { status: "unknown", data: message.content };
  }
  return (
    <details
      className={`tool-result ${result.status === "ok" ? "" : "failed"}`}
    >
      <summary>
        <span>{result.status === "ok" ? "✓" : "!"}</span> 도구 결과{" "}
        <small>{result.status}</small>
      </summary>
      <pre>{JSON.stringify(result, null, 2)}</pre>
    </details>
  );
}
