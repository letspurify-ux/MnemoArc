import { useLayoutEffect, useRef, useState } from "react";
import { displayPath, displayPathText } from "./paths.js";

export default function NewSession({ project, projects, onCreate, onClose }) {
  const dialog = useRef(null),
    lock = useRef(false);
  const [chosen, setChosen] = useState(project);
  const [workflow, setWorkflow] = useState("answer");
  const [output, setOutput] = useState(project.output);
  const [saving, setSaving] = useState(false),
    [error, setError] = useState("");
  const options = [
    project,
    ...projects.filter((item) => item.id !== project.id),
  ];
  useLayoutEffect(() => {
    const opener = document.activeElement;
    const element = dialog.current;
    element.showModal();
    return () => {
      element.close();
      if (opener?.isConnected) opener.focus();
    };
  }, []);
  async function submit(event) {
    event.preventDefault();
    if (lock.current) return;
    lock.current = true;
    setSaving(true);
    setError("");
    try {
      await onCreate({ ...chosen, output: output.trim() }, workflow);
    } catch (failure) {
      setError(failure.message);
    } finally {
      lock.current = false;
      setSaving(false);
    }
  }
  return (
    <dialog
      ref={dialog}
      className="modal new-session-modal"
      aria-label="새 세션 설정"
      tabIndex={-1}
      onKeyDown={(event) => {
        if (event.key !== "Tab") return;
        const controls = [
          ...dialog.current.querySelectorAll(
            "button:not(:disabled), input:not(:disabled), select:not(:disabled)",
          ),
        ];
        const first = controls[0],
          last = controls.at(-1);
        if (!first) {
          event.preventDefault();
          dialog.current.focus();
        } else if (
          event.shiftKey &&
          (document.activeElement === first ||
            document.activeElement === dialog.current)
        ) {
          event.preventDefault();
          last.focus();
        } else if (!event.shiftKey && document.activeElement === last) {
          event.preventDefault();
          first.focus();
        }
      }}
      onCancel={(event) => {
        event.preventDefault();
        if (!lock.current) onClose();
      }}
    >
      <form onSubmit={submit}>
        <div className="modal-head">
          <h2>새 세션</h2>
          <button
            type="button"
            aria-label="새 세션 설정 닫기"
            disabled={saving}
            onClick={onClose}
          >
            ×
          </button>
        </div>
        <label className="field">
          프로젝트
          <select
            value={chosen.id}
            disabled={saving}
            onChange={(event) => {
              const next = options.find(
                (item) => item.id === event.target.value,
              );
              setChosen(next);
              setOutput(next.output);
            }}
          >
            {options.map((item) => (
              <option key={item.id} value={item.id}>
                {item.name}
              </option>
            ))}
          </select>
        </label>
        <fieldset className="session-workflow-options" disabled={saving}>
          <legend>작업 방식</legend>
          <label>
            <input
              type="radio"
              name="workflow"
              value="answer"
              checked={workflow === "answer"}
              onChange={() => setWorkflow("answer")}
            />
            <span>
              <strong>일반 작업</strong>
              <small>질문 답변 · 파일 수정 · 문서 작성</small>
            </span>
          </label>
          <label>
            <input
              type="radio"
              name="workflow"
              value="source_document"
              checked={workflow === "source_document"}
              onChange={() => setWorkflow("source_document")}
            />
            <span>
              <strong>소스 기반 문서 작성</strong>
              <small>소스 조사 · 문서 작성 · 근거 검증</small>
            </span>
          </label>
        </fieldset>
        <label className="field">
          결과 문서
          <input
            value={displayPath(output)}
            required
            disabled={saving}
            onChange={(event) => setOutput(event.target.value)}
          />
        </label>
        <p className="subtle">
          프로젝트의 결과 문서 경로를 기본으로 사용합니다. 필요하면 수정하세요.
          작업 방식은 이 세션 동안 유지됩니다.
        </p>
        {error && (
          <p role="alert" className="inline-error">
            {displayPathText(error)}
          </p>
        )}
        <div className="session-create-actions">
          <button type="button" disabled={saving} onClick={onClose}>
            취소
          </button>
          <button className="primary" disabled={saving || !output.trim()}>
            {saving ? "세션 생성 중…" : "세션 시작"}
          </button>
        </div>
      </form>
    </dialog>
  );
}
