import { useCallback, useEffect, useRef, useState } from "react";
import Chat from "./Chat.jsx";
import NewSession from "./NewSession.jsx";
import RunHistory from "./RunHistory.jsx";
import Settings, { ProjectForm, cleanProject } from "./Settings.jsx";
import { Message } from "./chat/Message.jsx";
import {
  api,
  download,
  send,
  statusLabel,
  toolLabels,
  workflowOptions,
} from "./api.js";
import { mergeSession, mergeOlder } from "./session-history.js";
import { useServerDraft } from "./use-server-draft.js";
import { displayPath, displayPathText } from "./paths.js";
import { memoryMatchesQuery } from "./memory-search.js";
import { createComposerDrafts } from "./composer-drafts.js";
import {
  createSessionCache,
  FULL_REFRESH,
  NO_REFRESH,
  mergeRefresh,
  changeRefresh,
  parseChange,
} from "./session-cache.js";

const DEFAULT_INSPECTOR_WIDTH = 295;
const MIN_INSPECTOR_WIDTH = 240;
// The panel may take up to this share of the chat and panel area.
const MAX_INSPECTOR_RATIO = 0.7;
const INSPECTOR_WIDTH_KEY = "mnemoarc.inspector-width";

function maxInspectorWidth(areaWidth) {
  return Math.max(
    MIN_INSPECTOR_WIDTH,
    Math.floor(areaWidth * MAX_INSPECTOR_RATIO),
  );
}

function clampInspectorWidth(value, max = Infinity) {
  return Math.min(
    max,
    Math.max(
      MIN_INSPECTOR_WIDTH,
      Math.round(Number(value) || DEFAULT_INSPECTOR_WIDTH),
    ),
  );
}

function loadInspectorWidth() {
  try {
    return clampInspectorWidth(
      window.localStorage.getItem(INSPECTOR_WIDTH_KEY),
    );
  } catch {
    return DEFAULT_INSPECTOR_WIDTH;
  }
}

export default function App() {
  const [drafts] = useState(createComposerDrafts);
  const [cache] = useState(createSessionCache);
  const [state, setState] = useState(null),
    [session, setSession] = useState(null),
    [sessionReady, setSessionReady] = useState(false),
    [selected, setSelected] = useState(() => window.location.hash.slice(1)),
    [page, setPage] = useState("chat"),
    [error, setError] = useState(""),
    [refreshError, setRefreshError] = useState(""),
    [connected, setConnected] = useState(false),
    [detail, setDetail] = useState(
      () => !window.matchMedia("(max-width: 1000px)").matches,
    ),
    [mobileNav, setMobileNav] = useState(false),
    [navigating, setNavigating] = useState(false),
    [runPending, setRunPending] = useState(new Set()),
    [newSession, setNewSession] = useState(null),
    [stopped, setStopped] = useState(false),
    [stopping, setStopping] = useState(false),
    [inspectorWidth, setInspectorWidth] = useState(loadInspectorWidth),
    [workareaWidth, setWorkareaWidth] = useState(0);
  const selection = useRef(selected),
    fetching = useRef(null),
    pending = useRef(NO_REFRESH),
    workspace = useRef(null),
    displayed = useRef(null),
    lastChecked = useRef({ state: 0, session: 0 }),
    streamOpen = useRef(false),
    recovering = useRef(false),
    timer = useRef(null),
    alive = useRef(true),
    versions = useRef(new Map()),
    expectedVersions = useRef(new Map()),
    globalVersion = useRef(0),
    serverInstance = useRef(null),
    settingsDirty = useRef(false),
    projectsDirty = useRef(false),
    inspectorDirty = useRef(false),
    creating = useRef(false),
    startingRun = useRef(new Set()),
    navigationEpoch = useRef(0),
    selectionEpoch = useRef(0);
  const onSettingsDirtyChange = useCallback((dirty) => {
    settingsDirty.current = dirty;
  }, []);
  const onProjectsDirtyChange = useCallback((dirty) => {
    projectsDirty.current = dirty;
  }, []);
  const onInspectorDirtyChange = useCallback((dirty) => {
    inspectorDirty.current = dirty;
  }, []);
  const displaySession = useCallback(
    (value) => {
      displayed.current = value;
      setSession(value);
      if (value) cache.set(value);
    },
    [cache],
  );
  // A displayed response may lag a change already announced by the server.
  // Keep that minimum separate from accepted responses and from other sessions.
  const requiredVersion = useCallback(
    (id) =>
      Math.max(
        versions.current.get(id) || 0,
        expectedVersions.current.get(id) || 0,
        globalVersion.current,
      ),
    [],
  );
  const refresh = useCallback(
    (restart = false, scope = FULL_REFRESH) => {
      if (!alive.current) return Promise.resolve();
      pending.current = mergeRefresh(pending.current, scope);
      if (fetching.current) {
        if (!restart) return fetching.current.done;
        pending.current = mergeRefresh(pending.current, fetching.current.scope);
        fetching.current.controller.abort();
      }
      scope = pending.current;
      if (!scope.state && !scope.session) return Promise.resolve();
      if (!workspace.current) scope = { ...scope, state: true };
      const controller = new AbortController();
      const request = { controller, done: null, scope };
      fetching.current = request;
      pending.current = NO_REFRESH;
      clearTimeout(timer.current);
      timer.current = null;
      const epoch = selectionEpoch.current;
      const isCurrent = () =>
        alive.current &&
        fetching.current === request &&
        selectionEpoch.current === epoch &&
        !controller.signal.aborted;
      let timedOut = false;
      const deadline = setTimeout(() => {
        timedOut = true;
        controller.abort();
      }, 15000);
      const loadSession = async (id) => {
        const instance = serverInstance.current;
        const current = await api(`/sessions/${id}`, {
          signal: controller.signal,
        });
        if (
          isCurrent() &&
          selection.current === id &&
          (instance !== serverInstance.current ||
            current.server_instance !== serverInstance.current)
        ) {
          // A detail request can outlive the state request that discovers a
          // restart. Its revision and history belong to the previous server.
          setSessionReady(false);
          pending.current = mergeRefresh(pending.current, FULL_REFRESH);
          return;
        }
        if (
          isCurrent() &&
          selection.current === id &&
          current.revision >= (versions.current.get(id) || 0)
        ) {
          versions.current.set(id, current.revision);
          const previous =
            displayed.current?.id === id ? displayed.current : cache.peek(id);
          displaySession(mergeSession(previous, current));
          lastChecked.current.session = Date.now();
          setSessionReady(current.revision >= requiredVersion(id));
        } else if (isCurrent() && selection.current === id) {
          setSessionReady(false);
          pending.current = mergeRefresh(pending.current, {
            state: false,
            session: true,
          });
        }
      };
      // A known selection can load independently of the sidebar. Navigating
      // must not wait for an unrelated state request that is slow or stalled.
      const earlyId = selection.current;
      const early =
        scope.session &&
        workspace.current?.sessions.some((s) => s.id === earlyId)
          ? loadSession(earlyId).then(
              () => null,
              (error) => error,
            )
          : null;
      request.done = (async () => {
        try {
          const next = scope.state
            ? await api("/state", { signal: controller.signal })
            : workspace.current;
          if (isCurrent()) {
            if (scope.state) {
              if (serverInstance.current !== next.server_instance) {
                serverInstance.current = next.server_instance;
                versions.current.clear();
                expectedVersions.current.clear();
                globalVersion.current = 0;
                cache.clear();
                displaySession(null);
                setSessionReady(false);
                lastChecked.current.session = 0;
                scope = { ...scope, session: true };
              }
              workspace.current = next;
              setState(next);
              lastChecked.current.state = Date.now();
              const ids = next.sessions.map((s) => s.id);
              drafts.retain(ids);
              cache.retain(ids);
              const keep = new Set(ids);
              for (const entries of [
                versions.current,
                expectedVersions.current,
              ]) {
                for (const id of entries.keys())
                  if (!keep.has(id)) entries.delete(id);
              }
              for (const summary of next.sessions) {
                expectedVersions.current.set(
                  summary.id,
                  Math.max(
                    expectedVersions.current.get(summary.id) || 0,
                    summary.revision || 0,
                  ),
                );
              }
            }
            let id = selection.current;
            if (!next.sessions.some((s) => s.id === id)) {
              id = next.sessions[0]?.id || "";
              selection.current = id;
              setSelected(id);
              displaySession(cache.get(id));
              setSessionReady(false);
              scope = { ...scope, session: true };
              window.history.replaceState(
                null,
                "",
                id
                  ? `#${id}`
                  : window.location.pathname + window.location.search,
              );
            }
            if (id) {
              if (requiredVersion(id) > (versions.current.get(id) ?? -1))
                setSessionReady(false);
              if (early && id === earlyId) {
                const error = await early;
                if (error) throw error;
              }
              if (
                (scope.session && (!early || id !== earlyId)) ||
                requiredVersion(id) > (versions.current.get(id) ?? -1)
              ) {
                await loadSession(id);
              }
              if (
                isCurrent() &&
                requiredVersion(id) > (versions.current.get(id) ?? -1)
              ) {
                setSessionReady(false);
                pending.current = mergeRefresh(pending.current, {
                  state: false,
                  session: true,
                });
              }
            } else {
              displaySession(null);
              setSessionReady(false);
            }
            if (isCurrent()) {
              setRefreshError("");
              recovering.current = false;
            }
          }
        } catch (e) {
          if (
            alive.current &&
            fetching.current === request &&
            selectionEpoch.current === epoch
          ) {
            recovering.current = true;
            if (e.code === "server_version_mismatch") {
              workspace.current = null;
              setState(null);
              setSessionReady(false);
            }
            if (timedOut)
              setRefreshError(
                "작업 공간 응답이 지연되고 있습니다. 다시 연결을 시도합니다.",
              );
            else if (!controller.signal.aborted) setRefreshError(e.message);
            controller.abort();
          }
        } finally {
          clearTimeout(deadline);
          if (fetching.current === request) {
            fetching.current = null;
            if (
              (pending.current.state || pending.current.session) &&
              alive.current
            ) {
              timer.current = setTimeout(() => {
                timer.current = null;
                void refresh(false, NO_REFRESH);
              }, 120);
            }
          }
        }
        // Cancellation alone is not completion for an action awaiting fresh state.
        const replacement = fetching.current;
        if (replacement && replacement !== request) await replacement.done;
      })();
      return request.done;
    },
    [cache, displaySession, drafts, requiredVersion],
  );
  useEffect(() => {
    if (stopped) return;
    alive.current = true;
    void refresh();
    const stream = new EventSource("/api/events");
    stream.onopen = () => {
      streamOpen.current = true;
      setConnected(true);
      // The server sends a full changed event on every connection. Opening
      // the stream itself must not queue a duplicate of the initial read.
    };
    stream.addEventListener("changed", (event) => {
      let scope = changeRefresh(
        event.data,
        selection.current,
        versions.current.get(selection.current) || 0,
      );
      const change = parseChange(event.data);
      // An event may be queued on a connection to an older server, or may
      // announce a new server before its /state response arrives. Only the
      // accepted state establishes which instance owns the revision numbers.
      if (
        change?.server_instance &&
        change.server_instance !== serverInstance.current
      ) {
        setSessionReady(false);
        scope = FULL_REFRESH;
      } else if (change) {
        if (change.session === null)
          globalVersion.current = Math.max(
            globalVersion.current,
            change.revision,
          );
        else
          expectedVersions.current.set(
            change.session,
            Math.max(
              expectedVersions.current.get(change.session) || 0,
              change.revision,
            ),
          );
        if (
          scope.session &&
          requiredVersion(selection.current) >
            (versions.current.get(selection.current) || 0)
        )
          setSessionReady(false);
      }
      if (!scope.state && !scope.session) return;
      pending.current = mergeRefresh(pending.current, scope);
      if (timer.current) return;
      timer.current = setTimeout(() => {
        timer.current = null;
        void refresh(false, NO_REFRESH);
      }, 100);
    });
    stream.onerror = () => {
      streamOpen.current = false;
      setConnected(false);
    };
    const poll = setInterval(() => {
      if (!streamOpen.current || recovering.current) {
        void refresh();
        return;
      }
      // Events carry ordinary changes. Reconcile occasionally for missed
      // events and external file edits, which do not emit server events.
      void refresh(false, {
        state: Date.now() - lastChecked.current.state >= 30000,
        session: Date.now() - lastChecked.current.session >= 15000,
      });
    }, 3000);
    const resume = () => {
      if (document.visibilityState === "visible") void refresh();
    };
    document.addEventListener("visibilitychange", resume);
    return () => {
      alive.current = false;
      streamOpen.current = false;
      fetching.current?.controller.abort();
      fetching.current = null;
      pending.current = NO_REFRESH;
      stream.close();
      clearInterval(poll);
      clearTimeout(timer.current);
      document.removeEventListener("visibilitychange", resume);
      timer.current = null;
    };
  }, [refresh, requiredVersion, stopped]);
  useEffect(() => {
    const narrow = window.matchMedia("(max-width: 1000px)");
    const closeOnNarrow = (event) => {
      if (event.matches && !inspectorDirty.current) setDetail(false);
    };
    narrow.addEventListener("change", closeOnNarrow);
    return () => narrow.removeEventListener("change", closeOnNarrow);
  }, []);
  // Track the chat and panel area so the panel limit follows window resizes.
  const workareaObserver = useRef(null);
  const workareaRef = useCallback((node) => {
    workareaObserver.current?.disconnect();
    workareaObserver.current = null;
    if (!node || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(([entry]) =>
      setWorkareaWidth(entry.contentRect.width),
    );
    observer.observe(node);
    workareaObserver.current = observer;
  }, []);
  const inspectorMax = maxInspectorWidth(workareaWidth || window.innerWidth);
  useEffect(() => {
    try {
      window.localStorage.setItem(INSPECTOR_WIDTH_KEY, String(inspectorWidth));
    } catch {
      // A private browsing context or disabled storage should not prevent
      // resizing the panel for the current page.
    }
  }, [inspectorWidth]);
  useEffect(() => {
    const warnBeforeUnload = (event) => {
      if (
        !settingsDirty.current &&
        !projectsDirty.current &&
        !inspectorDirty.current &&
        !drafts.hasText()
      )
        return;
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", warnBeforeUnload);
    return () => window.removeEventListener("beforeunload", warnBeforeUnload);
  }, [drafts]);
  function canLeavePage({ projectDraftForNewSession = false } = {}) {
    const dirty =
      (page === "settings" && settingsDirty.current) ||
      (page === "projects" && projectsDirty.current) ||
      (page === "chat" && inspectorDirty.current);
    if (!dirty) return true;
    return window.confirm(
      projectDraftForNewSession && page === "projects"
        ? "현재 입력한 프로젝트 정보를 새 세션에만 적용하고 이동할까요? 저장하지 않은 프로젝트 목록 변경 사항은 저장되지 않습니다."
        : "저장하지 않은 변경 사항을 버리고 이동할까요?",
    );
  }
  function showPage(next) {
    if (next !== page && !canLeavePage()) return;
    if (next !== page) navigationEpoch.current++;
    setPage(next);
    setMobileNav(false);
  }
  function choose(id, confirmed = false) {
    if (page === "chat" && id === selection.current && session?.id === id) {
      setMobileNav(false);
      return;
    }
    if (!confirmed && !canLeavePage()) return;
    navigationEpoch.current++;
    if (id === selection.current && session?.id === id) {
      setPage("chat");
      setMobileNav(false);
      return;
    }
    selectionEpoch.current++;
    selection.current = id;
    window.history.replaceState(null, "", `#${id}`);
    setSelected(id);
    displaySession(cache.get(id));
    setSessionReady(false);
    setPage("chat");
    setMobileNav(false);
    void refresh(true, {
      state: !workspace.current?.sessions.some((s) => s.id === id),
      session: true,
    });
  }
  async function act(fn) {
    setError("");
    try {
      return await fn();
    } catch (e) {
      setError(e.message);
      throw e;
    } finally {
      await refresh(true);
    }
  }
  const safe = (fn) => () => {
    void act(fn).catch(() => {});
  };
  async function runSession(id, text, action) {
    if (startingRun.current.has(id))
      throw new Error("이 세션의 실행 요청을 처리하고 있습니다.");
    startingRun.current.add(id);
    setRunPending(new Set(startingRun.current));
    try {
      return await act(() => send(`/sessions/${id}/run`, { text, action }));
    } finally {
      startingRun.current.delete(id);
      setRunPending(new Set(startingRun.current));
    }
  }
  function requestSession(project, { projectDraft = false } = {}) {
    if (
      !project ||
      creating.current ||
      !canLeavePage({ projectDraftForNewSession: projectDraft })
    )
      return;
    const saved = state.config.projects.find((item) => item.id === project.id);
    setNewSession({
      project:
        !projectDraft && saved ? { ...project, output: saved.output } : project,
    });
  }
  async function create(project, workflow) {
    if (creating.current || !newSession) return;
    creating.current = true;
    const navigation = navigationEpoch.current;
    setNavigating(true);
    try {
      const data = await send("/sessions", { project, workflow });
      setNewSession(null);
      if (navigationEpoch.current === navigation) choose(data.id, true);
    } finally {
      creating.current = false;
      setNavigating(false);
    }
  }
  async function closeSession(id) {
    const unsaved =
      drafts.get(id).text.trim() ||
      (id === selected && (settingsDirty.current || inspectorDirty.current));
    if (
      !window.confirm(
        `이 세션을 닫을까요? 대화와 기억은 사라지고 결과 문서는 유지됩니다.${unsaved ? " 저장하지 않은 변경 사항도 사라집니다." : ""}`,
      )
    )
      return;
    await send(`/sessions/${id}`, undefined, "DELETE");
  }
  async function shutdown() {
    if (
      !window.confirm(
        `진행 중인 작업을 중지하고 앱을 종료할까요? 저장된 문서는 유지되며 세션과 기억은 사라집니다.${settingsDirty.current || projectsDirty.current || inspectorDirty.current || drafts.hasText() ? " 저장하지 않은 변경 사항도 사라집니다." : ""}`,
      )
    )
      return;
    setStopping(true);
    try {
      await send("/shutdown", {});
      drafts.retain([]);
      cache.clear();
      versions.current.clear();
      expectedVersions.current.clear();
      globalVersion.current = 0;
      setStopped(true);
    } catch (e) {
      setError(e.message);
    } finally {
      setStopping(false);
    }
  }
  const current = state?.sessions.find((s) => s.id === selected);
  const running = state?.running || [];
  const runningById = new Map(running.map((run) => [run.id, run]));
  const activeCount = new Set([...runningById.keys(), ...runPending]).size;
  const capacityFull =
    activeCount >= (state?.config.max_concurrent_sessions ?? 4);
  const selectedBusy =
    runPending.has(selected) ||
    runningById.has(selected) ||
    (session?.id === selected && session.status === "running");
  const otherRunning = running.filter((run) => run.id !== selected);
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
        <button className="brand" onClick={() => showPage("chat")}>
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
            requestSession(current?.project || state.config.projects[0]),
          )}
          disabled={navigating || !state?.config.projects.length}
        >
          <span>＋</span>새 세션
        </button>
        <div className="sidebar-section">
          <span>프로젝트</span>
          <button
            aria-label="프로젝트 추가·관리"
            title="프로젝트 관리"
            onClick={() => showPage("projects")}
          >
            ＋
          </button>
        </div>
        <div className="session-list">
          {state?.config.projects.map((project) => (
            <div className="project-group" key={project.id}>
              <button
                className="project-heading"
                title={displayPath(project.root)}
                disabled={navigating}
                onClick={safe(() => requestSession(project))}
              >
                <span>▱</span>
                {project.name}
                <span className="project-plus">＋</span>
              </button>
              {state.sessions
                .filter((s) => s.project.id === project.id)
                .map((s) => (
                  <SessionButton
                    key={s.id}
                    session={s}
                    active={s.id === selected && page === "chat"}
                    onClick={() => choose(s.id)}
                    onClose={safe(() => closeSession(s.id))}
                    closing={runningById.get(s.id)?.closing}
                  />
                ))}
            </div>
          ))}
          {state?.sessions
            .filter(
              (s) => !state.config.projects.some((p) => p.id === s.project.id),
            )
            .map((s) => (
              <SessionButton
                key={s.id}
                session={s}
                active={s.id === selected}
                onClick={() => choose(s.id)}
                onClose={safe(() => closeSession(s.id))}
                closing={runningById.get(s.id)?.closing}
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
            onClick={() => showPage("projects")}
          >
            <span>▱</span>프로젝트 관리
          </button>
          <button
            className={page === "settings" ? "selected" : ""}
            onClick={() => showPage("settings")}
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
            {otherRunning.length > 0 && (
              <details className="running-menu" key={selected}>
                <summary className="running-link">
                  ● 다른 세션 {otherRunning.length}개 작업 중
                </summary>
                <div className="running-list">
                  {otherRunning.map((run) => {
                    const item = state.sessions.find((s) => s.id === run.id);
                    return (
                      <button key={run.id} onClick={() => choose(run.id)}>
                        {item?.title || item?.project.name || "새 대화"}
                        {run.closing ? " · 종료 중" : " · 작업 중"}
                      </button>
                    );
                  })}
                </div>
              </details>
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
                  onClick={() => {
                    if (detail && inspectorDirty.current && !canLeavePage())
                      return;
                    setDetail(!detail);
                  }}
                >
                  ☷
                </button>
              </>
            )}
          </div>
        </header>
        {(error || refreshError) && (
          <div className="app-error" role="alert">
            <span>{displayPathText(error || refreshError)}</span>
            <button
              aria-label="오류 닫기"
              onClick={() => {
                setError("");
                setRefreshError("");
              }}
            >
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
            sessionReady={sessionReady}
            onSaved={() => refresh(true)}
            onClose={() => showPage("chat")}
            onDirtyChange={onSettingsDirtyChange}
          />
        ) : page === "projects" ? (
          <Projects
            config={state.config}
            onSaved={() => refresh(true)}
            onDirtyChange={onProjectsDirtyChange}
            onCreate={(project) =>
              requestSession(project, { projectDraft: true })
            }
            creating={navigating}
          />
        ) : session ? (
          <div
            ref={workareaRef}
            className={`workarea ${detail ? "with-details" : ""}`}
            // Marks the read-only wait for fresh session state without a notice.
            aria-busy={sessionReady ? undefined : true}
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
                  <span className="workflow-badge">
                    {workflowOptions.find(
                      ([value]) => value === session.workflow_mode,
                    )?.[1] || session.workflow_mode}
                  </span>
                  <p title={displayPath(session.project.root)}>
                    {displayPath(session.project.root)}
                  </p>
                </div>
                <div className="session-actions">
                  <button
                    title="보존된 상태로 재개"
                    disabled={
                      navigating ||
                      !sessionReady ||
                      selectedBusy ||
                      capacityFull ||
                      !canRun ||
                      !session.can_resume
                    }
                    onClick={() =>
                      void runSession(selected, undefined, "resume").catch(
                        () => {},
                      )
                    }
                  >
                    재개
                  </button>
                  <button
                    title="기억과 상태 정리"
                    disabled={
                      navigating ||
                      !sessionReady ||
                      selectedBusy ||
                      capacityFull ||
                      !canRun ||
                      !session.has_task
                    }
                    onClick={() =>
                      void runSession(selected, undefined, "cleanup").catch(
                        () => {},
                      )
                    }
                  >
                    기억 정리
                  </button>
                  <button
                    className="danger-text"
                    disabled={runningById.get(selected)?.closing}
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
                drafts={drafts}
                busy={navigating || !sessionReady || selectedBusy}
                capacityFull={capacityFull}
                navigating={navigating}
                canRun={canRun}
                onSend={(text) => runSession(session.id, text, "message")}
                onCancel={safe(() => send(`/sessions/${selected}/cancel`, {}))}
                onSettings={() => showPage("settings")}
                onOlder={() =>
                  act(async () => {
                    const epoch = selectionEpoch.current;
                    const older = await api(
                      `/sessions/${selected}?before=${session.previous}`,
                    );
                    if (
                      selection.current === older.id &&
                      selectionEpoch.current === epoch
                    )
                      displaySession(mergeOlder(displayed.current, older));
                  })
                }
              />
            </div>
            {detail && (
              <Inspector
                key={session.id}
                session={session}
                tools={state.tools}
                running={runningById.get(session.id) || null}
                onAction={act}
                onProjectDirtyChange={onInspectorDirtyChange}
                navigating={navigating || !sessionReady}
                width={Math.min(inspectorWidth, inspectorMax)}
                maxWidth={inspectorMax}
                onWidthChange={(next) =>
                  setInspectorWidth(clampInspectorWidth(next, inspectorMax))
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
            {!state.sessions.length && state.config.projects.length > 0 && (
              <button
                className="primary"
                onClick={() => requestSession(state.config.projects[0])}
              >
                새 세션 시작
              </button>
            )}
            <button onClick={() => showPage("projects")}>프로젝트 관리</button>
          </div>
        )}
      </main>
      {newSession && (
        <NewSession
          project={newSession.project}
          projects={state.config.projects}
          onCreate={create}
          onClose={() => setNewSession(null)}
        />
      )}
    </div>
  );
}
function SessionButton({ session, active, onClick, onClose, closing }) {
  const title = session.title || "새 대화";
  return (
    <div className={`session-row ${active ? "active" : ""}`}>
      <button className="session-button" onClick={onClick} title={title}>
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
function Projects({ config, onSaved, onCreate, onDirtyChange, creating }) {
  const empty = {
    name: "새 프로젝트",
    root: "",
    output: "docs/source-summary.md",
    include: [],
    exclude: [".env*"],
    purpose: "프로젝트 구조, 주요 흐름과 오류 처리를 소스 근거와 함께 설명",
    audience: "신규 개발자",
  };
  const [list, setList, markSaved] = useServerDraft(config.projects);
  const [index, setIndex] = useState(0),
    [error, setError] = useState(""),
    [message, setMessage] = useState(""),
    [busy, setBusy] = useState(false),
    [dirty, setDirtyState] = useState(false);
  const draftVersion = useRef(0),
    saving = useRef(false),
    mounted = useRef(true);
  if (index >= list.length && index !== 0) setIndex(0);
  function setDirty(value) {
    if (!mounted.current) return;
    if (value) draftVersion.current++;
    setDirtyState(value);
    onDirtyChange(value);
  }
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      onDirtyChange(false);
    };
  }, [onDirtyChange]);
  const project = list[index];
  async function save() {
    if (saving.current) return;
    saving.current = true;
    const submittedVersion = draftVersion.current;
    setBusy(true);
    setError("");
    try {
      await send(
        "/settings",
        { config: { ...config, projects: list.map(cleanProject) } },
        "PUT",
      );
      markSaved(list);
      const unchanged = submittedVersion === draftVersion.current;
      setMessage(
        "프로젝트를 저장했습니다. 새 세션에 적용됩니다." +
          (unchanged ? "" : " 이후 변경 사항은 아직 저장하지 않았습니다."),
      );
      if (unchanged) setDirty(false);
      await onSaved();
    } catch (e) {
      setError(e.message);
    } finally {
      saving.current = false;
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
          disabled={creating}
          onClick={() => {
            setList([...list, { ...empty, id: crypto.randomUUID() }]);
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
              key={p.id}
              className={i === index ? "active" : ""}
              onClick={() => setIndex(i)}
              disabled={creating}
            >
              {p.name || "이름 없는 프로젝트"}
            </button>
          ))}
        </nav>
        <div className="settings-body">
          {project ? (
            <>
              <ProjectForm
                key={index}
                project={project}
                disabled={creating}
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
                  disabled={creating || list.length <= 1}
                >
                  목록에서 삭제
                </button>
                <span />
                <button
                  className="secondary"
                  disabled={busy || creating}
                  onClick={() => onCreate(cleanProject(project))}
                >
                  이 프로젝트로 새 세션
                </button>
                <button
                  className="primary"
                  disabled={busy || creating}
                  onClick={save}
                >
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
              {displayPathText(error)}
            </p>
          )}
        </div>
      </div>
    </section>
  );
}
function InspectorResizeHandle({ width, maxWidth, onWidthChange }) {
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
      aria-valuemax={maxWidth}
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
          onWidthChange(maxWidth);
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
  onProjectDirtyChange,
  navigating,
  width,
  maxWidth,
  onWidthChange,
}) {
  const [toolSelection, setToolSelection] = useState(session.active_tools);
  const [toolsSaving, setToolsSaving] = useState(false);
  const [downloading, setDownloading] = useState(false);
  const [outputLoading, setOutputLoading] = useState(false);
  const [projectDirty, setProjectDirty] = useState(false);
  const [projectSaving, setProjectSaving] = useState(false);
  const [projectMessage, setProjectMessage] = useState("");
  const projectSavingRef = useRef(false),
    mounted = useRef(true);
  const activeToolsKey = JSON.stringify(session.active_tools);
  useEffect(() => {
    if (!toolsSaving) setToolSelection(JSON.parse(activeToolsKey));
  }, [activeToolsKey, toolsSaving]);
  const [tab, setTab] = useState("memory"),
    [query, setQuery] = useState(""),
    [memoryId, setMemoryId] = useState(null),
    [memory, setMemory] = useState(null),
    [document, setDocument] = useState(null);
  const [project, setProject, markProjectSaved] = useServerDraft(
    session.project,
  );
  const outputKey = JSON.stringify([
    session.project.root,
    session.project.output,
  ]);
  const outputRequest = useRef(0),
    outputIdentity = useRef(outputKey);
  outputIdentity.current = outputKey;
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      onProjectDirtyChange(false);
    };
  }, [onProjectDirtyChange]);
  useEffect(() => {
    setDocument(null);
    setOutputLoading(false);
    return () => {
      outputRequest.current++;
    };
  }, [outputKey]);
  async function loadDocument() {
    const request = ++outputRequest.current;
    setOutputLoading(true);
    setDocument(null);
    try {
      const next = await api(`/sessions/${session.id}/output`);
      if (
        request === outputRequest.current &&
        outputIdentity.current === outputKey
      )
        setDocument(next);
    } catch (error) {
      if (
        request === outputRequest.current &&
        outputIdentity.current === outputKey
      )
        throw error;
    } finally {
      if (request === outputRequest.current) setOutputLoading(false);
    }
  }
  async function saveProject() {
    if (projectSavingRef.current) return;
    projectSavingRef.current = true;
    setProjectSaving(true);
    setProjectMessage("");
    try {
      await send(
        `/sessions/${session.id}/project`,
        cleanProject(project),
        "PUT",
      );
      markProjectSaved(project);
      if (mounted.current) {
        setProjectDirty(false);
        onProjectDirtyChange(false);
        setProjectMessage("현재 세션에 적용했습니다.");
      }
    } finally {
      projectSavingRef.current = false;
      setProjectSaving(false);
    }
  }
  const memoryKey = JSON.stringify(
    session.memories.find((item) => item.id === memoryId) || null,
  );
  useEffect(() => {
    setMemory(null);
    if (!memoryId) return;
    if (memoryKey === "null") {
      setMemoryId(null);
      return;
    }
    let active = true;
    void onAction(async () => {
      try {
        const result = await api(
          `/sessions/${session.id}/memories/${memoryId}`,
        );
        if (active) setMemory(result);
      } catch (error) {
        if (active) {
          setMemoryId(null);
          throw error;
        }
      }
    }).catch(() => {});
    return () => {
      active = false;
    };
  }, [session.id, memoryId, memoryKey]);
  const doAction = (fn) => () => {
    void onAction(fn).catch(() => {});
  };
  const memories = session.memories.filter((m) => memoryMatchesQuery(m, query));
  const recovering =
    session.status === "running" &&
    session.run_guidance?.progress_recovery?.active;
  const todos = session.task.todos || [];
  const currentTodo = todos.find((item) => !item.done);
  return (
    <aside className="inspector">
      <InspectorResizeHandle
        width={width}
        maxWidth={maxWidth}
        onWidthChange={onWidthChange}
      />
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
                <button
                  className="text-button"
                  onClick={() => setMemoryId(null)}
                >
                  ← 목록으로
                </button>
                <h3>{displayPathText(memory.title)}</h3>
                <small>
                  개정 {memory.revision} · {memory.status}
                </small>
                <Message role="assistant" text={memory.body} />
                <h4>출처</h4>
                {memory.sources.map((source) => (
                  <div className="source-card" key={source.id}>
                    <code>
                      {displayPath(source.path) || "사용자 발언"}{" "}
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
                  onClick={() => setMemoryId(m.id)}
                >
                  <div>
                    <strong>{displayPathText(m.title)}</strong>
                    <small>r{m.revision}</small>
                  </div>
                  <p>{displayPathText(m.summary)}</p>
                  <span>
                    {m.status === "needs_review"
                      ? "재검증 필요"
                      : m.status === "superseded"
                        ? "대체됨"
                        : "사용 가능"}
                  </span>
                  {m.tags.map((t) => (
                    <span className="tag" key={t}>
                      {displayPathText(t)}
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
                      <span>{displayPathText(item.text)}</span>
                      {item.result && (
                        <small>{displayPathText(item.result)}</small>
                      )}
                      {item.reopen_reason && (
                        <small>
                          재개 사유: {displayPathText(item.reopen_reason)}
                        </small>
                      )}
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
                    <li key={i}>{displayPathText(gap)}</li>
                  ))}
                </ul>
              </section>
            )}
            {session.workflow_mode === "source_document" &&
              session.completion_review?.required && (
                <section
                  className="progress-section"
                  aria-label="완료 조건 검증"
                >
                  <h4>완료 조건 검증</h4>
                  <p role="status">
                    {session.completion_review.pending
                      ? "실제 결과를 검증하고 있습니다."
                      : session.completion_review.approved
                        ? "모든 완료 조건의 검증을 통과했습니다."
                        : session.completion_review.needs_review
                          ? "현재 결과의 완료 조건을 다시 확인해야 합니다."
                          : "할 일 완료 후에도 조건이 충족될 때까지 보완합니다."}
                  </p>
                  <ul>
                    {(session.completion_review.checks || []).map((check) => (
                      <li key={check.id}>
                        <strong>
                          {
                            {
                              met: "충족",
                              unmet: "미충족",
                              unverified: "확인 불가",
                            }[check.status]
                          }
                          {" · "}
                          {check.id === "R0"
                            ? "현재 요청"
                            : displayPathText(check.criterion) || "완료 조건"}
                        </strong>
                        <p>{displayPathText(check.reason)}</p>
                        {check.next_action && (
                          <p>보완: {displayPathText(check.next_action)}</p>
                        )}
                      </li>
                    ))}
                  </ul>
                  <small className="subtle">
                    최근 검증 결과입니다. 결과물이나 조건이 바뀌면 다시
                    확인합니다.
                  </small>
                </section>
              )}
            <section className="progress-section" aria-label="현재 작업 목표">
              <h4>현재 작업 목표</h4>
              <p>{session.current_goal || session.task.purpose}</p>
            </section>
            {session.task_amendments?.length > 0 && (
              <details className="progress-section">
                <summary>
                  요청 변경 이력 ({session.task_amendments.length})
                </summary>
                <ol>
                  {session.task_amendments.map((change, index) => (
                    <li key={index}>{change.request}</li>
                  ))}
                </ol>
              </details>
            )}
            {["constraints", "completion", "findings", "unresolved"].map(
              (key, i) => (
                <section className="progress-section" key={key}>
                  <h4>
                    {["제약", "완료 조건", "발견한 내용", "미확인 사항"][i]}
                  </h4>
                  {session.task[key].length ? (
                    <ul>
                      {session.task[key].map((v, i) => (
                        <li key={i}>{displayPathText(v)}</li>
                      ))}
                    </ul>
                  ) : (
                    <p className="subtle">아직 등록된 내용이 없습니다.</p>
                  )}
                </section>
              ),
            )}
            {session.workflow_mode === "source_document" && <h4>조사 목록</h4>}
            {session.investigations.map((item) => (
              <div className="source-card" key={item.id}>
                <strong>{displayPathText(item.title)}</strong>
                <small>
                  {{
                    uninvestigated: "미조사",
                    in_progress: "조사 중",
                    written: "작성됨",
                    verified: "검증됨",
                    gap: "미확인",
                    superseded: "변경된 범위에서 제외",
                  }[item.status] || item.status}
                </small>
                <p>{displayPathText(item.note)}</p>
              </div>
            ))}
          </>
        )}
        {tab === "tools" && (
          <>
            <h3>사용할 도구</h3>
            <p className="subtle">변경 사항은 다음 요청부터 적용됩니다.</p>
            {tools
              // Tools the selected workflow excludes are hidden.
              .filter(
                (tool) =>
                  !(session.workflow_forbidden_tools ?? []).includes(tool.name),
              )
              .map((tool) => (
                <label className="tool-toggle" key={tool.name}>
                  <input
                    type="checkbox"
                    aria-label={toolLabels[tool.name] || tool.name}
                    checked={
                      !tool.optional || toolSelection.includes(tool.name)
                    }
                    disabled={navigating || !tool.optional || toolsSaving}
                    onChange={(e) => {
                      if (navigating || toolsSaving) return;
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
                        .catch(() => {})
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
                        {displayPath(session.project.root)}
                        <br />
                        설정된 결과 문서: {displayPath(session.project.output)}
                        <br />
                        결과 문서가 프로젝트 밖에 있으면 문서 구조
                        조회(document_inspect)가 반환한 절대 경로를 그대로
                        사용하세요. 파일명만 넘기면 프로젝트 안에서 찾습니다.
                        <br />
                        시작 줄은 start_line, 읽을 줄 수는 max_lines입니다.
                        limit은 max_lines의 별칭이며 offset은 줄 번호가
                        아닙니다. 잘린 결과는 반환된 cursor로 이어 읽으세요.
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
                        경로 입력 없이 설정된 결과 문서를 조회합니다. 섹션
                        제목은 #을 생략해도 유일하면 찾습니다. 중복이면 #을
                        포함한 실제 제목으로 구분하세요.
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
            <p className="directory-path">
              {displayPath(session.project.output)}
            </p>
            <button
              className="secondary"
              disabled={outputLoading}
              onClick={doAction(loadDocument)}
            >
              문서 불러오기
            </button>
            {document && (
              <>
                <button
                  className="text-button"
                  disabled={downloading}
                  onClick={doAction(async () => {
                    setDownloading(true);
                    try {
                      const blob = await download(
                        `/sessions/${encodeURIComponent(session.id)}/output/download`,
                      );
                      const url = URL.createObjectURL(blob);
                      const link = Object.assign(
                        window.document.createElement("a"),
                        {
                          href: url,
                          download:
                            session.project.output.split(/[\\/]/).pop() ||
                            "output.md",
                        },
                      );
                      window.document.body.appendChild(link);
                      link.click();
                      link.remove();
                      setTimeout(() => URL.revokeObjectURL(url), 1000);
                    } finally {
                      setDownloading(false);
                    }
                  })}
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
              onChange={(next) => {
                if (projectSavingRef.current) return;
                setProject(next);
                setProjectDirty(true);
                onProjectDirtyChange(true);
                setProjectMessage("");
              }}
              disabled={
                navigating || running?.id === session.id || projectSaving
              }
            />
            <button
              className="primary"
              disabled={
                navigating || running?.id === session.id || projectSaving
              }
              onClick={doAction(saveProject)}
            >
              현재 세션에 적용
            </button>
            {projectDirty && (
              <p className="subtle">저장하지 않은 변경 사항이 있습니다.</p>
            )}
            {projectMessage && (
              <p role="status" className="success-message">
                {projectMessage}
              </p>
            )}
          </>
        )}
      </div>
    </aside>
  );
}
