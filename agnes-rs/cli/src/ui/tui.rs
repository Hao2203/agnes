//! ratatui 终端后端：消息区、审批区、输入框与状态栏的绘制，以及键盘输入。
//!
//! 由 [`Renderer`](crate::render::Renderer) 在独立线程上启动；本模块不持有
//! ECS 句柄，只接收 [`UiCommand`] 并通过 `UserInput` / `RuntimeSender`
//! 把输入送回管线。
//!
//! TUI 线程同时负责键盘输入：普通行经 [`classify`] 路由成任务/命令消息，
//! 审批期间则把按键原样交给 [`UserInput`] 的答覆队列。

use std::io::Stdout;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use agnes_ecs::{CancelToken, RuntimeSender};
use crossterm::cursor;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use unicode_width::UnicodeWidthStr;

use super::{Status, UiCommand};
use crate::input::{Line as InputLine, TaskLine, UserInput, classify};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const POLL_INTERVAL: Duration = Duration::from_millis(50);

struct TuiHost {
    input: Arc<UserInput>,
    sender: RuntimeSender,
    shutdown: CancelToken,
    running: tokio::sync::watch::Receiver<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Exit,
}

pub(crate) fn run(
    rx: std_mpsc::Receiver<UiCommand>,
    input: Arc<UserInput>,
    sender: RuntimeSender,
    shutdown: CancelToken,
) {
    let mut terminal = match enter_terminal() {
        Ok(terminal) => terminal,
        Err(error) => {
            eprintln!("could not start the TUI: {error}");
            return;
        }
    };
    let running = input.running_handle();
    let mut host = TuiHost {
        input,
        sender,
        shutdown: shutdown.clone(),
        running,
    };
    let mut state = TuiState {
        follow: true,
        ..TuiState::default()
    };
    let mut pending_reset = false;
    let mut dirty = true;

    'outer: loop {
        if shutdown.is_cancelled() {
            break;
        }
        loop {
            match rx.try_recv() {
                Ok(UiCommand::Shutdown) | Err(std_mpsc::TryRecvError::Disconnected) => break 'outer,
                Ok(command) => {
                    state.apply(command);
                    dirty = true;
                }
                Err(std_mpsc::TryRecvError::Empty) => break,
            }
        }

        if host.running.has_changed().unwrap_or(false) {
            let running = *host.running.borrow_and_update();
            if pending_reset && !running {
                pending_reset = false;
                let _ = host
                    .sender
                    .send_message(agnes_plugin_session::RotateSession);
            }
        }

        match event::poll(POLL_INTERVAL) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if state.handle_key(key, &mut host, &mut pending_reset) == Flow::Exit {
                        break 'outer;
                    }
                    dirty = true;
                }
                Ok(Event::Paste(text)) => {
                    state.insert_str(&text);
                    dirty = true;
                }
                Ok(Event::Mouse(mouse)) => {
                    let changed = match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            state.scroll_up(3);
                            true
                        }
                        MouseEventKind::ScrollDown => {
                            state.scroll_down(3);
                            true
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            state.begin_selection(mouse.column, mouse.row);
                            true
                        }
                        MouseEventKind::Drag(MouseButton::Left) => {
                            state.update_selection(mouse.column, mouse.row);
                            true
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            if let Some(text) = state.finish_selection() {
                                copy_to_clipboard(&text);
                            }
                            true
                        }
                        _ => false,
                    };
                    if changed {
                        dirty = true;
                    }
                }
                Ok(Event::Resize(_, _)) => dirty = true,
                Ok(_) => {}
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }

        if state.status.running {
            state.spinner = state.spinner.wrapping_add(1);
            dirty = true;
        }
        if dirty {
            if state.draw(&mut terminal).is_err() {
                break;
            }
            dirty = false;
        }
    }

    restore_terminal(&mut terminal);
}

fn enter_terminal() -> std::io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    if let Err(error) = execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        cursor::Hide
    ) {
        let _ = disable_raw_mode();
        return Err(error);
    }
    match Terminal::new(CrosstermBackend::new(stdout)) {
        Ok(terminal) => Ok(terminal),
        Err(error) => {
            let _ = execute!(
                std::io::stdout(),
                LeaveAlternateScreen,
                DisableBracketedPaste,
                DisableMouseCapture,
                cursor::Show
            );
            let _ = disable_raw_mode();
            Err(error)
        }
    }
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) {
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture,
        cursor::Show
    );
    let _ = terminal.show_cursor();
}

fn copy_to_clipboard(text: &str) {
    use std::io::Write;

    let encoded = encode_base64(text.as_bytes());
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b]52;c;{encoded}\x07");
    let _ = stdout.flush();
}

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[usize::from(b0 >> 2)] as char);
        out.push(TABLE[usize::from(((b0 & 0b11) << 4) | (b1 >> 4))] as char);
        out.push(if chunk.len() > 1 {
            TABLE[usize::from(((b1 & 0b1111) << 2) | (b2 >> 6))] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[usize::from(b2 & 0b111111)] as char
        } else {
            '='
        });
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CellPos {
    row: u16,
    column: u16,
}

#[derive(Clone, Copy)]
struct Selection {
    anchor: CellPos,
    head: CellPos,
}

struct TuiState {
    transcript: Vec<Line<'static>>,
    streaming: String,
    input: String,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    status: Status,
    approval: Option<String>,
    follow: bool,
    scroll: u16,
    max_scroll: u16,
    viewport: u16,
    hidden_below: u16,
    transcript_area: Rect,
    visible_rows: Vec<Vec<String>>,
    selection: Option<Selection>,
    spinner: usize,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            transcript: Vec::new(),
            streaming: String::new(),
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_index: None,
            status: Status::default(),
            approval: None,
            follow: true,
            scroll: 0,
            max_scroll: 0,
            viewport: 0,
            hidden_below: 0,
            transcript_area: Rect::new(0, 0, 0, 0),
            visible_rows: Vec::new(),
            selection: None,
            spinner: 0,
        }
    }
}

impl TuiState {
    fn apply(&mut self, command: UiCommand) {
        match command {
            UiCommand::Line(text) => self.push_text(&text, None),
            UiCommand::Prompt(_) => {
                self.status.running = false;
                self.status.streaming = false;
                self.streaming.clear();
                self.selection = None;
            }
            UiCommand::StreamStart => {
                self.streaming.clear();
                self.status.streaming = true;
                self.selection = None;
            }
            UiCommand::StreamDelta(text) => {
                if !self.status.streaming {
                    self.streaming.clear();
                    self.status.streaming = true;
                }
                self.streaming.push_str(&text);
                self.selection = None;
            }
            UiCommand::StreamEnd => {
                self.status.streaming = false;
                self.streaming.clear();
                self.selection = None;
            }
            UiCommand::CommitAssistant(text) => {
                self.status.streaming = false;
                self.streaming.clear();
                self.push_text(&text, None);
            }
            UiCommand::StepStats { step, usage } => {
                self.status.step = step;
                self.status.usage = usage;
                self.status.streaming = false;
                self.streaming.clear();
                self.selection = None;
            }
            UiCommand::Status(status) => self.status = status,
            UiCommand::Approval(prompt) => self.approval = prompt,
            UiCommand::Flush | UiCommand::Shutdown => {}
        }
    }

    fn page(&self) -> u16 {
        self.viewport.saturating_sub(1).max(1)
    }

    fn to_transcript(&self, column: u16, row: u16) -> Option<CellPos> {
        let area = self.transcript_area;
        if area.width == 0
            || area.height == 0
            || column < area.x
            || column >= area.x + area.width
            || row < area.y
            || row >= area.y + area.height
        {
            return None;
        }
        Some(CellPos {
            row: row - area.y,
            column: column - area.x,
        })
    }

    fn to_transcript_clamped(&self, column: u16, row: u16) -> Option<CellPos> {
        let area = self.transcript_area;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        Some(CellPos {
            row: row.clamp(area.y, area.y + area.height - 1) - area.y,
            column: column.clamp(area.x, area.x + area.width - 1) - area.x,
        })
    }

    fn begin_selection(&mut self, column: u16, row: u16) {
        if let Some(position) = self.to_transcript(column, row) {
            self.selection = Some(Selection {
                anchor: position,
                head: position,
            });
        }
    }

    fn update_selection(&mut self, column: u16, row: u16) {
        if let Some(position) = self.to_transcript_clamped(column, row)
            && let Some(selection) = &mut self.selection
        {
            selection.head = position;
            if selection.head != selection.anchor {
                self.follow = false;
            }
        }
    }

    fn finish_selection(&mut self) -> Option<String> {
        let text = self.selected_text();
        if text.is_none() {
            self.selection = None;
        }
        text
    }

    fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let (start, end) = if selection.anchor <= selection.head {
            (selection.anchor, selection.head)
        } else {
            (selection.head, selection.anchor)
        };
        if start == end {
            return None;
        }
        let mut lines = Vec::new();
        for row in start.row..=end.row {
            let cells = self.visible_rows.get(usize::from(row))?;
            if cells.is_empty() {
                continue;
            }
            let from = if row == start.row {
                usize::from(start.column)
            } else {
                0
            };
            let to = if row == end.row {
                usize::from(end.column)
            } else {
                cells.len() - 1
            };
            let from = from.min(cells.len() - 1);
            let to = to.min(cells.len() - 1);
            if from > to {
                continue;
            }
            lines.push(cells[from..=to].concat().trim_end().to_owned());
        }
        let text = lines.join("\n");
        (!text.trim().is_empty()).then_some(text)
    }

    fn snapshot_rows(&mut self, frame: &mut Frame) {
        let area = self.transcript_area;
        let buffer = frame.buffer_mut();
        let mut rows = Vec::with_capacity(usize::from(area.height));
        for row in 0..area.height {
            let mut cells = Vec::with_capacity(usize::from(area.width));
            for column in 0..area.width {
                if let Some(cell) = buffer.cell((area.x + column, area.y + row)) {
                    cells.push(cell.symbol().to_owned());
                }
            }
            rows.push(cells);
        }
        self.visible_rows = rows;
    }

    fn highlight_selection(&self, frame: &mut Frame) {
        let Some(selection) = self.selection else {
            return;
        };
        let area = self.transcript_area;
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (start, end) = if selection.anchor <= selection.head {
            (selection.anchor, selection.head)
        } else {
            (selection.head, selection.anchor)
        };
        let style = Style::new().bg(Color::Cyan).fg(Color::Black);
        let buffer = frame.buffer_mut();
        for row in start.row..=end.row {
            let (from, to) = if row == start.row && row == end.row {
                (start.column, end.column)
            } else if row == start.row {
                (start.column, area.width.saturating_sub(1))
            } else if row == end.row {
                (0, end.column)
            } else {
                (0, area.width.saturating_sub(1))
            };
            for column in from..=to {
                if let Some(cell) = buffer.cell_mut((area.x + column, area.y + row)) {
                    cell.set_style(style);
                }
            }
        }
    }

    fn scroll_up(&mut self, lines: u16) {
        self.follow = false;
        self.selection = None;
        self.scroll = self.scroll.saturating_sub(lines);
    }

    fn scroll_down(&mut self, lines: u16) {
        self.selection = None;
        self.scroll = self.scroll.saturating_add(lines).min(self.max_scroll);
        if self.scroll >= self.max_scroll {
            self.follow = true;
        }
    }

    fn push_text(&mut self, text: &str, style: Option<Style>) {
        self.selection = None;
        for line in text.split('\n') {
            let style = style.unwrap_or_else(|| line_style(line));
            self.transcript
                .push(Line::from(Span::styled(line.to_owned(), style)));
        }
    }

    fn insert_char(&mut self, ch: char) {
        self.input.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    fn insert_str(&mut self, text: &str) {
        let text = text.replace(['\n', '\r'], " ");
        self.input.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    fn backspace(&mut self) {
        let start = prev_boundary(&self.input, self.cursor);
        self.input.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn delete(&mut self) {
        let end = next_boundary(&self.input, self.cursor);
        self.input.replace_range(self.cursor..end, "");
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_index {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.history_index = Some(index);
        self.input = self.history[index].clone();
        self.cursor = self.input.len();
    }

    fn history_next(&mut self) {
        match self.history_index {
            Some(index) if index + 1 < self.history.len() => {
                self.history_index = Some(index + 1);
                self.input = self.history[index + 1].clone();
            }
            _ => {
                self.history_index = None;
                self.input.clear();
            }
        }
        self.cursor = self.input.len();
    }

    fn submit(&mut self, host: &mut TuiHost, pending_reset: &mut bool) {
        let line = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.history_index = None;
        if host.input.is_answering() {
            host.input.push_answer(line);
            return;
        }
        if line.trim().is_empty() {
            return;
        }
        self.history.push(line.clone());
        self.follow = true;
        self.selection = None;
        self.push_text(&format!("› {}", line.trim()), Some(user_style()));
        match classify(&line) {
            None => {}
            Some(InputLine::Exit) => host.shutdown.cancel(),
            Some(InputLine::Reset) => {
                if *host.running.borrow_and_update() {
                    *pending_reset = true;
                } else {
                    let _ = host
                        .sender
                        .send_message(agnes_plugin_session::RotateSession);
                }
            }
            Some(InputLine::Task(content)) => {
                let _ = host.sender.send_message(TaskLine { content });
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent, host: &mut TuiHost, pending_reset: &mut bool) -> Flow {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if host.input.is_answering() {
                    host.input.push_answer(String::new());
                    return Flow::Continue;
                }
                if *host.running.borrow_and_update() || self.status.running {
                    let _ = host
                        .sender
                        .send_message(agnes_agent::runtime::InterruptTurn);
                } else {
                    host.shutdown.cancel();
                    return Flow::Exit;
                }
            }
            KeyCode::Char('d') if ctrl => {
                if host.input.is_answering() {
                    host.input.push_answer(String::new());
                }
                host.shutdown.cancel();
                return Flow::Exit;
            }
            KeyCode::Enter => self.submit(host, pending_reset),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.cursor = prev_boundary(&self.input, self.cursor),
            KeyCode::Right => self.cursor = next_boundary(&self.input, self.cursor),
            KeyCode::Up => self.history_prev(),
            KeyCode::Down => self.history_next(),
            KeyCode::PageUp => self.scroll_up(self.page()),
            KeyCode::PageDown => self.scroll_down(self.page()),
            KeyCode::Home if ctrl => {
                self.follow = false;
                self.scroll = 0;
                self.selection = None;
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => {
                self.follow = true;
                self.selection = None;
            }
            KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                if host.input.is_answering() && matches!(ch, 'y' | 'Y' | 'n' | 'N' | 'a' | 'A') {
                    host.input.push_answer(ch.to_string());
                    self.input.clear();
                    self.cursor = 0;
                } else {
                    self.insert_char(ch);
                }
            }
            _ => {}
        }
        Flow::Continue
    }

    fn content_lines(&self) -> Vec<Line<'static>> {
        let mut lines = self.transcript.clone();
        if self.status.streaming || !self.streaming.is_empty() {
            let mut parts: Vec<&str> = self.streaming.split('\n').collect();
            let last = parts.pop().unwrap_or_default();
            for part in parts {
                lines.push(Line::from(part.to_owned()));
            }
            lines.push(Line::from(vec![
                Span::raw(last.to_owned()),
                Span::styled("▍", Style::new().fg(Color::Cyan)),
            ]));
        }
        lines
    }

    fn draw(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> std::io::Result<()> {
        terminal.draw(|frame| self.render(frame))?;
        Ok(())
    }

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let approval_height = u16::from(self.approval.is_some()) * 4;
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(approval_height),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

        let block = Block::bordered().title(" agnes ");
        let inner = block.inner(chunks[0]);
        self.transcript_area = inner;
        let paragraph = Paragraph::new(self.content_lines())
            .block(block)
            .wrap(Wrap { trim: false });
        let total = u16::try_from(paragraph.line_count(inner.width)).unwrap_or(u16::MAX);
        let max_scroll = total.saturating_sub(inner.height);
        let scroll = if self.follow {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };
        self.scroll = scroll;
        self.max_scroll = max_scroll;
        self.viewport = inner.height;
        self.hidden_below = max_scroll.saturating_sub(scroll);
        frame.render_widget(paragraph.scroll((scroll, 0)), chunks[0]);
        self.snapshot_rows(frame);
        self.highlight_selection(frame);

        if let Some(prompt) = &self.approval {
            let widget = Paragraph::new(prompt.trim().to_owned())
                .block(
                    Block::bordered()
                        .title(" approval ")
                        .border_style(Style::new().fg(Color::Yellow)),
                )
                .style(Style::new().fg(Color::Yellow))
                .wrap(Wrap { trim: true });
            frame.render_widget(widget, chunks[1]);
        }

        let running = self.status.running;
        let input_block = Block::bordered()
            .title(" input ")
            .border_style(Style::new().fg(if running {
                Color::Cyan
            } else {
                Color::DarkGray
            }));
        frame.render_widget(
            Paragraph::new(self.input.clone()).block(input_block),
            chunks[2],
        );
        let cursor_width = UnicodeWidthStr::width(&self.input[..self.cursor]);
        frame.set_cursor_position((
            chunks[2].x + 1 + u16::try_from(cursor_width).unwrap_or(u16::MAX),
            chunks[2].y + 1,
        ));

        frame.render_widget(
            Paragraph::new(status_line(&self.status, self.spinner, self.hidden_below))
                .style(Style::new().bg(Color::Black)),
            chunks[3],
        );
    }
}

fn line_style(line: &str) -> Style {
    let trimmed = line.trim_start();
    if trimmed.starts_with('⚒') {
        Style::new().fg(Color::Yellow)
    } else if trimmed.starts_with('✗') || trimmed.starts_with('⛔') || line.starts_with("error:")
    {
        Style::new().fg(Color::Red)
    } else if trimmed.starts_with('↪')
        || trimmed.starts_with('→')
        || trimmed.starts_with('←')
        || trimmed.starts_with('─')
        || trimmed.starts_with('*')
    {
        Style::new().fg(Color::DarkGray)
    } else {
        Style::default()
    }
}

fn user_style() -> Style {
    Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD)
}

fn status_line(status: &Status, spinner: usize, hidden_below: u16) -> Line<'static> {
    let mark = if status.running {
        SPINNER[spinner % SPINNER.len()]
    } else {
        "●"
    };
    let step = if status.step == 0 {
        "-".to_owned()
    } else {
        status.step.to_string()
    };
    let mut spans = vec![
        Span::styled(
            format!(" {mark} "),
            Style::new().fg(if status.running {
                Color::Cyan
            } else {
                Color::Green
            }),
        ),
        Span::styled(
            status.model.clone(),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  step {step}"), Style::new().fg(Color::DarkGray)),
    ];
    if status.streaming && status.usage.input == 0 && status.usage.output == 0 {
        spans.push(Span::styled(
            format!("  out ~{} tok", estimate_tokens(status.live_chars)),
            Style::new().fg(Color::Cyan),
        ));
    } else {
        spans.push(Span::raw(format!(
            "  in {} / out {}",
            human(status.usage.input),
            human(status.usage.output)
        )));
    }
    if status.session_usage.input + status.session_usage.output > 0 {
        spans.push(Span::styled(
            format!(
                "  Σ {}/{}",
                human(status.session_usage.input),
                human(status.session_usage.output)
            ),
            Style::new().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::styled(
        format!("  {:.1}s", status.elapsed.as_secs_f64()),
        Style::new().fg(Color::DarkGray),
    ));
    if hidden_below > 0 {
        spans.push(Span::styled(
            format!("  ↓ {hidden_below} lines · End"),
            Style::new().fg(Color::Yellow),
        ));
    } else {
        spans.push(Span::styled(
            "  C-c stop · C-d quit",
            Style::new().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

fn estimate_tokens(chars: u64) -> u64 {
    chars.div_ceil(4)
}

fn human(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn prev_boundary(text: &str, mut index: usize) -> usize {
    if index == 0 {
        return 0;
    }
    index -= 1;
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn next_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    index += 1;
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn state() -> TuiState {
        TuiState::default()
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for row in buffer.content().chunks(usize::from(buffer.area.width)) {
            for cell in row {
                out.push_str(cell.symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn rendering_follows_the_tail_and_scrolls_back() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut state = state();
        for index in 0..100 {
            state
                .transcript
                .push(Line::from(format!("line {index:03}")));
        }

        terminal.draw(|frame| state.render(frame)).unwrap();
        let tail = screen(&terminal);
        assert!(tail.contains("line 099"));
        assert!(!tail.contains("line 000"));
        assert!(state.max_scroll > 0);

        state.follow = false;
        state.scroll = 0;
        terminal.draw(|frame| state.render(frame)).unwrap();
        let top = screen(&terminal);
        assert!(top.contains("line 000"));
        assert!(!top.contains("line 099"));
        assert!(state.hidden_below > 0);
    }

    #[test]
    fn editing_respects_utf8_boundaries() {
        let mut state = state();
        state.insert_str("a中b");
        assert_eq!(state.cursor, 5);
        state.backspace();
        assert_eq!(state.input, "a中");
        state.cursor = state.input.len();
        state.backspace();
        assert_eq!(state.input, "a");
        state.cursor = 0;
        state.delete();
        assert_eq!(state.input, "");
    }

    #[test]
    fn history_moves_through_submitted_lines() {
        let mut state = state();
        state.history = vec!["first".to_owned(), "second".to_owned()];
        state.history_prev();
        assert_eq!(state.input, "second");
        state.history_prev();
        assert_eq!(state.input, "first");
        state.history_next();
        assert_eq!(state.input, "second");
        state.history_next();
        assert_eq!(state.input, "");
    }

    #[test]
    fn scrolling_detaches_from_the_tail_and_reaching_it_follows_again() {
        let mut state = state();
        state.max_scroll = 100;
        state.viewport = 20;
        state.scroll = 100;

        assert_eq!(state.page(), 19);
        state.scroll_up(state.page());
        assert!(!state.follow);
        assert_eq!(state.scroll, 81);

        state.scroll_down(state.page());
        assert!(state.follow);
        assert_eq!(state.scroll, 100);

        state.scroll_up(10);
        assert!(!state.follow);
        state.scroll_down(10);
        assert!(state.follow);
    }

    #[test]
    fn clipboard_payloads_are_base64_encoded() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"hello"), "aGVsbG8=");
        assert_eq!(encode_base64("中".as_bytes()), "5Lit");
    }

    #[test]
    fn mouse_selection_extracts_visible_rows() {
        let mut state = state();
        state.transcript_area = Rect::new(0, 0, 11, 2);
        state.visible_rows = vec![
            "hello world".chars().map(|ch| ch.to_string()).collect(),
            "second line".chars().map(|ch| ch.to_string()).collect(),
        ];

        state.begin_selection(6, 0);
        state.update_selection(6, 1);
        assert!(!state.follow);
        assert_eq!(state.selected_text().as_deref(), Some("world\nsecond"));

        state.begin_selection(6, 1);
        state.update_selection(6, 0);
        assert_eq!(state.selected_text().as_deref(), Some("world\nsecond"));

        state.begin_selection(0, 0);
        state.update_selection(100, 100);
        assert_eq!(
            state.selected_text().as_deref(),
            Some("hello world\nsecond line")
        );

        state.begin_selection(0, 0);
        assert_eq!(state.finish_selection(), None);
        assert!(state.selection.is_none());
    }

    #[test]
    fn selection_is_highlighted_in_the_rendered_transcript() {
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let mut state = state();
        state.transcript = vec![Line::from("hello")];
        terminal.draw(|frame| state.render(frame)).unwrap();

        let area = state.transcript_area;
        state.selection = Some(Selection {
            anchor: CellPos { row: 0, column: 0 },
            head: CellPos { row: 0, column: 2 },
        });
        terminal.draw(|frame| state.render(frame)).unwrap();

        let cell = terminal.backend().buffer().cell((area.x, area.y)).unwrap();
        assert_eq!(cell.bg, Color::Cyan);
    }

    #[test]
    fn token_helpers_are_readable() {
        assert_eq!(human(0), "0");
        assert_eq!(human(999), "999");
        assert_eq!(human(1_234), "1,234");
        assert_eq!(human(1_234_567), "1,234,567");
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(5), 2);
    }

    #[test]
    fn a_streamed_message_replaces_the_live_buffer_on_commit() {
        let mut state = state();
        state.apply(UiCommand::StreamStart);
        state.apply(UiCommand::StreamDelta("hel".to_owned()));
        state.apply(UiCommand::StreamDelta("lo".to_owned()));
        assert_eq!(state.streaming, "hello");

        state.apply(UiCommand::CommitAssistant("hello".to_owned()));
        assert!(state.streaming.is_empty());
        assert_eq!(state.transcript.len(), 1);
    }
}
