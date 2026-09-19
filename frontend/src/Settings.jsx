import { useEffect, useState } from "react";
import { groups, inputValue, parseValue } from "./fields.js";
import { api, send } from "./api.js";

export function DirectoryPicker({ path, onSelect, onClose }) {
  const [data, setData] = useState(null),
    [error, setError] = useState("");
  const load = async (path) => {
    try {
      setData(
        await api(
          `/directories${path ? `?path=${encodeURIComponent(path)}` : ""}`,
        ),
      );
      setError("");
    } catch (e) {
      setError(e.message);
    }
  };
  useEffect(() => {
    void load(path);
  }, []);
  return (
    <div className="modal-backdrop">
      <section
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-label="프로젝트 폴더 선택"
      >
        <div className="modal-head">
          <h2>프로젝트 폴더 선택</h2>
          <button aria-label="폴더 선택 닫기" onClick={onClose}>
            ×
          </button>
        </div>
        <p className="directory-path">{data?.path || path}</p>
        {error && (
          <p role="alert" className="inline-error">
            {error}
          </p>
        )}
        <div className="directory-list">
          {data?.parent && (
            <button onClick={() => load(data.parent)}>↑ 상위 폴더</button>
          )}
          {data?.directories.map((d) => (
            <button key={d.path} onClick={() => load(d.path)}>
              <span>▱</span>
              {d.name}
              <span>›</span>
            </button>
          ))}
        </div>
        <button
          className="primary"
          disabled={!data}
          onClick={() => onSelect(data.path)}
        >
          이 폴더 선택
        </button>
      </section>
    </div>
  );
}
export function ProjectForm({ project, onChange, disabled = false }) {
  const [browse, setBrowse] = useState(false);
  const set = (key, value) => onChange({ ...project, [key]: value });
  return (
    <div className="project-fields">
      <label>
        프로젝트 이름
        <input
          value={project.name}
          disabled={disabled}
          onChange={(e) => set("name", e.target.value)}
        />
      </label>
      <label>
        소스 폴더
        <div className="input-action">
          <input
            value={project.root}
            disabled={disabled}
            onChange={(e) => set("root", e.target.value)}
          />
          <button
            type="button"
            disabled={disabled}
            onClick={() => setBrowse(true)}
          >
            폴더 선택
          </button>
        </div>
      </label>
      <label>
        결과 문서 경로
        <input
          value={project.output}
          disabled={disabled}
          onChange={(e) => set("output", e.target.value)}
        />
        <small>프로젝트 폴더 기준 상대 경로 또는 절대 경로</small>
      </label>
      <label>
        작업 목적
        <textarea
          rows={3}
          value={project.purpose}
          disabled={disabled}
          onChange={(e) => set("purpose", e.target.value)}
        />
      </label>
      <label>
        문서 독자
        <input
          value={project.audience}
          disabled={disabled}
          onChange={(e) => set("audience", e.target.value)}
        />
      </label>
      {["include", "exclude"].map((key) => (
        <label key={key}>
          {key === "include" ? "포함할 파일 패턴" : "제외할 파일 패턴"}
          <textarea
            rows={3}
            disabled={disabled}
            placeholder={
              key === "include" ? "비우면 전체 포함" : "예: secrets/**\n.env*"
            }
            value={project[key].join("\n")}
            onChange={(e) => set(key, e.target.value.split("\n"))}
          />
          <small>한 줄에 한 패턴. 예: src/**, **/*.rs</small>
        </label>
      ))}
      {browse && (
        <DirectoryPicker
          path={project.root}
          onClose={() => setBrowse(false)}
          onSelect={(path) => {
            set("root", path);
            setBrowse(false);
          }}
        />
      )}
    </div>
  );
}
export function cleanProject(project) {
  return {
    ...project,
    include: project.include.filter((s) => s.trim()),
    exclude: project.exclude.filter((s) => s.trim()),
  };
}
export default function Settings({
  config,
  credential,
  sessionCredential,
  sessionConfig,
  sessionId,
  onSaved,
  onClose,
}) {
  const [scope, setScope] = useState("global"),
    [draft, setDraft] = useState(() => structuredClone(config)),
    [tab, setTab] = useState("connection");
  const [key, setKey] = useState(""),
    [remember, setRemember] = useState(false),
    [clear, setClear] = useState(false),
    [apply, setApply] = useState(true),
    [busy, setBusy] = useState(false),
    [message, setMessage] = useState(""),
    [error, setError] = useState("");
  const [dirty, setDirty] = useState(false);
  const group = groups.find((g) => g.id === tab);
  const change = (name, value) => {
    setDraft((d) => ({ ...d, [name]: value }));
    setDirty(true);
    setMessage("");
  };
  const payload = () => ({
    config: draft,
    api_key: key || null,
    credential_mode: clear
      ? "clear"
      : key
        ? remember && scope === "global"
          ? "save"
          : "session"
        : "keep",
  });
  async function save() {
    setBusy(true);
    setError("");
    try {
      const data = payload();
      const result = await send(
        scope === "global" ? "/settings" : `/sessions/${sessionId}/settings`,
        data,
        "PUT",
      );
      let pending = result.pending;
      if (scope === "global" && apply && sessionId) {
        const res = await send(
          `/sessions/${sessionId}/settings`,
          {
            ...data,
            credential_mode: key ? "session" : clear ? "clear" : "keep",
          },
          "PUT",
        );
        pending = res.pending;
      }
      setMessage(
        pending
          ? "저장했습니다. 현재 요청이 끝나거나 기억 정리가 완료되면 적용됩니다."
          : "설정을 저장했습니다.",
      );
      setDirty(false);
      setKey("");
      setClear(false);
      await onSaved();
    } catch (e) {
      setError(e.message);
    } finally {
      setBusy(false);
    }
  }
  async function check() {
    setBusy(true);
    setError("");
    setMessage("일반 응답, 스트리밍, 도구 왕복을 확인하고 있습니다…");
    try {
      await send("/check", payload());
      setMessage("연결 확인 완료 · 응답, 스트리밍, 도구 호출이 정상입니다.");
    } catch (e) {
      setError(e.message);
      setMessage("");
    } finally {
      setBusy(false);
    }
  }
  function switchScope(value) {
    if (dirty && !confirm("저장하지 않은 설정을 버리고 전환할까요?")) return;
    setScope(value);
    setDraft(structuredClone(value === "global" ? config : sessionConfig));
    setDirty(false);
    setKey("");
    setClear(false);
    setMessage("");
    setError("");
  }
  return (
    <section className="settings-page">
      <header className="page-heading">
        <div>
          <span className="eyebrow">PREFERENCES</span>
          <h1>설정</h1>
          <p>연결부터 기억·작업 예산까지, 이곳에서 조정하세요.</p>
        </div>
        <button
          className="secondary"
          onClick={() => {
            if (!dirty || confirm("저장하지 않은 설정을 버리고 돌아갈까요?"))
              onClose();
          }}
        >
          채팅으로 돌아가기
        </button>
      </header>
      <div className="settings-toolbar">
        <label>
          적용 범위
          <select
            aria-label="설정 적용 범위"
            value={scope}
            onChange={(e) => switchScope(e.target.value)}
          >
            <option value="global">전체 기본 설정 · 파일에 저장</option>
            {sessionId && (
              <option value="session">현재 세션만 · 임시 적용</option>
            )}
          </select>
        </label>
        {scope === "global" && sessionId && (
          <label className="check-label">
            <input
              type="checkbox"
              checked={apply}
              onChange={(e) => setApply(e.target.checked)}
            />
            현재 세션에도 적용
          </label>
        )}
      </div>
      <div className="settings-layout">
        <nav className="settings-tabs" aria-label="설정 분류">
          {groups.map((g) => (
            <button
              key={g.id}
              className={tab === g.id ? "active" : ""}
              onClick={() => setTab(g.id)}
            >
              {g.label}
              <span>›</span>
            </button>
          ))}
        </nav>
        <div className="settings-body">
          <h2>{group.label}</h2>
          <p className="subtle">{group.description}</p>
          {tab === "connection" && (
            <div className="credential-card">
              <div className="credential-title">
                <h3>API 키</h3>
                <span className="badge">
                  {(
                    scope === "session"
                      ? sessionCredential
                      : credential?.configured
                  )
                    ? "등록됨"
                    : "미등록"}
                </span>
              </div>
              <label>
                새 API 키
                <input
                  type="password"
                  autoComplete="new-password"
                  placeholder="변경할 때만 입력하세요"
                  aria-label="API 키"
                  value={key}
                  onChange={(e) => {
                    setKey(e.target.value);
                    setClear(false);
                    setDirty(true);
                  }}
                />
              </label>
              <div className="credential-options">
                {scope === "global" && (
                  <label className="check-label">
                    <input
                      type="checkbox"
                      checked={remember}
                      onChange={(e) => setRemember(e.target.checked)}
                    />
                    이 기기에 키 저장
                  </label>
                )}
                <button
                  type="button"
                  className="text-button"
                  onClick={() => {
                    setClear(true);
                    setKey("");
                    setDirty(true);
                  }}
                >
                  입력·저장한 키 지우기
                </button>
              </div>
              <small>
                {clear
                  ? "저장하면 앱에 등록한 키를 지웁니다. 환경변수의 키는 계속 사용할 수 있습니다."
                  : remember
                    ? "키는 접근을 제한한 별도 설정 파일에 저장되며 화면에 다시 표시하지 않습니다."
                    : "직접 입력한 키는 앱 종료 시 폐기합니다. 환경변수 이름은 아래에서 설정할 수 있습니다."}
              </small>
            </div>
          )}
          <div className="field-grid">
            {group.fields.map(([name, label, type, help]) => (
              <label
                className={`setting-field ${type === "boolean" ? "boolean-field" : ""}`}
                key={name}
                htmlFor={`setting-${name}`}
              >
                <span>{label}</span>
                {type === "boolean" ? (
                  <input
                    id={`setting-${name}`}
                    aria-label={label}
                    type="checkbox"
                    checked={draft[name]}
                    onChange={(e) => change(name, e.target.checked)}
                  />
                ) : (
                  <input
                    id={`setting-${name}`}
                    aria-label={label}
                    type={
                      [
                        "number",
                        "optionalNumber",
                        "kib",
                        "mib",
                        "percent",
                      ].includes(type)
                        ? "number"
                        : type === "url"
                          ? "url"
                          : "text"
                    }
                    min="0"
                    step={
                      type === "percent" || type === "mib" || type === "kib"
                        ? "any"
                        : "1"
                    }
                    value={inputValue(draft[name], type)}
                    placeholder={
                      type.startsWith("optional") ? "사용 안 함" : ""
                    }
                    onChange={(e) =>
                      change(name, parseValue(e.target.value, type))
                    }
                  />
                )}
                <small>{help}</small>
              </label>
            ))}
          </div>
          {error && (
            <div className="inline-error" role="alert">
              {error}
            </div>
          )}
          {message && (
            <div className="success-message" role="status">
              {message}
            </div>
          )}
          <footer className="settings-actions">
            {tab === "connection" && (
              <button className="secondary" disabled={busy} onClick={check}>
                연결 확인
              </button>
            )}
            <span>{dirty ? "저장하지 않은 변경 사항" : ""}</span>
            <button className="primary" disabled={busy} onClick={save}>
              {busy ? "처리 중…" : "설정 저장"}
            </button>
          </footer>
        </div>
      </div>
    </section>
  );
}
