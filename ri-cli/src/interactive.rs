//! Interactive TUI for agent sessions.
//!
//! Uses a small ratatui Viewport::Inline pinned to the bottom of the terminal.
//! Completed content is pushed to normal terminal scrollback via insert_before.
//! The viewport only handles live concerns: streaming preview, input, and status.

use crate::agent::{self, AgentEvent};
use ri::{AuthMethod, LlmProvider, Model, SessionStore, StreamEvent, ThinkingLevel, Tool, Usage};

use std::io::{self, Write};
use std::path::PathBuf;

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures::StreamExt;
use ratatui::{
    Frame, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap},
};
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::{Duration, interval};
use tui_textarea::TextArea;

/// Fixed height of the inline viewport at the bottom of the terminal.
const VIEWPORT_HEIGHT: u16 = 8;

// ---------------------------------------------------------------------------
// Phase
// ---------------------------------------------------------------------------

enum Phase {
    Input,
    Waiting,
    Thinking,
    Responding,
    Tool(String),
}

impl Phase {
    fn label(&self) -> &str {
        match self {
            Phase::Input => "input",
            Phase::Waiting => "waiting",
            Phase::Thinking => "thinking",
            Phase::Responding => "responding",
            Phase::Tool(_) => "tool",
        }
    }

    fn detail(&self) -> String {
        match self {
            Phase::Input => String::new(),
            Phase::Waiting => "sending request...".into(),
            Phase::Thinking => "reasoning...".into(),
            Phase::Responding => "writing...".into(),
            Phase::Tool(name) => format!("executing: {}", name),
        }
    }
}

// ---------------------------------------------------------------------------
// Content blocks — the conversation history rendered to scrollback
// ---------------------------------------------------------------------------

enum BlockKind {
    User,
    Assistant,
    Thinking,
    Tool { name: String, is_error: bool },
    Info,
    Error,
}

struct ContentBlock {
    kind: BlockKind,
    body: String,
}

// ---------------------------------------------------------------------------
// TUI state (separated from Terminal to avoid borrow conflicts in draw)
// ---------------------------------------------------------------------------

struct TuiState {
    phase: Phase,
    blocks: Vec<ContentBlock>,
    text_buf: String,
    thinking_buf: String,
    textarea: TextArea<'static>,
    total_usage: Usage,
    model_name: String,
    tick: usize,
    last_size: (u16, u16),
}

// ---------------------------------------------------------------------------
// TUI handle
// ---------------------------------------------------------------------------

struct Tui {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    state: TuiState,
}

impl Tui {
    fn new(model_name: String) -> eyre::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let size = crossterm::terminal::size()?;
        let height = VIEWPORT_HEIGHT.min(size.1);

        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )?;

        let mut tui = Self {
            terminal,
            state: TuiState {
                phase: Phase::Input,
                blocks: Vec::new(),
                text_buf: String::new(),
                thinking_buf: String::new(),
                textarea: new_textarea(),
                total_usage: Usage::default(),
                model_name,
                tick: 0,
                last_size: size,
            },
        };
        tui.draw()?;
        Ok(tui)
    }

    /// Push styled lines to scrollback above the viewport, then redraw.
    /// The emit and redraw are wrapped in synchronized output so the
    /// terminal displays the result atomically.
    fn emit_and_draw(&mut self, lines: Vec<Line<'static>>) -> io::Result<()> {
        if !lines.is_empty() {
            let width = self.terminal.size().map(|s| s.width).unwrap_or(80);
            let height = wrapped_height(&lines, width);
            if height > 0 {
                sync_start()?;
                self.terminal.insert_before(height, |buf| {
                    Paragraph::new(Text::from(lines))
                        .wrap(Wrap { trim: false })
                        .render(buf.area, buf);
                })?;
                self.draw_inner()?;
                sync_end()?;
                return Ok(());
            }
        }
        self.draw()
    }

    fn draw(&mut self) -> io::Result<()> {
        self.draw_inner()
    }

    fn draw_inner(&mut self) -> io::Result<()> {
        self.state.tick += 1;
        self.terminal
            .draw(|frame| render_viewport(frame, &self.state))?;
        Ok(())
    }

    /// Check if terminal size changed and re-render if so.
    fn check_resize(&mut self) -> io::Result<()> {
        let size = crossterm::terminal::size()?;
        if size != self.state.last_size {
            self.state.last_size = size;
            self.handle_resize()?;
        }
        Ok(())
    }

    fn handle_resize(&mut self) -> io::Result<()> {
        let (width, term_height) = self.state.last_size;

        // Clear scrollback + screen + cursor home.
        {
            let mut out = io::stdout().lock();
            write!(out, "\x1b[3J\x1b[2J\x1b[H")?;
            out.flush()?;
        }

        // Brief pause so the terminal processes the clear before we query
        // cursor position during Viewport::Inline initialization.
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Recreate terminal at the (possibly new) size.
        let height = VIEWPORT_HEIGHT.min(term_height);
        self.terminal = Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )?;

        // Re-render all blocks at the new width.
        for block in &self.state.blocks {
            let lines = render_block_content(block);
            let h = block_render_height(&block.kind, width, &lines);
            if h > 0 {
                let kind = &block.kind;
                self.terminal.insert_before(h, |buf| {
                    render_block_widget(kind, &lines, buf);
                })?;
            }
        }

        self.draw_inner()
    }

    // -- Block management --

    /// Render a completed block as a fancy widget and push to scrollback.
    /// The block is also stored for potential future re-rendering.
    fn emit_block(&mut self, block: ContentBlock) -> io::Result<()> {
        let width = self.terminal.size().map(|s| s.width).unwrap_or(80);
        let lines = render_block_content(&block);
        let h = block_render_height(&block.kind, width, &lines);
        if h > 0 {
            sync_start()?;
            let kind = &block.kind;
            self.terminal.insert_before(h, |buf| {
                render_block_widget(kind, &lines, buf);
            })?;
            self.draw_inner()?;
            sync_end()?;
        }
        self.state.blocks.push(block);
        Ok(())
    }

    // -- Agent events --

    fn handle_agent_event(&mut self, evt: &AgentEvent) -> io::Result<()> {
        match evt {
            AgentEvent::Stream(se) => self.handle_stream_event(se),
            AgentEvent::ToolStart { name, .. } => {
                self.state.phase = Phase::Tool(name.clone());
                self.draw()
            }
            AgentEvent::ToolEnd {
                output, is_error, ..
            } => {
                let name = match &self.state.phase {
                    Phase::Tool(n) => n.clone(),
                    _ => "tool".into(),
                };
                self.state.phase = Phase::Waiting;
                self.emit_block(ContentBlock {
                    kind: BlockKind::Tool {
                        name,
                        is_error: *is_error,
                    },
                    body: truncate(output, 50_000),
                })
            }
            AgentEvent::MessageComplete(_) => Ok(()),
            AgentEvent::Error(msg) => self.emit_block(ContentBlock {
                kind: BlockKind::Error,
                body: msg.clone(),
            }),
        }
    }

    fn handle_stream_event(&mut self, se: &StreamEvent) -> io::Result<()> {
        match se {
            StreamEvent::ThinkingStart => {
                self.state.thinking_buf.clear();
                self.state.phase = Phase::Thinking;
                self.draw()
            }
            StreamEvent::ThinkingDelta(d) => {
                self.state.thinking_buf.push_str(d);
                self.draw()
            }
            StreamEvent::ThinkingEnd { .. } => {
                let body = std::mem::take(&mut self.state.thinking_buf);
                self.state.phase = Phase::Waiting;
                if !body.is_empty() {
                    self.emit_block(ContentBlock {
                        kind: BlockKind::Thinking,
                        body,
                    })
                } else {
                    self.draw()
                }
            }
            StreamEvent::TextStart => {
                self.state.text_buf.clear();
                self.state.phase = Phase::Responding;
                self.draw()
            }
            StreamEvent::TextDelta(d) => {
                self.state.text_buf.push_str(d);
                self.draw()
            }
            StreamEvent::TextEnd { .. } => {
                let body = std::mem::take(&mut self.state.text_buf);
                self.state.phase = Phase::Waiting;
                if !body.is_empty() {
                    self.emit_block(ContentBlock {
                        kind: BlockKind::Assistant,
                        body,
                    })
                } else {
                    self.draw()
                }
            }
            StreamEvent::ToolCallStart { name, .. } => {
                self.state.phase = Phase::Tool(name.clone());
                self.draw()
            }
            StreamEvent::Usage(u) => {
                self.state.total_usage.input_tokens += u.input_tokens;
                self.state.total_usage.output_tokens += u.output_tokens;
                self.state.total_usage.cache_read_tokens += u.cache_read_tokens;
                self.state.total_usage.cache_write_tokens += u.cache_write_tokens;
                self.emit_block(ContentBlock {
                    kind: BlockKind::Info,
                    body: format!(
                        "tokens: {} in / {} out / {} cached",
                        u.input_tokens, u.output_tokens, u.cache_read_tokens
                    ),
                })
            }
            StreamEvent::Error(msg) => self.emit_block(ContentBlock {
                kind: BlockKind::Error,
                body: msg.clone(),
            }),
            _ => self.draw(),
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        // Clear the viewport so stale content doesn't linger after exit.
        let _ = self.terminal.draw(|frame| {
            frame.render_widget(Paragraph::new(""), frame.area());
        });
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

// ---------------------------------------------------------------------------
// Viewport rendering
// ---------------------------------------------------------------------------

fn render_viewport(frame: &mut Frame, state: &TuiState) {
    let area = frame.area();

    let chunks = Layout::vertical([
        Constraint::Min(0),    // main: preview or input
        Constraint::Length(1), // status bar
    ])
    .split(area);

    let main_area = chunks[0];
    let status_area = chunks[1];

    if matches!(state.phase, Phase::Input) {
        frame.render_widget(&state.textarea, main_area);
    } else {
        render_preview(frame, state, main_area);
    }

    render_status_bar(frame, state, status_area);
}

/// Render a live preview of in-progress streaming content.
fn render_preview(frame: &mut Frame, state: &TuiState, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    match &state.phase {
        Phase::Thinking if !state.thinking_buf.is_empty() => {
            let lines: Vec<Line<'static>> = state
                .thinking_buf
                .lines()
                .map(|l| Line::styled(l.to_string(), Style::default().add_modifier(Modifier::DIM)))
                .collect();
            render_preview_block(
                frame,
                area,
                "thinking",
                Style::default().fg(Color::DarkGray),
                lines,
            );
        }
        Phase::Responding if !state.text_buf.is_empty() => {
            let text = tui_markdown::from_str(&state.text_buf);
            let lines: Vec<Line<'static>> = text.lines.into_iter().map(own_line).collect();
            render_preview_block(
                frame,
                area,
                "assistant",
                Style::default().fg(Color::Blue),
                lines,
            );
        }
        Phase::Tool(name) => {
            let lines = vec![Line::styled(
                format!("executing: {}", name),
                Style::default().add_modifier(Modifier::DIM),
            )];
            render_preview_block(frame, area, name, Style::default().fg(Color::Yellow), lines);
        }
        _ => {
            let para = Paragraph::new(Text::from(vec![Line::styled(
                state.phase.detail(),
                Style::default().add_modifier(Modifier::DIM),
            )]))
            .wrap(Wrap { trim: false });
            frame.render_widget(para, area);
        }
    }
}

/// Render streaming content inside a bordered block, scrolled to the bottom.
fn render_preview_block(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    border_style: Style,
    lines: Vec<Line<'static>>,
) {
    let chrome = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(format!(" {} ", title))
        .border_style(border_style);
    let inner = chrome.inner(area);
    frame.render_widget(chrome, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let total = para.line_count(inner.width) as u16;
    let scroll = total.saturating_sub(inner.height);
    frame.render_widget(para.scroll((scroll, 0)), inner);
}

fn render_status_bar(frame: &mut Frame, state: &TuiState, area: Rect) {
    let spinner = spinner_frame(state.tick);

    let left = format!(
        " {} {} | {} ",
        spinner,
        state.phase.label(),
        state.model_name
    );
    let right = format!(
        " {}in/{}out/{}cache ",
        state.total_usage.input_tokens,
        state.total_usage.output_tokens,
        state.total_usage.cache_read_tokens,
    );

    let bar_width = area.width as usize;
    let content_len = left.len() + right.len();
    let padding = if bar_width > content_len {
        " ".repeat(bar_width - content_len)
    } else {
        String::new()
    };

    let bar = Line::from(vec![Span::raw(left), Span::raw(padding), Span::raw(right)])
        .style(Style::default().bg(Color::DarkGray).fg(Color::White));

    frame.render_widget(Paragraph::new(bar), area);
}

// ---------------------------------------------------------------------------
// Block rendering — each ContentBlock rendered as a ratatui widget
// ---------------------------------------------------------------------------

/// Convert a block's body into styled lines for rendering.
fn render_block_content(block: &ContentBlock) -> Vec<Line<'static>> {
    match &block.kind {
        BlockKind::User | BlockKind::Assistant => {
            let md = tui_markdown::from_str(&block.body);
            md.lines.into_iter().map(own_line).collect()
        }
        BlockKind::Thinking => block
            .body
            .lines()
            .map(|l| Line::styled(l.to_string(), Style::default().add_modifier(Modifier::DIM)))
            .collect(),
        BlockKind::Tool { is_error, .. } => {
            let style = if *is_error {
                Style::default().fg(Color::Red)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
            block
                .body
                .lines()
                .map(|l| Line::styled(l.to_string(), style))
                .collect()
        }
        BlockKind::Info => vec![Line::styled(
            block.body.clone(),
            Style::default().add_modifier(Modifier::DIM),
        )],
        BlockKind::Error => block
            .body
            .lines()
            .map(|l| Line::styled(l.to_string(), Style::default().fg(Color::Red)))
            .collect(),
    }
}

/// Compute the total rendered height of a block (content + chrome).
fn block_render_height(kind: &BlockKind, width: u16, lines: &[Line<'static>]) -> u16 {
    let (inner_width, chrome) = match kind {
        BlockKind::Info => (width, 0u16),
        _ => (width.saturating_sub(2).max(1), 2u16),
    };
    let para = Paragraph::new(Text::from(lines.to_vec())).wrap(Wrap { trim: false });
    para.line_count(inner_width) as u16 + chrome
}

/// Render a block widget into a Buffer (used inside insert_before closures).
fn render_block_widget(kind: &BlockKind, lines: &[Line<'static>], buf: &mut Buffer) {
    match kind {
        BlockKind::Info => {
            Paragraph::new(Text::from(lines.to_vec()))
                .wrap(Wrap { trim: false })
                .render(buf.area, buf);
        }
        _ => {
            let (title, style) = block_chrome(kind);
            let chrome = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(format!(" {} ", title))
                .border_style(style);
            let inner = chrome.inner(buf.area);
            chrome.render(buf.area, buf);
            Paragraph::new(Text::from(lines.to_vec()))
                .wrap(Wrap { trim: false })
                .render(inner, buf);
        }
    }
}

fn block_chrome(kind: &BlockKind) -> (String, Style) {
    match kind {
        BlockKind::User => ("you".into(), Style::default().fg(Color::Cyan)),
        BlockKind::Assistant => ("assistant".into(), Style::default().fg(Color::Blue)),
        BlockKind::Thinking => ("thinking".into(), Style::default().fg(Color::DarkGray)),
        BlockKind::Tool { name, is_error } => {
            let style = if *is_error {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Yellow)
            };
            (name.clone(), style)
        }
        BlockKind::Info => (String::new(), Style::default()),
        BlockKind::Error => ("error".into(), Style::default().fg(Color::Red)),
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

fn new_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_block(
        Block::default()
            .borders(Borders::TOP)
            .title(" ri> ")
            .style(Style::default().fg(Color::Cyan)),
    );
    ta.set_cursor_line_style(Style::default());
    ta
}

enum InputResult {
    Submit(String),
    Quit,
}

async fn read_input(tui: &mut Tui, events: &mut EventStream) -> io::Result<InputResult> {
    let mut sigwinch =
        signal(SignalKind::window_change()).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let mut size_poll = interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            event = events.next() => {
                match event {
                    Some(Ok(Event::Key(key))) => {
                        if key.code == KeyCode::Char('d')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            return Ok(InputResult::Quit);
                        }
                        if key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            return Ok(InputResult::Quit);
                        }
                        if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT) {
                            let text = tui.state.textarea.lines().join("\n");
                            tui.state.textarea = new_textarea();
                            return Ok(InputResult::Submit(text));
                        }
                        tui.state.textarea.input(key);
                        tui.draw()?;
                    }
                    Some(Ok(Event::Resize(_, _))) => {
                        tui.handle_resize()?;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(io::Error::new(io::ErrorKind::Other, e)),
                    None => return Ok(InputResult::Quit),
                }
            }
            _ = sigwinch.recv() => {
                tui.check_resize()?;
            }
            _ = size_poll.tick() => {
                tui.check_resize()?;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

pub async fn run(
    mut provider: Box<dyn LlmProvider>,
    model: Model,
    tools: Vec<Box<dyn Tool>>,
    cwd: PathBuf,
    initial_prompt: Option<String>,
    thinking: ThinkingLevel,
    mut seen_agents: std::collections::HashSet<PathBuf>,
) -> eyre::Result<()> {
    let session_name = session_name_from_prompt(initial_prompt.as_deref());
    let system_prompt = {
        let context_files = ri_tools::resources::discover_context_files(&cwd);
        ri_tools::resources::build_system_prompt(&context_files)
    };
    let cwd_str = cwd
        .to_str()
        .ok_or_else(|| eyre::eyre!("working directory contains non-UTF-8 characters"))?;
    let sessions_dir = SessionStore::default_dir()?;
    let mut store = SessionStore::new(sessions_dir);
    store.load_all()?;
    let file_id = store.create_session(&session_name, cwd_str, None, &[])?;
    let sys_msg = store.write_message(
        &file_id,
        ri::Role::System,
        vec![ri::ContentBlock::text(&system_prompt)],
        None,
        None,
    )?;
    let mut message_ids = vec![sys_msg.id];

    let mut tui = Tui::new(model.name.clone())?;
    let mut events = EventStream::new();

    if let Some(prompt) = initial_prompt {
        let templates = load_prompt_templates(&cwd);
        let expanded = ri_tools::prompts::expand_prompt(&prompt, &templates);
        run_prompt(
            &expanded,
            &mut tui,
            provider.as_ref(),
            &model,
            &tools,
            &mut store,
            &mut message_ids,
            &cwd,
            thinking,
            &file_id,
            &mut seen_agents,
            &mut events,
        )
        .await?;
    }

    loop {
        tui.state.phase = Phase::Input;
        tui.draw()?;

        match read_input(&mut tui, &mut events).await? {
            InputResult::Submit(text) => {
                let trimmed = text.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed == "/quit" || trimmed == "/exit" {
                    break;
                }
                if trimmed == "/help" {
                    let templates = load_prompt_templates(&cwd);
                    let help = help_text(&templates);
                    let md = tui_markdown::from_str(&help);
                    let lines = md.lines.into_iter().map(own_line).collect();
                    tui.emit_and_draw(lines)?;
                    continue;
                }
                if trimmed.starts_with("/login") {
                    handle_login(&trimmed, &model, &mut provider, &mut tui).await;
                    continue;
                }
                // Expand prompt templates (e.g. /task implement foo).
                let templates = load_prompt_templates(&cwd);
                let expanded = ri_tools::prompts::expand_prompt(&trimmed, &templates);
                run_prompt(
                    &expanded,
                    &mut tui,
                    provider.as_ref(),
                    &model,
                    &tools,
                    &mut store,
                    &mut message_ids,
                    &cwd,
                    thinking,
                    &file_id,
                    &mut seen_agents,
                    &mut events,
                )
                .await?;
            }
            InputResult::Quit => break,
        }
    }

    drop(tui);
    Ok(())
}

// ---------------------------------------------------------------------------
// Prompt submission + agent streaming
// ---------------------------------------------------------------------------

async fn run_prompt(
    text: &str,
    tui: &mut Tui,
    provider: &dyn LlmProvider,
    model: &Model,
    tools: &[Box<dyn Tool>],
    store: &mut SessionStore,
    message_ids: &mut Vec<String>,
    cwd: &PathBuf,
    thinking: ThinkingLevel,
    session_id: &str,
    seen_agents: &mut std::collections::HashSet<PathBuf>,
    term_events: &mut EventStream,
) -> eyre::Result<()> {
    tui.emit_block(ContentBlock {
        kind: BlockKind::User,
        body: text.to_string(),
    })?;

    tui.state.phase = Phase::Waiting;
    tui.draw()?;

    let cancel = tokio_util::sync::CancellationToken::new();
    let agent_stream = agent::submit(
        text,
        provider,
        model,
        tools,
        store,
        message_ids,
        cwd,
        thinking,
        session_id,
        seen_agents,
        cancel.clone(),
    )?;
    tokio::pin!(agent_stream);

    let mut sigwinch =
        signal(SignalKind::window_change()).map_err(|e| eyre::eyre!("signal setup: {}", e))?;
    let mut size_poll = interval(Duration::from_millis(500));

    loop {
        tokio::select! {
            agent_evt = agent_stream.next() => {
                match agent_evt {
                    Some(evt) => { tui.handle_agent_event(&evt)?; }
                    None => break,
                }
            }
            term_evt = term_events.next() => {
                match term_evt {
                    Some(Ok(Event::Key(key))) => {
                        if (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL))
                            || key.code == KeyCode::Esc
                        {
                            cancel.cancel();
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => {
                        tui.handle_resize()?;
                    }
                    _ => {}
                }
            }
            _ = sigwinch.recv() => {
                tui.check_resize()?;
            }
            _ = size_poll.tick() => {
                tui.check_resize()?;
            }
        }
    }

    // Emit any partial content as blocks (e.g. after cancellation).
    if !tui.state.text_buf.is_empty() {
        let body = std::mem::take(&mut tui.state.text_buf);
        tui.emit_block(ContentBlock {
            kind: BlockKind::Assistant,
            body,
        })?;
    }
    if !tui.state.thinking_buf.is_empty() {
        let body = std::mem::take(&mut tui.state.thinking_buf);
        tui.emit_block(ContentBlock {
            kind: BlockKind::Thinking,
            body,
        })?;
    }

    tui.state.phase = Phase::Input;
    tui.draw()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

async fn handle_login(
    input: &str,
    model: &Model,
    provider: &mut Box<dyn LlmProvider>,
    tui: &mut Tui,
) {
    let login_name = input.strip_prefix("/login").unwrap().trim();

    let login_provider = if login_name.is_empty() {
        ri_ai::registry::all_providers().into_iter().next()
    } else {
        ri_ai::registry::all_providers()
            .into_iter()
            .find(|p| p.id() == login_name)
    };

    let Some(login_provider) = login_provider else {
        let _ = tui.emit_and_draw(vec![Line::styled(
            format!("Unknown provider: {}", login_name),
            Style::default().fg(Color::Red),
        )]);
        return;
    };

    match login_provider.begin_login().await {
        Ok(Some(AuthMethod::PasteCode { url })) => {
            let msg = format!(
                "Visit this URL to authorize:\n{}\n\nPaste-code login not yet supported in TUI. Use --mode print.",
                url
            );
            let md = tui_markdown::from_str(&msg);
            let _ = tui.emit_and_draw(md.lines.into_iter().map(own_line).collect());
        }
        Ok(Some(AuthMethod::LocalCallback { url, port, path })) => {
            let msg = format!("Starting OAuth login...\nVisit: {}", url);
            let md = tui_markdown::from_str(&msg);
            let _ = tui.emit_and_draw(md.lines.into_iter().map(own_line).collect());

            #[cfg(target_os = "macos")]
            {
                let _ = std::process::Command::new("open").arg(&url).spawn();
            }

            match run_local_callback_login(login_provider, &url, port, &path).await {
                Ok(()) => match ri_ai::registry::resolve(&model.id).await {
                    Ok((p, _)) => {
                        *provider = p;
                        let _ = tui.emit_and_draw(vec![Line::styled(
                            "Logged in successfully.",
                            Style::default().fg(Color::Green),
                        )]);
                    }
                    Err(e) => {
                        let _ = tui.emit_and_draw(vec![Line::styled(
                            format!("resolve error: {}", e),
                            Style::default().fg(Color::Red),
                        )]);
                    }
                },
                Err(e) => {
                    let _ = tui.emit_and_draw(vec![Line::styled(
                        format!("login failed: {}", e),
                        Style::default().fg(Color::Red),
                    )]);
                }
            }
        }
        Ok(Some(AuthMethod::TextInput { prompt, .. })) => {
            let msg = format!("{}\n\nText-input login not yet supported in TUI. Use the web UI or set the GEMINI_API_KEY env var.", prompt);
            let md = tui_markdown::from_str(&msg);
            let _ = tui.emit_and_draw(md.lines.into_iter().map(own_line).collect());
        }
        Ok(None) => {
            let _ = tui.emit_and_draw(vec![Line::raw("No login needed for this provider.")]);
        }
        Err(e) => {
            let _ = tui.emit_and_draw(vec![Line::styled(
                format!("login error: {}", e),
                Style::default().fg(Color::Red),
            )]);
        }
    }
}

async fn run_local_callback_login(
    provider: Box<dyn LlmProvider>,
    _auth_url: &str,
    port: u16,
    expected_path: &str,
) -> eyre::Result<()> {
    use axum::{Router, extract::Query, response::Html, routing::get};
    use std::collections::HashMap;

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
    let tx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(tx)));

    let handler = {
        let tx = tx.clone();
        move |Query(params): Query<HashMap<String, String>>| {
            let tx = tx.clone();
            async move {
                let mut guard = tx.lock().await;
                if let Some(tx) = guard.take() {
                    if let Some(error) = params.get("error") {
                        let _ = tx.send(Err(error.clone()));
                        return Html("<h1>Authorization failed</h1>".to_string());
                    }
                    if let Some(code) = params.get("code") {
                        let _ = tx.send(Ok(code.clone()));
                        return Html(
                            "<h1>Success</h1><p>You can close this window.</p>".to_string(),
                        );
                    }
                    let _ = tx.send(Err("No authorization code in callback".into()));
                }
                Html("<h1>Unexpected request</h1>".to_string())
            }
        }
    };

    let app = Router::new().route(expected_path, get(handler));
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port))
        .await
        .map_err(|e| eyre::eyre!("Failed to bind OAuth callback on port {}: {}", port, e))?;

    let code = tokio::select! {
        result = axum::serve(listener, app) => {
            result.map_err(|e| eyre::eyre!("OAuth callback server error: {}", e))?;
            return Err(eyre::eyre!("OAuth callback server stopped unexpectedly"));
        }
        result = rx => {
            result
                .map_err(|_| eyre::eyre!("OAuth callback channel closed"))?
                .map_err(|e| eyre::eyre!("OAuth error: {}", e))?
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
            return Err(eyre::eyre!("OAuth callback timed out after 5 minutes"));
        }
    };

    provider.complete_login(&code).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Synchronized output (DEC 2026)
// ---------------------------------------------------------------------------

fn sync_start() -> io::Result<()> {
    let mut out = io::stdout().lock();
    write!(out, "\x1b[?2026h")?;
    out.flush()
}

fn sync_end() -> io::Result<()> {
    let mut out = io::stdout().lock();
    write!(out, "\x1b[?2026l")?;
    out.flush()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn own_line(line: Line<'_>) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| Span::styled(s.content.into_owned(), s.style))
        .collect();
    Line::from(spans).style(line.style)
}

fn spinner_frame(tick: usize) -> &'static str {
    const FRAMES: &[&str] = &["*", "o", "O", "o"];
    FRAMES[tick % FRAMES.len()]
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i <= max)
            .last()
            .unwrap_or(0);
        format!("{}...", &s[..end])
    }
}

fn wrapped_height(lines: &[Line<'_>], width: u16) -> u16 {
    let text = Text::from(lines.to_vec());
    let para = Paragraph::new(text).wrap(Wrap { trim: false });
    para.line_count(width).min(u16::MAX as usize) as u16
}

fn help_text(templates: &[ri_tools::prompts::PromptTemplate]) -> String {
    let mut text = String::from("**Commands:**\n");
    for p in ri_ai::registry::all_providers() {
        text.push_str(&format!("- `/login {}` - {}\n", p.id(), p.name()));
    }
    text.push_str("- `/quit`, `/exit` - Exit ri\n");
    text.push_str("- `Ctrl+C` - Cancel running agent\n");

    if !templates.is_empty() {
        text.push_str("\n**Prompt Templates:**\n");
        for t in templates {
            if t.description.is_empty() {
                text.push_str(&format!("- `/{}`\n", t.name));
            } else {
                text.push_str(&format!("- `/{}` - {}\n", t.name, t.description));
            }
        }
    }

    text
}

fn session_name_from_prompt(prompt: Option<&str>) -> String {
    match prompt {
        Some(p) => {
            let words: String = p.split_whitespace().take(5).collect::<Vec<_>>().join("-");
            if words.is_empty() {
                "session".to_string()
            } else {
                words
            }
        }
        None => "interactive".to_string(),
    }
}

/// Load prompt templates from global config and project-local directories.
fn load_prompt_templates(cwd: &std::path::Path) -> Vec<ri_tools::prompts::PromptTemplate> {
    use std::path::Path;
    let mut templates = Vec::new();
    if let Some(global) = ri_tools::resources::config_dir() {
        templates.extend(ri_tools::prompts::load_templates(&global.join("prompts")));
    }
    let mut dir = cwd.canonicalize().ok().or_else(|| Some(cwd.to_path_buf()));
    while let Some(d) = dir {
        templates.extend(ri_tools::prompts::load_templates(
            &d.join(".agents").join("prompts"),
        ));
        if d.join(".git").exists() {
            break;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    templates
}
