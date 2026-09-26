import { useCallback, useEffect, useRef, useState } from "react";
import Chat from "./Chat.jsx";
import RunHistory from "./RunHistory.jsx";
import Settings, { ProjectForm, cleanProject } from "./Settings.jsx";
import { Message } from "./chat/Message.jsx";
import { api, send, statusLabel, toolLabels } from "./api.js";

const DEFAULT_INSPECTOR_WIDTH = 295;
const MIN_INSPECTOR_WIDTH = 240;
const MAX_INSPECTOR_WIDTH = 560;
const INSPECTOR_WIDTH_KEY = "mnemoarc.inspector-width";

function clampInspectorWidth(value) {
  return Math.min(
    MAX_INSPECTOR_WIDTH,
    Math.max(
      MIN_INSPECTOR_WIDTH,
      Math.round(Number(value) || DEFAULT_INSPECTOR_WIDTH),
    ),
  );
}

function loadInspectorWidth() {
  try {
    return clampInspectorWidth(window.localStorage.getItem(INSPECTOR_WIDTH_KEY));
  } catch {
    return DEFAULT_INSPECTOR_WIDTH;
  }
}

export default function App() {
  const [state, setState] = useState(null),
    [session, setSession] = useState(null),
    [selected, setSelected] = useState(() => window.location.hash.slice(1)),
    [page, setPage] = useState("chat"),
    [error, setError] = useState(""),
    [connected, setConnected] = useState(false),
    [detail, setDetail] = useState(true),
    [mobileNav, setMobileNav] = useState(false),
    [navigating, setNavigating] = useState(false),
    [stopped, setStopped] = useState(false),
    [stopping, setStopping] = useState(false),
    [inspectorWidth, setInspectorWidth] = useState(loadInspectorWidth);
  const selection = useRef(selected),
    fetching = useRef(false),
    pending = useRef(false),
    timer = useRef(null),
    alive = useRef(true),
    versions = useRef(new Map());
  const refresh = useCallback(async () => {
    if (fetching.current) {
      pending.current = true;
      return;
    }
    fetching.current = true;
    try {
      const next = await api("/state");
      if (!alive.current) return;
      setState(next);
      let id = selection.current;
      if (!next.sessions.some((s) => s.id === id)) {
        id = next.sessions[0]?.id || "";
        selection.current = id;
        setSelected(id);
      }
      if (id) {
        const current = await api(`/sessions/${id}`);
        if (
          alive.current &&
          selection.current === id &&
          current.revision >= (versions.current.get(id) || 0)
        ) {
          versions.current.set(id, current.revision);
          setSession((old) => {
            // Keep explicitly loaded older pages across live refreshes.
            const before =
              old?.id === id
                ? old.bundles.filter(
                    (b) =>
                      b.id < (current.bundles[0]?.id || 0) &&
                      (!current.pruned_through ||
                        b.id > current.pruned_through),
                  )
                : [];
            return {
              ...current,
              bundles: [...before, ...current.bundles],
              previous: before.length ? old.previous : current.previous,
            };
          });
        }
      } else setSession(null);
    } catch (e) {
      if (alive.current) setError(e.message);
    } finally {
      fetching.current = false;
      if (pending.current && alive.current) {
        pending.current = false;
        timer.current = setTimeout(() => {
          timer.current = null;
          void refresh();
        }, 120);
      }
    }
  }, []);
  useEffect(() => {
    if (stopped) return;
    alive.current = true;
    void refresh();
    const stream = new EventSource("/api/events");
    stream.onopen = () => {
      setConnected(true);
      void refresh();
    };
    stream.addEventListener("changed", () => {
      if (timer.current) return;
      timer.current = setTimeout(() => {
        timer.current = null;
        void refresh();
      }, 100);
    });
    stream.onerror = () => setConnected(false);
    const poll = setInterval(() => void refresh(), 3000);
    return () => {
      alive.current = false;
      stream.close();
      clearInterval(poll);
      clearTimeout(timer.current);
    };
  }, [refresh, stopped]);
  useEffect(() => {
    try {
      window.localStorage.setItem(INSPECTOR_WIDTH_KEY, String(inspectorWidth));
    } catch {
      // A private browsing context or disabled storage should not prevent
      // resizing the panel for the current page.
    }
  }, [inspectorWidth]);
  function choose(id) {
    selection.current = id;
    window.history.replaceState(null, "", `#${id}`);
    setSelected(id);
    setSession(null);
    setPage("chat");
    setMobileNav(false);
    void refresh();
  }
  async function act(fn) {
    setError("");
    try {
      return await fn();
    } catch (e) {
      setError(e.message);
      throw e;
    } finally {
      void refresh();
    }
  }
  const safe = (fn) => () => {
    void act(fn).catch(() => {});
  };
  async function create(project) {
    setNavigating(true);
    try {
      const data = await send("/sessions", { project });
      choose(data.id);
    } finally {
      setNavigating(false);
    }
  }
  async function closeSession(id) {
    if (
      !window.confirm(
        "이 세션을 닫을까요? 대화와 기억은 사라지고 결과 문서는 유지됩니다.",
      )
    )
      return;
    await send(`/sessions/${id}`, undefined, "DELETE");
  }
  async function shutdown() {
    if (
      !window.confirm(
        "진행 중인 작업을 중지하고 앱을 종료할까요? 저장된 문서는 유지되며 세션과 기억은 사라집니다.",
      )
    )
      return;
    setStopping(true);
    try {
      await send("/shutdown", {});
      setStopped(true);
    } catch (e) {
      setError(e.message);
    } finally {
      setStopping(false);
    }
  }
  const current = state?.sessions.find((s) => s.id === selected);
  const canRun = Boolean(
    session?.config.model && session?.config.model_context,
  );
  if (stopped)
    return (
      <main style={{ padding: "3rem", maxWidth: "40rem", margin: "auto" }}>
        <h1>앱 종료를 요청했습니다</h1>
        <p>진행 중인 작업을 정리한 뒤 종료합니다. 이 탭을 닫아도 됩니다.</p>
        <p>다시 사용하려면 MnemoArc 실행 파일을 실행하세요.</p>
      </main>
    );
  return (
    <div className="workspace">
      <aside className={`sidebar ${mobileNav ? "mobile-open" : ""}`}>
        <button className="brand" onClick={() => setPage("chat")}>
          <span className="brand-symbol">
            m<span>·</span>
          </span>
          <span>
            MnemoArc<small>기억하는 작업 공간</small>
          </span>
        </button>
        <button
          className="new-session"
          aria-label="새 세션"
          onClick={safe(() =>
            create(current?.project || state.config.projects[0]),
          )}
          disabled={!state?.config.projects.length}
        >
          <span>＋</span>새 세션
        </button>
        <div className="sidebar-section">
          <span>프로젝트</span>
          <button
            aria-label="프로젝트 추가·관리"
            title="프로젝트 관리"
            onClick={() => {
              setPage("projects");
              setMobileNav(false);
            }}
          >
            ＋
          </button>
        </div>
        <div className="session-list">
          {state?.config.projects.map((project, index) => (
            <div className="project-group" key={`${project.root}-${index}`}>
              <button
                className="project-heading"
                title={project.root}
                onClick={safe(() => create(project))}
              >
                <span>▱</span>
                {project.name}
                <span className="project-plus">＋</span>
              </button>
              {state.sessions
                .filter((s) => s.project.root === project.root)
                .map((s) => (
                  <SessionButton
                    key={s.id}
                    session={s}
                    active={s.id === selected && page === "chat"}
                    onClick={() => choose(s.id)}
                    onClose={safe(() => closeSession(s.id))}
                    closing={state.running?.id === s.id && state.running.closing}
                  />
                ))}
            </div>
          ))}
          {state?.sessions
            .filter(
              (s) =>
                !state.config.projects.some((p) => p.root === s.project.root),
            )
            .map((s) => (
              <SessionButton
                key={s.id}
                session={s}
                active={s.id === selected}
                onClick={() => choose(s.id)}
                onClose={safe(() => closeSession(s.id))}
                closing={state.running?.id === s.id && state.running.closing}
              />
            ))}
        </div>
        <div className="sidebar-bottom">
          <div className="connection-status">
            <i className={connected ? "online" : ""} />
            {connected ? "로컬 에이전트 연결됨" : "연결 복구 중…"}
          </div>
          <button
            className={page === "projects" ? "selected" : ""}
            aria-label="프로젝트 관리"
            onClick={() => {
              setPage("projects");
              setMobileNav(false);
            }}
          >
            <span>▱</span>프로젝트 관리
          </button>
          <button
            className={page === "settings" ? "selected" : ""}
            onClick={() => {
              setPage("settings");
              setMobileNav(false);
            }}
          >
            <span>⚙</span>모든 설정
          </button>
          <button aria-label="앱 종료" onClick={shutdown} disabled={stopping}>
            <span>⏻</span>
            {stopping ? "종료 요청 중…" : "앱 종료"}
          </button>
          <p>탭을 닫아도 작업은 계속됩니다. 종료하려면 앱 종료를 누르세요.</p>
        </div>
      </aside>
      <main className="main-workspace">
        <header className="topbar">
          <button
            className="mobile-menu"
            aria-label="메뉴 열기"
            onClick={() => setMobileNav(!mobileNav)}
          >
            ☰
          </button>
          <div className="breadcrumb">
            <span>작업 공간</span>
            <b>/</b>
            <strong>
              {page === "settings"
                ? "설정"
                : page === "projects"
                  ? "프로젝트"
                  : current?.project.name || "새 작업"}
            </strong>
          </div>
          <div className="topbar-actions">
            {state?.running && state.running.id !== selected && (
              <button
                className="running-link"
                onClick={() => choose(state.running.id)}
              >
                ● 다른 세션 작업 중
              </button>
            )}
            {page === "chat" && session && (
              <>
                <span className={`status-pill ${session.status}`}>
                  {statusLabel[session.status] || session.status}
                </span>
                <button
                  className="icon-button"
                  title="상세 패널 표시"
                  aria-label="상세 패널 표시"
                  aria-pressed={detail}
                  onClick={() => setDetail(!detail)}
                >
                  ☷
                </button>
              </>
            )}
          </div>
        </header>
        {error && (
          <div className="app-error" role="alert">
            <span>{error}</span>
            <button aria-label="오류 닫기" onClick={() => setError("")}>
              ×
            </button>
          </div>
        )}
        {!state ? (
          <div className="loading-state">작업 공간을 불러오는 중…</div>
        ) : page === "settings" ? (
          <Settings
            key={`settings-${selected}`}
            config={state.config}
            credential={state.credential}
            sessionCredential={session?.credential_configured}
            sessionConfig={session?.pending_config || session?.config}
            sessionId={session?.id}
            onSaved={refresh}
            onClose={() => setPage("chat")}
          />
        ) : page === "projects" ? (
          <Projects
            config={state.config}
            onSaved={refresh}
            onCreate={(project) => act(() => create(project))}
          />
        ) : session && !navigating ? (
          <div
            className={`workarea ${detail ? "with-details" : ""}`}
            style={
              detail
                ? { "--inspector-width": `${inspectorWidth}px` }
                : undefined
            }
          >
            <div className="chat-column">
              <div className="session-heading">
                <div>
                  <h2>{session.project.name}</h2>
                  <p title={session.project.root}>{session.project.root}</p>
                </div>
                <div className="session-actions">
                  <button
                    title="보존된 상태로 재개"
                    disabled={Boolean(state.running) || !canRun}
                    onClick={safe(() =>
                      send(`/sessions/${selected}/run`, { action: "resume" }),
                    )}
                  >
                    재개
                  </button>
                  <button
                    title="기억과 상태 정리"
                    disabled={Boolean(state.running) || !canRun}
                    onClick={safe(() =>
                      send(`/sessions/${selected}/run`, { action: "cleanup" }),
                    )}
                  >
                    기억 정리
                  </button>
                  <button
                    className="danger-text"
                    disabled={
                      state.running?.id === selected && state.running.closing
                    }
                    onClick={safe(() => closeSession(selected))}
                  >
                    세션 닫기
                  </button>
                </div>
              </div>
              {session.pending_config && (
                <div className="pending-note">
                  설정 변경이 대기 중입니다. 현재 요청이 끝나거나 필요한 기억
                  정리가 완료되면 적용됩니다.
                </div>
              )}
              <Chat
                key={session.id}
                session={session}
                busy={Boolean(state.running)}
                canRun={canRun}
                onSend={(text) =>
                  act(() => send(`/sessions/${selected}/run`, { text }))
                }
                onWorkflow={(workflow) =>
                  act(() =>
                    send(`/sessions/${selected}/workflow`, { workflow }, "PUT"),
                  )
                }
                onCancel={safe(() => send(`/sessions/${selected}/cancel`, {}))}
                onSettings={() => setPage("settings")}
                onOlder={safe(async () => {
                  const older = await api(
                    `/sessions/${selected}?before=${session.previous}`,
                  );
                  if (selection.current === older.id)
                    setSession((old) => {
                      if (!old || old.id !== older.id) return old;
                      const byId = new Map();
                      for (const bundle of older.bundles) byId.set(bundle.id, bundle);
                      // Prefer the live response for overlapping IDs because it
                      // carries the newest active/reviewed flags.
                      for (const bundle of old.bundles) byId.set(bundle.id, bundle);
                      return {
                        ...old,
                        bundles: [...byId.values()].sort((a, b) => a.id - b.id),
                        previous: older.previous,
                      };
                    });
                })}
              />
            </div>
            {detail && (
              <Inspector
                key={session.id}
                session={session}
                tools={state.tools}
                running={state.running}
                onAction={act}
                width={inspectorWidth}
                onWidthChange={(next) =>
                  setInspectorWidth(clampInspectorWidth(next))
                }
              />
            )}
          </div>
        ) : (
          <div className="loading-state">
            <h2>
              {state.sessions.length
                ? "세션을 불러오는 중…"
                : "첫 작업을 시작해 보세요"}
            </h2>
            <button className="primary" onClick={() => setPage("projects")}>
              프로젝트 관리
            </button>
          </div>
        )}
      </main>
    </div>
  );
}
function SessionButton({ session, active, onClick, onClose, closing }) {
  const title = session.title || "새 대화";
  return (
    <div className={`session-row ${active ? "active" : ""}`}>
      <button
        className="session-button"
        onClick={onClick}
        title={title}
      >
        <i className={session.status === "running" ? "pulse" : ""} />
        <span>
          {title}
          <small>
            {session.memory_count}개 기억 ·{" "}
            {statusLabel[session.status] || session.status}
          </small>
        </span>
      </button>
      <button
        className="session-close"
        aria-label="세션 닫기"
        title="세션 닫기"
        disabled={closing}
        onClick={(event) => {
          event.stopPropagation();
          onClose();
        }}
      >
        ×
      </button>
    </div>
  );
}
function Projects({ config, onSaved, onCreate }) {
  const empty = {
    name: "새 프로젝트",
    root: "",
    output: "docs/source-summary.md",
    include: [],
    exclude: [".env*"],
    purpose: "프로젝트 구조, 주요 흐름과 오류 처리를 소스 근거와 함께 설명",
    audience: "신규 개발자",
  };
  const [list, setList] = useState(() => structuredClone(config.projects)),
    [index, setIndex] = useState(0),
    [error, setError] = useState(""),
    [message, setMessage] = useState(""),
    [busy, setBusy] = useState(false),
    [dirty, setDirty] = useState(false);
  const project = list[index];
  async function save() {
    setBusy(true);
    setError("");
    try {
      await send(
        "/settings",
        { config: { ...config, projects: list.map(cleanProject) } },
        "PUT",
      );
      setMessage("프로젝트를 저장했습니다. 새 세션에 적용됩니다.");
      setDirty(false);
      await onSaved();
    } catch (e) {
      setError(e.message);
    } finally {
      setBusy(false);
    }
  }
  return (
    <section className="settings-page">
      <header className="page-heading">
        <div>
          <span className="eyebrow">PROJECTS</span>
          <h1>프로젝트 관리</h1>
          <p>조사할 폴더와 결과 문서의 위치를 정합니다.</p>
        </div>
        <button
          className="secondary"
          onClick={() => {
            setList([...list, empty]);
            setIndex(list.length);
            setDirty(true);
          }}
        >
          ＋ 프로젝트 추가
        </button>
      </header>
      <div className="settings-layout">
        <nav className="settings-tabs">
          {list.map((p, i) => (
            <button
              key={i}
              className={i === index ? "active" : ""}
              onClick={() => setIndex(i)}
            >
              {p.name || "이름 없는 프로젝트"}
            </button>
          ))}
        </nav>
        <div className="settings-body">
          {project ? (
            <>
              <ProjectForm
                project={project}
                onChange={(p) => {
                  setList(list.map((old, i) => (i === index ? p : old)));
                  setDirty(true);
                  setMessage("");
                }}
              />
              <div className="settings-actions">
                <button
                  className="danger-text"
                  onClick={() => {
                    if (list.length <= 1) return;
                    setList(list.filter((_, i) => i !== index));
                    setIndex(0);
                    setDirty(true);
                  }}
                  disabled={list.length <= 1}
                >
                  목록에서 삭제
                </button>
                <span />
                <button
                  className="secondary"
                  disabled={busy}
                  onClick={() => {
                    void onCreate(cleanProject(project)).catch((e) =>
                      setError(e.message),
                    );
                  }}
                >
                  이 프로젝트로 새 세션
                </button>
                <button className="primary" disabled={busy} onClick={save}>
                  프로젝트 저장
                </button>
              </div>
            </>
          ) : (
            <>
              <p>프로젝트를 추가하세요.</p>
              <button className="primary" onClick={save}>
                변경 사항 저장
              </button>
            </>
          )}
          {dirty && (
            <small className="subtle">
              저장하지 않은 변경 사항이 있습니다.
            </small>
          )}
          {message && (
            <p className="success-message" role="status">
              {message}
            </p>
          )}
          {error && (
            <p className="inline-error" role="alert">
              {error}
            </p>
          )}
        </div>
      </div>
    </section>
  );
}
function InspectorResizeHandle({ width, onWidthChange }) {
  const drag = useRef(null);
  const [dragging, setDragging] = useState(false);

  useEffect(() => {
    if (!dragging) return;
    const previousCursor = document.body.style.cursor;
    const previousUserSelect = document.body.style.userSelect;
    document.body.classList.add("inspector-resizing");
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    return () => {
      document.body.classList.remove("inspector-resizing");
      document.body.style.cursor = previousCursor;
      document.body.style.userSelect = previousUserSelect;
    };
  }, [dragging]);

  function finish(event) {
    if (event?.currentTarget?.hasPointerCapture?.(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
    drag.current = null;
    setDragging(false);
  }

  return (
    <div
      className={`inspector-resizer ${dragging ? "dragging" : ""}`}
      role="separator"
      aria-label="오른쪽 패널 너비 조절"
      aria-orientation="vertical"
      aria-valuemin={MIN_INSPECTOR_WIDTH}
      aria-valuemax={MAX_INSPECTOR_WIDTH}
      aria-valuenow={Math.round(width)}
      aria-valuetext={`${Math.round(width)}픽셀`}
      tabIndex={0}
      title="드래그하여 패널 너비 조절 · 더블클릭하면 기본 너비로 복원"
      onPointerDown={(event) => {
        if (event.button !== 0) return;
        event.preventDefault();
        drag.current = { x: event.clientX, width };
        event.currentTarget.setPointerCapture?.(event.pointerId);
        setDragging(true);
      }}
      onPointerMove={(event) => {
        if (!drag.current) return;
        onWidthChange(drag.current.width + drag.current.x - event.clientX);
      }}
      onPointerUp={finish}
      onPointerCancel={finish}
      onDoubleClick={() => onWidthChange(DEFAULT_INSPECTOR_WIDTH)}
      onKeyDown={(event) => {
        const step = event.shiftKey ? 40 : 16;
        if (event.key === "ArrowLeft") {
          event.preventDefault();
          onWidthChange(width + step);
        } else if (event.key === "ArrowRight") {
          event.preventDefault();
          onWidthChange(width - step);
        } else if (event.key === "Home") {
          event.preventDefault();
          onWidthChange(MIN_INSPECTOR_WIDTH);
        } else if (event.key === "End") {
          event.preventDefault();
          onWidthChange(MAX_INSPECTOR_WIDTH);
        }
      }}
    >
      <span aria-hidden="true" />
    </div>
  );
}

function Inspector({
  session,
  tools,
  running,
  onAction,
  width,
  onWidthChange,
}) {
  const [toolSelection, setToolSelection] = useState(session.active_tools);
  const [toolsSaving, setToolsSaving] = useState(false);
  const activeToolsKey = JSON.stringify(session.active_tools);
  useEffect(() => {
    setToolSelection(JSON.parse(activeToolsKey));
  }, [activeToolsKey]);
  const [tab, setTab] = useState("memory"),
    [query, setQuery] = useState(""),
    [memory, setMemory] = useState(null),
    [document, setDocument] = useState(null),
    [project, setProject] = useState(session.project);
  const doAction = (fn) => () => {
    void onAction(fn).catch(() => {});
  };
  const memories = session.memories.filter((m) =>
    JSON.stringify(m).toLowerCase().includes(query.toLowerCase()),
  );
  const recovering =
    session.status === "running" && session.run_guidance?.progress_recovery?.active;
  const todos = session.task.todos || [];
  const currentTodo = todos.find((item) => !item.done);
  return (
    <aside className="inspector">
      <InspectorResizeHandle width={width} onWidthChange={onWidthChange} />
      <div className="inspector-tabs" role="tablist">
        {[
          ["memory", "기억"],
          ["progress", "진행"],
          ["tools", "도구"],
          ["output", "문서"],
          ["project", "프로젝트"],
        ].map(([id, label]) => (
          <button
            key={id}
            role="tab"
            aria-selected={tab === id}
            className={tab === id ? "active" : ""}
            onClick={() => setTab(id)}
          >
            {label}
          </button>
        ))}
      </div>
      <div className="inspector-content">
        {tab === "memory" && (
          <>
            <div className="panel-heading">
              <h3>세션 기억</h3>
              <span className="count-badge">{session.memories.length}</span>
            </div>
            <p className="subtle">
              필요한 발견을 저장하고 다음 작업에 활용합니다.
            </p>
            <input
              className="memory-search"
              aria-label="기억 검색"
              placeholder="제목, 태그, 키 검색…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
            {memory ? (
              <article className="memory-detail">
                <button className="text-button" onClick={() => setMemory(null)}>
                  ← 목록으로
                </button>
                <h3>{memory.title}</h3>
                <small>
                  개정 {memory.revision} · {memory.status}
                </small>
                <Message role="assistant" text={memory.body} />
                <h4>출처</h4>
                {memory.sources.map((source) => (
                  <div className="source-card" key={source.id}>
                    <code>
                      {source.path || "사용자 발언"}{" "}
                      {source.start_line ? `:${source.start_line}` : ""}
                    </code>
                    <p>{source.excerpt}</p>
                  </div>
                ))}
              </article>
            ) : memories.length ? (
              memories.map((m) => (
                <button
                  className="memory-card"
                  key={m.id}
                  onClick={doAction(async () =>
                    setMemory(
                      await api(`/sessions/${session.id}/memories/${m.id}`),
                    ),
                  )}
                >
                  <div>
                    <strong>{m.title}</strong>
                    <small>r{m.revision}</small>
                  </div>
                  <p>{m.summary}</p>
                  <span>
                    {m.status === "needs_review"
                      ? "재검증 필요"
                      : m.status === "superseded"
                        ? "대체됨"
                        : "사용 가능"}
                  </span>
                  {m.tags.map((t) => (
                    <span className="tag" key={t}>
                      {t}
                    </span>
                  ))}
                </button>
              ))
            ) : (
              <div className="panel-empty">
                <span>◇</span>
                <p>
                  {query
                    ? "검색 결과가 없습니다."
                    : "작업 중 발견한 내용이\n여기에 기억으로 쌓입니다."}
                </p>
              </div>
            )}
            <div className="usage-card">
              <h4>이 세션의 사용량</h4>
              <dl>
                <dt>입력 토큰</dt>
                <dd>{session.usage.input.toLocaleString()}</dd>
                <dt>출력 토큰</dt>
                <dd>{session.usage.output.toLocaleString()}</dd>
                <dt>캐시 토큰</dt>
                <dd>{session.usage.cached ?? "제공 안 됨"}</dd>
                <dt>체크포인트</dt>
                <dd>{session.usage.checkpoints}회</dd>
                <dt>원문 보관</dt>
                <dd>
                  {(session.usage.history_bytes / 1048576).toFixed(2)} MiB
                </dd>
              </dl>
              <small>
                {session.usage.estimated
                  ? "일부 사용량은 추정치입니다."
                  : "제공된 사용량 기준"}
                {session.usage.context_estimated &&
                  " · 컨텍스트 예산은 기준 토크나이저 + 25% 여유로 추정합니다."}
              </small>
            </div>
          </>
        )}
        {tab === "progress" && (
          <>
            <h3>목표와 진행</h3>
            <RunHistory records={session.run_history} />
            {session.run_guidance?.phase && (
              <p>
                실행 단계:{" "}
                {
                  {
                    investigate: "조사",
                    draft: "작성 우선",
                    verify: "검증 우선",
                    answer: "답변 작성",
                  }[session.run_guidance.phase]
                }
                {" · 남은 실행 예산 "}
                {session.run_guidance.remaining_tokens?.toLocaleString()} 토큰
                {" · 미검증 "}
                {session.run_guidance.pending_count}개
              </p>
            )}
            <section className="task-plan-section" aria-label="할 일 목록">
              <h4>할 일 목록</h4>
              <p className="subtle">
                남은 항목 {todos.filter((item) => !item.done).length}/100 · 누적
                완료 {session.task.todos_completed_total || 0}개
              </p>
              {session.status === "running" &&
              session.run_guidance?.closing?.active ? (
                <p role="status" className="plan-recovery">
                  마감 단계입니다 (
                  {session.run_guidance.closing.reason === "budget"
                    ? "예산 도달"
                    : "진행 정체"}
                  , 요청 {session.run_guidance.closing.rounds}/
                  {session.run_guidance.closing.round_limit}). 수집한 근거로
                  문서를 마무리하고 확인하지 못한 항목은 결과에 명시합니다.
                </p>
              ) : (
                recovering && (
                  <p role="status" className="plan-recovery">
                    반복을 감지해 현재 항목의 실제 결과를 만드는 데 집중하고
                    있습니다.
                  </p>
                )
              )}
              {todos.length ? (
                <ol className="task-plan">
                  {todos.map((item) => (
                    <li
                      key={item.id}
                      className={
                        item.done
                          ? "done"
                          : item.id === currentTodo?.id
                            ? "active"
                            : "pending"
                      }
                      aria-current={
                        item.id === currentTodo?.id ? "step" : undefined
                      }
                    >
                      <span className="todo-status">
                        {item.done
                          ? "완료"
                          : item.id === currentTodo?.id
                            ? "진행 중"
                            : "대기"}
                      </span>
                      <span>{item.text}</span>
                      {item.result && <small>{item.result}</small>}
                    </li>
                  ))}
                </ol>
              ) : (
                <p className="subtle">
                  여러 단계가 필요한 작업을 시작하면 순서대로 표시됩니다.
                </p>
              )}
              {session.task.todos_completed_total > 5 && (
                <small className="subtle">
                  완료 기록은 최근 5개를 표시합니다.
                </small>
              )}
            </section>
            {session.completion_gaps?.length > 0 && (
              <section
                className="progress-section completion-gaps"
                aria-label="확인하지 못한 항목"
              >
                <h4>확인하지 못한 항목 {session.completion_gaps.length}건</h4>
                <p className="subtle">
                  문서는 완료로 처리했지만 아래 항목은 검증되지 않았습니다.
                  이어서 진행하면 보완할 수 있습니다.
                </p>
                <ul>
                  {session.completion_gaps.map((gap, i) => (
                    <li key={i}>{gap}</li>
                  ))}
                </ul>
              </section>
            )}
            {session.completion_review?.required && (
              <section className="progress-section" aria-label="완료 조건 검증">
                <h4>완료 조건 검증</h4>
                <p role="status">
                  {session.completion_review.pending
                    ? "실제 결과를 검증하고 있습니다."
                    : session.completion_review.approved
                      ? "모든 완료 조건의 검증을 통과했습니다."
                      : "할 일 완료 후에도 조건이 충족될 때까지 보완합니다."}
                </p>
                <ul>
                  {(session.completion_review.checks || []).map((check) => (
                    <li key={check.id}>
                      <strong>
                        {{ met: "충족", unmet: "미충족", unverified: "확인 불가" }[check.status]}
                        {" · "}{check.id === "R0" ? "원래 요청" : check.criterion || "완료 조건"}
                      </strong>
                      <p>{check.reason}</p>
                      {check.next_action && <p>보완: {check.next_action}</p>}
                    </li>
                  ))}
                </ul>
                <small className="subtle">
                  최근 검증 결과입니다. 결과물이나 조건이 바뀌면 다시 확인합니다.
                </small>
              </section>
            )}
            <p>{session.task.purpose}</p>
            {["constraints", "completion", "findings", "unresolved"].map(
              (key, i) => (
                <section className="progress-section" key={key}>
                  <h4>
                    {["제약", "완료 조건", "발견한 내용", "미확인 사항"][i]}
                  </h4>
                  {session.task[key].length ? (
                    <ul>
                      {session.task[key].map((v, i) => (
                        <li key={i}>{v}</li>
                      ))}
                    </ul>
                  ) : (
                    <p className="subtle">아직 등록된 내용이 없습니다.</p>
                  )}
                </section>
              ),
            )}
            <h4>조사 목록</h4>
            {session.investigations.map((item) => (
              <div className="source-card" key={item.id}>
                <strong>{item.title}</strong>
                <small>
                  {{
                    uninvestigated: "미조사",
                    in_progress: "조사 중",
                    written: "작성됨",
                    verified: "검증됨",
                    gap: "미확인",
                  }[item.status] || item.status}
                </small>
                <p>{item.note}</p>
              </div>
            ))}
          </>
        )}
        {tab === "tools" && (
          <>
            <h3>사용할 도구</h3>
            <p className="subtle">변경 사항은 다음 요청부터 적용됩니다.</p>
            {tools.map((tool) => (
              <label className="tool-toggle" key={tool.name}>
                <input
                  type="checkbox"
                  aria-label={toolLabels[tool.name] || tool.name}
                  checked={!tool.optional || toolSelection.includes(tool.name)}
                  disabled={!tool.optional || toolsSaving}
                  onChange={(e) => {
                    const next = new Set(toolSelection);
                    e.target.checked
                      ? next.add(tool.name)
                      : next.delete(tool.name);
                    setToolSelection([...next]);
                    setToolsSaving(true);
                    void onAction(() =>
                      send(
                        `/sessions/${session.id}/tools`,
                        { names: [...next] },
                        "PUT",
                      ),
                    )
                      .catch(() => setToolSelection(session.active_tools))
                      .finally(() => setToolsSaving(false));
                  }}
                />
                <span>
                  <strong>{toolLabels[tool.name] || tool.name}</strong>
                  <small>
                    {tool.optional ? "선택 도구" : "기본 도구 · 항상 사용"}
                  </small>
                  {tool.name === "file_read" && (
                    <small style={{ overflowWrap: "anywhere" }}>
                      상대 경로는 프로젝트 루트 기준입니다:{" "}
                      {session.project.root}
                      <br />
                      설정된 결과 문서: {session.project.output}
                      <br />
                      결과 문서가 프로젝트 밖에 있으면 문서 구조
                      조회(document_inspect)가 반환한 절대 경로를 그대로
                      사용하세요. 파일명만 넘기면 프로젝트 안에서 찾습니다.
                      <br />
                      시작 줄은 start_line, 읽을 줄 수는 max_lines입니다.
                      limit은 max_lines의 별칭이며 offset은 줄 번호가 아닙니다.
                      잘린 결과는 반환된 cursor로 이어 읽으세요.
                    </small>
                  )}
                  {tool.name === "investigation" && (
                    <small>
                      upsert는 title을 포함해 항목 하나씩 등록합니다. 여러
                      항목은 각각 호출하세요. verify는 id, source_ids,
                      verification_note가 필요합니다. items는 기존 작성 항목의
                      일괄 검증인 verify_batch에서만 사용하며, 항목 ID를 키로
                      갖는 객체입니다.
                    </small>
                  )}
                  {tool.name === "document_inspect" && (
                    <small>
                      경로 입력 없이 설정된 결과 문서를 조회합니다. 섹션 제목은
                      #을 생략해도 유일하면 찾습니다. 중복이면 #을 포함한 실제
                      제목으로 구분하세요.
                    </small>
                  )}
                </span>
              </label>
            ))}
          </>
        )}
        {tab === "output" && (
          <>
            <h3>결과 문서</h3>
            <p className="directory-path">{session.project.output}</p>
            <button
              className="secondary"
              onClick={doAction(async () =>
                setDocument(await api(`/sessions/${session.id}/output`)),
              )}
            >
              문서 불러오기
            </button>
            {document && (
              <>
                <button
                  className="text-button"
                  onClick={() => {
                    const url = URL.createObjectURL(
                      new Blob([document.content], {
                        type: "text/markdown;charset=utf-8",
                      }),
                    );
                    const link = Object.assign(
                      window.document.createElement("a"),
                      {
                        href: url,
                        download: session.project.output.split("/").pop(),
                      },
                    );
                    link.click();
                    setTimeout(() => URL.revokeObjectURL(url), 1000);
                  }}
                >
                  Markdown 내려받기
                </button>
                {document.truncated && (
                  <p>미리보기 한도로 일부만 표시합니다.</p>
                )}
                <Message role="assistant" text={document.content} />
              </>
            )}
          </>
        )}
        {tab === "project" && (
          <>
            <h3>현재 세션 프로젝트</h3>
            <p className="subtle">
              이 세션에만 적용합니다. 프로젝트 기본값은 왼쪽 관리 화면에서
              변경하세요.
            </p>
            <ProjectForm
              project={project}
              onChange={setProject}
              disabled={running?.id === session.id}
            />
            <button
              className="primary"
              disabled={running?.id === session.id}
              onClick={doAction(() =>
                send(
                  `/sessions/${session.id}/project`,
                  cleanProject(project),
                  "PUT",
                ),
              )}
            >
              현재 세션에 적용
            </button>
          </>
        )}
      </div>
    </aside>
  );
}
