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
function DatabaseEditor({ database, onChange }) {
  const set = (key, value) => onChange({ ...database, [key]: value });
  const updateQuery = (index, patch) => set("queries", database.queries.map((q, i) => i === index ? { ...q, ...patch } : q));
  const updateParam = (queryIndex, paramIndex, patch) => {
    const q = database.queries[queryIndex];
    updateQuery(queryIndex, { params: q.params.map((p, i) => i === paramIndex ? { ...p, ...patch } : p) });
  };
  return <div className="project-fields">
    <label className="check-label"><input type="checkbox" checked={database.enabled} onChange={e => set("enabled", e.target.checked)} />DB 도구 전체 활성화</label>
    <small>기본값은 꺼짐입니다. 모델은 이 설정이나 아래 스위치를 변경할 수 없습니다. 전체 스위치와 해당 기능 스위치가 모두 켜져야 도구가 노출됩니다.</small>
    <div className="field-grid">
      {[["host", "호스트", "localhost"], ["port", "포트", "1521"], ["service", "서비스 이름", "FREEPDB1"], ["username", "사용자", "READ_ONLY_USER"], ["password_env", "암호 환경변수 이름", "MNEMOARC_DB_PASSWORD"], ["max_rows", "최대 결과 행", "100"]].map(([key, label, placeholder]) =>
        <label className="setting-field" key={key}><span>{label}</span><input aria-label={label} type={["port", "max_rows"].includes(key) ? "number" : "text"} value={database[key]} placeholder={placeholder} onChange={e => set(key, ["port", "max_rows"].includes(key) ? Number(e.target.value) : e.target.value)} /></label>
      )}
    </div>
    <small>암호 값은 이 화면이나 설정 파일에 저장하지 않습니다. 앱 실행 환경변수 또는 실행 폴더의 .env에 지정하세요. DB 사용자에게 필요한 권한만 부여하세요.</small>
    <h3>자유 실행 도구</h3>
    <small>모델이 실행할 SQL 또는 프로시저·함수 이름과 바인드 값을 직접 지정할 수 있습니다. 변경 SQL과 프로시저·함수는 DB 내용을 바꿀 수 있으며 성공 시 커밋됩니다. 필요한 모드만 사용자가 직접 켜세요.</small>
    {[
      ["raw_query_enabled", "자유 SELECT/WITH 조회", "읽기 전용 트랜잭션과 결과 제한을 적용합니다."],
      ["raw_statement_enabled", "자유 변경 SQL 실행", "INSERT/UPDATE/DELETE/DDL 등을 실행합니다. DDL은 Oracle에서 자체 커밋될 수 있습니다."],
      ["procedure_enabled", "프로시저 호출", "IN/OUT/IN OUT 값과 REF CURSOR 결과를 지원합니다."],
      ["function_enabled", "함수 호출", "반환값과 OUT/IN OUT 값, REF CURSOR 결과를 지원합니다."],
    ].map(([key, label, help]) => <label className="check-label" key={key}>
      <input type="checkbox" checked={database[key]} onChange={e => set(key, e.target.checked)} />
      <span>{label}<br /><small>{help}</small></span>
    </label>)}
    <h3>저장 쿼리</h3>
    <small>SQL은 사용자가 작성하며 모델은 쿼리 ID와 바인드 값만 선택할 수 있습니다. 단일 SELECT/WITH 문을 입력하고 값은 :이름 바인드로 지정하세요. 결과는 최대 {database.max_rows}행입니다.</small>
    {database.queries.map((q, index) => <section className="credential-card" key={index}>
      <label className="check-label"><input type="checkbox" checked={q.enabled} onChange={e => updateQuery(index, { enabled: e.target.checked })} />이 쿼리 활성화</label>
      <label>쿼리 ID<input aria-label={`쿼리 ${index + 1} ID`} value={q.id} placeholder="recent_orders" onChange={e => updateQuery(index, { id: e.target.value })} /></label>
      <label>모델에게 보일 설명<textarea aria-label={`쿼리 ${index + 1} 설명`} rows={2} value={q.description} placeholder="최근 주문의 번호, 고객, 날짜를 조회합니다" onChange={e => updateQuery(index, { description: e.target.value })} /></label>
      <label>SQL<textarea aria-label={`쿼리 ${index + 1} SQL`} rows={5} value={q.sql} placeholder="SELECT order_id, customer_name FROM orders WHERE customer_id = :customer_id" onChange={e => updateQuery(index, { sql: e.target.value })} /></label>
      <strong>바인드 매개변수</strong>
      {q.params.map((p, paramIndex) => <div className="field-grid" key={paramIndex}>
        <label>이름<input aria-label={`매개변수 ${paramIndex + 1} 이름`} value={p.name} placeholder="customer_id" onChange={e => updateParam(index, paramIndex, { name: e.target.value })} /></label>
        <label>설명<input aria-label={`매개변수 ${paramIndex + 1} 설명`} value={p.description} placeholder="조회할 고객 ID" onChange={e => updateParam(index, paramIndex, { description: e.target.value })} /></label>
        <button type="button" className="secondary" onClick={() => updateQuery(index, { params: q.params.filter((_, i) => i !== paramIndex) })}>매개변수 삭제</button>
      </div>)}
      <div className="credential-options"><button type="button" className="secondary" onClick={() => updateQuery(index, { params: [...q.params, { name: "", description: "" }] })}>매개변수 추가</button><button type="button" className="text-button" onClick={() => set("queries", database.queries.filter((_, i) => i !== index))}>쿼리 삭제</button></div>
    </section>)}
    <button type="button" className="secondary" onClick={() => set("queries", [...database.queries, { id: "", description: "", sql: "", enabled: false, params: [] }])}>쿼리 추가</button>
  </div>;
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
          {tab === "database" && <DatabaseEditor database={draft.database} onChange={value => change("database", value)} />}
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
