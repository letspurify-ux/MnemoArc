use crate::{
    agent::{self, AgentEvent, RunCommand},
    config::{Config, Project},
    llm::OpenAiClient,
    session::Session,
};
use anyhow::{Result, bail};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    DefaultTerminal,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Running {
    id: String,
    cancel: CancellationToken,
    commands: mpsc::Sender<RunCommand>,
}
struct App {
    config: Config,
    config_path: PathBuf,
    sessions: Vec<Session>,
    view: usize,
    input: String,
    notice: String,
    tab: usize,
    scroll: u16,
    memory_view: Option<String>,
    stream: BTreeMap<String, String>,
    running: Option<Running>,
    close_after: Option<String>,
    quitting: bool,
}
impl App {
    fn new(mut config: Config, config_path: PathBuf) -> Result<Self> {
        if config.projects.is_empty() {
            config.projects.push(Project {
                root: std::env::current_dir()?,
                ..Default::default()
            });
        }
        let session = Session::new(config.projects[0].clone(), config.clone());
        Ok(Self {
            config,
            config_path,
            sessions: vec![session],
            view: 0,
            input: String::new(),
            notice: "/help for commands. Set model and model_context before running.".into(),
            tab: 0,
            scroll: 0,
            memory_view: None,
            stream: BTreeMap::new(),
            running: None,
            close_after: None,
            quitting: false,
        })
    }
    fn selected(&self) -> Option<&Session> {
        self.sessions.get(self.view)
    }
    fn viewing_running(&self) -> bool {
        self.selected()
            .is_some_and(|s| self.running.as_ref().is_some_and(|r| r.id == s.id))
    }
    fn detail(&self) -> String {
        let Some(s) = self.selected() else {
            return "Use /new to create a session".into();
        };
        match self.tab {
            0 => serde_json::to_string_pretty(&s.task).unwrap(),
            1 => {
                if let Some(id) = &self.memory_view {
                    return s.memory.get(id).map_or_else(
                        |e| e.to_string(),
                        |m| serde_json::to_string_pretty(m).unwrap(),
                    );
                }
                s.memory
                    .recent(s.config.memory_count)
                    .into_iter()
                    .map(|m| {
                        format!(
                            "{} r{} {:?}\n{}\n{}\n",
                            m.id, m.revision, m.status, m.title, m.summary
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            2 => format!(
                "Active tools:\n{}\n\nInvestigations:\n{}",
                s.active_tools
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
                serde_json::to_string_pretty(&s.investigations).unwrap()
            ),
            3 => {
                let context = crate::context::ContextManager::request(
                    s,
                    crate::tools::ToolRegistry::definitions(s),
                )
                .map(|v| crate::context::count(&v, &s.config.model));
                format!(
                    "State: {}\nContext input: {:?} / {}\nTokenizer: {}\nInput: {}\nOutput: {}\nCached: {:?}\nUsage estimated/incomplete: {}\nMemory: {} entries / {} bytes\nHistory: {} bytes\nPruned through: {:?}\nCheckpoint: {:?}\nError: {:?}",
                    s.status,
                    context,
                    s.config.context_tokens,
                    if tiktoken_rs::tokenizer::get_tokenizer(&s.config.model).is_some() {
                        "known"
                    } else {
                        "conservative byte estimate"
                    },
                    s.input_tokens,
                    s.output_tokens,
                    s.cached_tokens,
                    s.usage_incomplete,
                    s.memory.entries.len(),
                    s.memory.bytes(),
                    s.history.bytes(),
                    s.history.pruned_through,
                    s.checkpoint,
                    s.last_error
                )
            }
            _ => format!(
                "SESSION SETTINGS\n{}\nPROJECT\n{}",
                toml::to_string_pretty(&s.config).unwrap_or_default(),
                toml::to_string_pretty(&s.project).unwrap_or_default()
            ),
        }
    }
    fn transcript(&self) -> String {
        let Some(s) = self.selected() else {
            return String::new();
        };
        let mut rows = vec![];
        for bundle in &s.history.bundles {
            for m in &bundle.messages {
                if let Some(role) = m["role"].as_str() {
                    let content = m["content"].as_str().unwrap_or("");
                    rows.push(if role == "tool" {
                        format!("[tool] {}", content.chars().take(400).collect::<String>())
                    } else {
                        format!("[{role}] {content}")
                    });
                }
            }
        }
        if let Some(text) = self.stream.get(&s.id)
            && !text.is_empty()
        {
            rows.push(format!("[streaming]\n{text}"));
        }
        rows.join("\n\n")
    }
    fn remove_session(&mut self, id: &str) {
        self.sessions.retain(|s| s.id != id);
        self.stream.remove(id);
        self.view = self.view.min(self.sessions.len().saturating_sub(1));
    }
    async fn command(
        &mut self,
        input: String,
        events: &mpsc::Sender<AgentEvent>,
        done: &mpsc::Sender<Session>,
        probe: &mpsc::Sender<String>,
    ) -> Result<()> {
        if input == "/help" {
            self.notice="/new [project index] · /project PATH · /project-set KEY JSON · /output PATH · /close · /memory [ID] · /settings · /set KEY JSON · /temp KEY JSON · /save · /check · /resume · /cleanup".into();
        } else if input == "/settings" {
            self.tab = 4;
            self.scroll = 0;
        } else if input == "/save" {
            self.config.save(&self.config_path)?;
            self.notice = format!(
                "Saved {}. Temporary settings and session data are not saved.",
                self.config_path.display()
            );
        } else if input.starts_with("/set ") || input.starts_with("/temp ") {
            let temporary = input.starts_with("/temp ");
            let rest = input.split_once(' ').unwrap().1;
            let (key, raw) = rest
                .split_once(' ')
                .ok_or_else(|| anyhow::anyhow!("Use /set key JSON-value"))?;
            let value = serde_json::from_str(raw).unwrap_or(serde_json::Value::String(raw.into()));
            let base = if temporary {
                &self
                    .selected()
                    .ok_or_else(|| anyhow::anyhow!("No session"))?
                    .config
            } else {
                &self.config
            };
            let mut updated = serde_json::to_value(base)?;
            updated[key] = value;
            let config: Config = serde_json::from_value(updated)?;
            config.validate()?;
            if self.viewing_running() {
                self.running
                    .as_ref()
                    .unwrap()
                    .commands
                    .send(RunCommand::Configure(config.clone()))
                    .await?;
                self.notice="Settings queued for next request boundary; old settings remain during cleanup if required".into();
            } else if let Some(s) = self.sessions.get_mut(self.view) {
                match agent::apply_config(s, config.clone()) {
                    Ok(()) => self.notice = "Settings applied".into(),
                    Err(error) => {
                        s.pending_config = Some(config.clone());
                        self.notice = format!(
                            "Settings pending: {error}. /cleanup or /resume will preserve necessary memory before applying."
                        );
                    }
                }
            }
            if !temporary {
                self.config = config;
            }
        } else if let Some(root) = input.strip_prefix("/project ") {
            let root = PathBuf::from(root).canonicalize()?;
            if !root.is_dir() {
                bail!("Project must be a directory");
            }
            let project = Project {
                name: root
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                root,
                ..Default::default()
            };
            self.config.projects.push(project.clone());
            self.sessions
                .push(Session::new(project, self.config.clone()));
            self.view = self.sessions.len() - 1;
            self.notice = "Project added; /save persists project settings".into();
        } else if input == "/new" || input.starts_with("/new ") {
            let index = input
                .strip_prefix("/new ")
                .unwrap_or("0")
                .parse::<usize>()?;
            let project = self
                .config
                .projects
                .get(index)
                .ok_or_else(|| {
                    anyhow::anyhow!("Project index not found; inspect projects in /settings")
                })?
                .clone();
            self.sessions
                .push(Session::new(project, self.config.clone()));
            self.view = self.sessions.len() - 1;
        } else if input == "/close" {
            if let Some(s) = self.selected() {
                let id = s.id.clone();
                if self.viewing_running() {
                    self.running.as_ref().unwrap().cancel.cancel();
                    self.close_after = Some(id);
                } else {
                    self.remove_session(&id);
                }
            }
        } else if input.starts_with("/output ") || input.starts_with("/project-set ") {
            if self.viewing_running() {
                bail!("Cancel current work before editing its project configuration");
            }
            let (key, raw) = if let Some(path) = input.strip_prefix("/output ") {
                ("output", path)
            } else {
                input
                    .strip_prefix("/project-set ")
                    .unwrap()
                    .split_once(' ')
                    .ok_or_else(|| anyhow::anyhow!("Use /project-set key JSON-value"))?
            };
            let s = self
                .sessions
                .get_mut(self.view)
                .ok_or_else(|| anyhow::anyhow!("No session"))?;
            let old_root = s.project.root.clone();
            let mut value = serde_json::to_value(&s.project)?;
            value[key] = serde_json::from_str(raw).unwrap_or(serde_json::Value::String(raw.into()));
            let mut project: Project = serde_json::from_value(value)?;
            project.root = project.root.canonicalize()?;
            if !project.root.is_dir() {
                bail!("Project must be a directory");
            }
            crate::tools::output_path(&project)?;
            for pattern in project.include.iter().chain(&project.exclude) {
                globset::Glob::new(pattern)?;
            }
            if key == "root" {
                // Changing a source root creates an isolated session rather than reusing old evidence.
                self.sessions
                    .push(Session::new(project.clone(), self.config.clone()));
                self.view = self.sessions.len() - 1;
            } else {
                s.project = project.clone();
            }
            if let Some(p) = self.config.projects.iter_mut().find(|p| p.root == old_root) {
                *p = project;
            }
            self.notice = "Project settings changed; /save persists them".into();
        } else if input == "/memory" {
            self.memory_view = None;
            self.tab = 1;
            self.scroll = 0;
        } else if let Some(id) = input.strip_prefix("/memory ") {
            let m = self
                .selected()
                .ok_or_else(|| anyhow::anyhow!("No session"))?
                .memory
                .get(id)?;
            self.memory_view = Some(m.id.clone());
            self.tab = 1;
            self.scroll = 0;
        } else if input == "/check" {
            let config = self
                .selected()
                .map_or_else(|| self.config.clone(), |s| s.config.clone());
            let tx = probe.clone();
            tokio::spawn(async move {
                let message = OpenAiClient
                    .probe(&config)
                    .await
                    .unwrap_or_else(|e| e.to_string());
                let _ = tx.send(message).await;
            });
            self.notice = "Testing plain response, streaming and tool round-trip…".into();
        } else {
            if self.running.is_some() {
                bail!(
                    "Another session is running. View switching is allowed; cancel before starting another run."
                );
            }
            if input.starts_with('/') && !matches!(input.as_str(), "/resume" | "/cleanup") {
                bail!("Unknown command; /help");
            }
            let s = self
                .sessions
                .get_mut(self.view)
                .ok_or_else(|| anyhow::anyhow!("Use /new first"))?;
            s.config.runnable()?;
            if input == "/cleanup" {
                s.add_user("Clean up unused or superseded memory and detailed state to fit pending settings. Preserve referenced evidence and user constraints. Do not alter project files.".into());
            } else if input != "/resume" {
                s.add_user(input);
            }
            if let Some(cp) = &mut s.checkpoint {
                cp.attempts = 0;
                cp.failed = false;
            }
            let cancel = CancellationToken::new();
            let (tx, rx) = mpsc::channel(16);
            self.running = Some(Running {
                id: s.id.clone(),
                cancel: cancel.clone(),
                commands: tx,
            });
            let session = s.clone();
            let events = events.clone();
            let done = done.clone();
            tokio::spawn(async move {
                let final_session = agent::run_session_controlled(
                    session,
                    Arc::new(OpenAiClient),
                    cancel,
                    events,
                    rx,
                )
                .await;
                let _ = done.send(final_session).await;
            });
            self.notice = "Running".into();
        }
        Ok(())
    }
}
fn draw(terminal: &mut DefaultTerminal, app: &App) -> Result<()> {
    terminal.draw(|f| {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(3),
                Constraint::Length(4),
            ])
            .split(f.area());
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(18),
                Constraint::Percentage(47),
                Constraint::Percentage(35),
            ])
            .split(outer[0]);
        let rows = app
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| {
                ListItem::new(format!(
                    "{} {}\n{} {}",
                    if i == app.view { "▶" } else { " " },
                    s.project.name,
                    &s.id[..8],
                    s.status
                ))
                .style(if i == app.view {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                })
            })
            .collect::<Vec<_>>();
        f.render_widget(
            List::new(rows).block(
                Block::default()
                    .title("Projects / sessions")
                    .borders(Borders::ALL),
            ),
            columns[0],
        );
        f.render_widget(
            Paragraph::new(app.transcript())
                .block(
                    Block::default()
                        .title("MnemoArc · Shift+Tab session · PgUp/PgDn scroll")
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: false })
                .scroll((app.scroll, 0)),
            columns[1],
        );
        let titles = [
            "Goal / progress",
            "Memory · /memory ID",
            "Tools / investigations",
            "Usage / errors",
            "Settings · /set /temp /project-set",
        ];
        f.render_widget(
            Paragraph::new(app.detail())
                .block(
                    Block::default()
                        .title(titles[app.tab])
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: false })
                .scroll((app.scroll, 0)),
            columns[2],
        );
        f.render_widget(
            Paragraph::new(app.input.as_str()).block(
                Block::default()
                    .title("Request or command · Tab panel · Esc cancel · Ctrl+Q quit")
                    .borders(Borders::ALL),
            ),
            outer[1],
        );
        f.render_widget(
            Paragraph::new(app.notice.as_str())
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL)),
            outer[2],
        );
    })?;
    Ok(())
}
pub async fn run(config: Config, path: PathBuf) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = run_inner(&mut terminal, config, path).await;
    ratatui::restore();
    result
}
async fn run_inner(terminal: &mut DefaultTerminal, config: Config, path: PathBuf) -> Result<()> {
    let mut app = App::new(config, path)?;
    let mut keys = EventStream::new();
    let (events, mut event_rx) = mpsc::channel(128);
    let (done, mut done_rx) = mpsc::channel::<Session>(4);
    let (probe_tx, mut probe_rx) = mpsc::channel::<String>(4);
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        tokio::select! {
            _=tick.tick()=>draw(terminal,&app)?,
            Some(message)=probe_rx.recv()=>app.notice=message,
            Some(event)=event_rx.recv()=>match event {
                AgentEvent::Delta{session,text}=>app.stream.entry(session).or_default().push_str(&text),
                AgentEvent::Snapshot(s)=>if let Some(old)=app.sessions.iter_mut().find(|x|x.id==s.id){*old = *s;},
                AgentEvent::Tool{session,name,status}=>{app.stream.remove(&session);app.notice=format!("{name}: {status}");},
                AgentEvent::Notice{text,..}=>app.notice=text,
            },
            Some(session)=done_rx.recv()=>{
                app.stream.remove(&session.id);app.running=None;
                app.notice=format!("{}{}",session.status,session.last_error.as_ref().map(|e|format!(": {e}")).unwrap_or_default());
                if app.close_after.take().as_deref()==Some(session.id.as_str()){app.remove_session(&session.id);}else if let Some(old)=app.sessions.iter_mut().find(|s|s.id==session.id){*old=session;}
                if app.quitting {break;}
            },
            event=keys.next()=>{
                let Some(Ok(Event::Key(key)))=event else{continue};if key.kind!=KeyEventKind::Press{continue;}
                match key.code {
                    KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL)=>if let Some(running)=&app.running{running.cancel.cancel();app.quitting=true;app.notice="Cancelling active work before exit…".into();}else{break;},
                    KeyCode::Esc=>if let Some(running)=&app.running{running.cancel.cancel();app.notice="Cancelling; completed writes remain saved".into();},
                    KeyCode::BackTab=>if !app.sessions.is_empty(){app.view=(app.view+1)%app.sessions.len();app.scroll=0;app.memory_view=None;},
                    KeyCode::Tab=>{app.tab=(app.tab+1)%5;app.scroll=0;},
                    KeyCode::PageDown=>app.scroll=app.scroll.saturating_add(10),
                    KeyCode::PageUp=>app.scroll=app.scroll.saturating_sub(10),
                    KeyCode::Backspace=>{app.input.pop();},
                    KeyCode::Char(c)=>app.input.push(c),
                    KeyCode::Enter=>{let input=std::mem::take(&mut app.input);if !input.trim().is_empty()&& let Err(e)=app.command(input,&events,&done,&probe_tx).await{app.notice=e.to_string();}},
                    _=>{},
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn viewing_another_session_does_not_cancel_and_busy_start_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            ..Default::default()
        };
        let mut app = App::new(config, directory.path().join("config.toml")).unwrap();
        let original = app.sessions[0].id.clone();
        let cancel = CancellationToken::new();
        let (commands, _command_rx) = mpsc::channel(4);
        app.running = Some(Running {
            id: original.clone(),
            cancel: cancel.clone(),
            commands,
        });
        let (events, _event_rx) = mpsc::channel(8);
        let (done, _done_rx) = mpsc::channel(4);
        let (probe, _probe_rx) = mpsc::channel(4);
        app.command("/new".into(), &events, &done, &probe)
            .await
            .unwrap();
        assert_ne!(app.selected().unwrap().id, original);
        assert!(!cancel.is_cancelled());
        assert!(
            app.command("another task".into(), &events, &done, &probe)
                .await
                .is_err()
        );
        app.command("/close".into(), &events, &done, &probe)
            .await
            .unwrap();
        assert_eq!(app.sessions.len(), 1);
        assert!(!cancel.is_cancelled());
        app.command("/close".into(), &events, &done, &probe)
            .await
            .unwrap();
        assert!(cancel.is_cancelled());
        assert_eq!(app.close_after.as_deref(), Some(original.as_str()));
        assert_eq!(
            app.sessions.len(),
            1,
            "wait for execution cleanup before deleting RAM"
        );
    }
    #[tokio::test]
    async fn temporary_settings_and_session_history_are_not_saved() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut app = App::new(Config::default(), path.clone()).unwrap();
        let (events, _event_rx) = mpsc::channel(8);
        let (done, _done_rx) = mpsc::channel(4);
        let (probe, _probe_rx) = mpsc::channel(4);
        app.sessions[0].add_user("private session content".into());
        app.command("/temp run_tokens 123456".into(), &events, &done, &probe)
            .await
            .unwrap();
        assert_eq!(app.selected().unwrap().config.run_tokens, 123456);
        app.command("/save".into(), &events, &done, &probe)
            .await
            .unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        let saved: Config = toml::from_str(&text).unwrap();
        assert_eq!(saved.run_tokens, Config::default().run_tokens);
        assert!(!text.contains("private session content"));
    }
}
