//! Embedded terminal: the UI paints snapshots; workers own all PTY I/O.
use crate::{settings::Settings, state::Msg};
use crossbeam_channel::Sender;
use egui::{Color32, Event, Key, Rect, Sense};
use keel_term::{Session, Shell};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

const BACKGROUND: Color32 = Color32::from_rgb(16, 18, 22);

pub fn keyboard_event(event: &Event) -> bool {
    matches!(
        event,
        Event::Key { .. }
            | Event::Text(_)
            | Event::Paste(_)
            | Event::Copy
            | Event::Cut
            | Event::Ime(_)
    )
}

/// Alt+letter/digit is meta: ESC then the key (Windows/Linux; macOS Option composes text).
fn meta_char(key: Key, modifiers: egui::Modifiers) -> Option<char> {
    if cfg!(target_os = "macos") || !modifiers.alt || modifiers.ctrl || modifiers.command {
        return None;
    }
    let name = key.name();
    let c = name.chars().next().filter(|c| c.is_ascii_alphanumeric())?;
    (name.len() == 1).then(|| {
        if modifiers.shift {
            c.to_ascii_uppercase()
        } else {
            c.to_ascii_lowercase()
        }
    })
}

/// egui-winit also reports Alt+key as text; that echo is already sent as meta.
fn is_meta_echo(meta: Option<char>, event: &Event) -> bool {
    match (meta, event) {
        (Some(c), Event::Text(text)) => text.eq_ignore_ascii_case(c.encode_utf8(&mut [0; 4])),
        _ => false,
    }
}

fn input_bytes(event: &Event, application_cursor: bool, bracketed_paste: bool) -> Option<Vec<u8>> {
    let bytes = match event {
        Event::Text(text) => text.as_bytes().to_vec(),
        Event::Paste(text) => {
            if bracketed_paste {
                // An ESC in the text could end paste mode early (`ESC[201~`) and run the
                // rest as typed keys.
                format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).into_bytes()
            } else {
                text.replace('\n', "\r").into_bytes()
            }
        }
        Event::Copy => vec![3],
        Event::Cut => vec![24],
        Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => {
            // The physical Ctrl key (not macOS Cmd) makes control bytes. Ctrl+Alt is
            // AltGr on Windows: a layout character, never a control byte.
            if modifiers.ctrl && !modifiers.alt {
                let name = key.name();
                if name.len() == 1 && name.as_bytes()[0].is_ascii_alphabetic() {
                    return Some(vec![name.as_bytes()[0].to_ascii_uppercase() - b'A' + 1]);
                }
            }
            if let Some(c) = meta_char(*key, *modifiers) {
                return Some(vec![0x1b, c as u8]);
            }
            // xterm modifier parameter: 1 + Shift 1 + Alt 2 + Ctrl 4.
            let m = 1
                + u8::from(modifiers.shift)
                + 2 * u8::from(modifiers.alt)
                + 4 * u8::from(modifiers.ctrl);
            if m > 1 {
                let modified = match key {
                    Key::ArrowUp => Some(format!("\x1b[1;{m}A")),
                    Key::ArrowDown => Some(format!("\x1b[1;{m}B")),
                    Key::ArrowRight => Some(format!("\x1b[1;{m}C")),
                    Key::ArrowLeft => Some(format!("\x1b[1;{m}D")),
                    Key::Home => Some(format!("\x1b[1;{m}H")),
                    Key::End => Some(format!("\x1b[1;{m}F")),
                    Key::Insert => Some(format!("\x1b[2;{m}~")),
                    Key::Delete => Some(format!("\x1b[3;{m}~")),
                    Key::PageUp => Some(format!("\x1b[5;{m}~")),
                    Key::PageDown => Some(format!("\x1b[6;{m}~")),
                    _ => None,
                };
                if let Some(sequence) = modified {
                    return Some(sequence.into_bytes());
                }
            }
            let sequence = match key {
                Key::ArrowUp => {
                    if application_cursor {
                        "\x1bOA"
                    } else {
                        "\x1b[A"
                    }
                }
                Key::ArrowDown => {
                    if application_cursor {
                        "\x1bOB"
                    } else {
                        "\x1b[B"
                    }
                }
                Key::ArrowRight => {
                    if application_cursor {
                        "\x1bOC"
                    } else {
                        "\x1b[C"
                    }
                }
                Key::ArrowLeft => {
                    if application_cursor {
                        "\x1bOD"
                    } else {
                        "\x1b[D"
                    }
                }
                Key::Home => {
                    if application_cursor {
                        "\x1bOH"
                    } else {
                        "\x1b[H"
                    }
                }
                Key::End => {
                    if application_cursor {
                        "\x1bOF"
                    } else {
                        "\x1b[F"
                    }
                }
                Key::PageUp => "\x1b[5~",
                Key::PageDown => "\x1b[6~",
                Key::Insert => "\x1b[2~",
                Key::Delete => "\x1b[3~",
                Key::Backspace => "\x7f",
                Key::Tab if modifiers.shift => "\x1b[Z",
                Key::Tab => "\t",
                Key::Enter => "\r",
                Key::Escape => "\x1b",
                Key::F1 => "\x1bOP",
                Key::F2 => "\x1bOQ",
                Key::F3 => "\x1bOR",
                Key::F4 => "\x1bOS",
                Key::F5 => "\x1b[15~",
                Key::F7 => "\x1b[18~",
                Key::F8 => "\x1b[19~",
                Key::F9 => "\x1b[20~",
                Key::F10 => "\x1b[21~",
                Key::F11 => "\x1b[23~",
                Key::F12 => "\x1b[24~",
                _ => return None,
            };
            sequence.as_bytes().to_vec()
        }
        _ => return None,
    };
    Some(bytes)
}

enum Command {
    Write(Vec<u8>),
    Resize(u16, u16),
    Cd(PathBuf),
}

#[derive(Default)]
pub struct TermPane {
    pub open: bool,
    session: Option<Arc<Session>>,
    commands: Option<Sender<Command>>,
    shells: Vec<Shell>,
    generation: u64,
    starting: bool,
    cwd: Option<PathBuf>,
    dimensions: (u16, u16),
    seen_height: Option<f32>,
    wake_pending: Arc<AtomicBool>,
    selection: Option<(usize, usize)>,
}

impl TermPane {
    fn id() -> egui::Id {
        egui::Id::new("terminal-grid")
    }
    pub fn focused(&self, ctx: &egui::Context) -> bool {
        self.open && ctx.memory(|m| m.has_focus(Self::id()))
    }
    pub fn focus(&self, ctx: &egui::Context) {
        ctx.memory_mut(|m| m.request_focus(Self::id()));
    }
    pub fn leave(&self, ctx: &egui::Context) {
        ctx.memory_mut(|m| m.surrender_focus(Self::id()));
    }
    /// Ctrl+` while focused: hide the panel, keep the shell running.
    pub fn hide(&mut self, ctx: &egui::Context) {
        self.leave(ctx);
        self.open = false;
        self.seen_height = None;
    }
    /// Shows a hidden terminal again (its shell kept running); false if there is none.
    pub fn reopen(&mut self, ctx: &egui::Context) -> bool {
        if !self.open && !self.starting && self.session.is_none() {
            return false;
        }
        self.open = true;
        self.focus(ctx);
        true
    }
    #[cfg(test)]
    pub fn running(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.is_alive())
    }
    /// The × button: ends the shell.
    pub fn close(&mut self, ctx: &egui::Context) {
        self.leave(ctx);
        self.open = false;
        self.starting = false;
        self.generation += 1;
        self.commands = None;
        if let Some(session) = self.session.take() {
            session.terminate();
        }
        self.seen_height = None;
    }
    pub fn open_at(
        &mut self,
        cwd: PathBuf,
        settings: &Settings,
        tx: &Sender<Msg>,
        ctx: &egui::Context,
    ) {
        self.open = true;
        if !self.starting && self.session.as_ref().is_none_or(|s| !s.is_alive()) {
            self.restart(cwd, settings, tx, ctx);
        } else {
            self.cwd = Some(cwd.clone());
            self.send(Command::Cd(cwd), tx, ctx);
        }
        self.focus(ctx);
    }
    /// Types `line` and Enter into the shell ("Open terminal here" on a remote host),
    /// starting one at `cwd` first when none is running.
    pub fn type_line(
        &mut self,
        line: &str,
        cwd: PathBuf,
        settings: &Settings,
        tx: &Sender<Msg>,
        ctx: &egui::Context,
    ) {
        let dead = !self.starting && self.session.as_ref().is_none_or(|s| !s.is_alive());
        if !self.open || dead {
            self.open = true;
            self.restart(cwd, settings, tx, ctx);
        }
        self.focus(ctx);
        self.send(Command::Write(format!("{line}\r").into_bytes()), tx, ctx);
    }
    fn restart(
        &mut self,
        cwd: PathBuf,
        settings: &Settings,
        tx: &Sender<Msg>,
        ctx: &egui::Context,
    ) {
        if let Some(session) = self.session.take() {
            session.terminate();
        }
        self.commands = None;
        self.generation += 1;
        let generation = self.generation;
        self.starting = true;
        self.cwd = Some(cwd.clone());
        self.dimensions = (80, 12);
        self.selection = None;
        self.wake_pending = Arc::new(AtomicBool::new(false));
        let pending = self.wake_pending.clone();
        let (commands, rx) = crossbeam_channel::bounded(128);
        self.commands = Some(commands);
        let (tx, ctx, preferred) = (tx.clone(), ctx.clone(), settings.terminal_shell.clone());
        let error_tx = tx.clone();
        let error_ctx = ctx.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("keel-terminal".into())
            .spawn(move || {
                let shells = keel_term::available_shells();
                let Some(shell) = shells
                    .iter()
                    .find(|s| s.label == preferred)
                    .or_else(|| shells.first())
                else {
                    return;
                };
                let (notify_tx, notify_ctx) = (tx.clone(), ctx.clone());
                let notify = Arc::new(move || {
                    if !pending.swap(true, Ordering::AcqRel) {
                        let _ = notify_tx.send(Msg::TerminalChanged(generation));
                        notify_ctx.request_repaint();
                    }
                });
                let session = match Session::spawn(shell, &cwd, 80, 12, notify) {
                    Ok(s) => Arc::new(s),
                    Err(e) => {
                        let _ = tx.send(Msg::TerminalError {
                            generation,
                            text: format!("Terminal: {e:#}"),
                        });
                        ctx.request_repaint();
                        return;
                    }
                };
                if tx
                    .send(Msg::TerminalReady {
                        generation,
                        session: session.clone(),
                        shells,
                    })
                    .is_err()
                {
                    return;
                }
                ctx.request_repaint();
                while let Ok(command) = rx.recv() {
                    let result = match command {
                        Command::Write(bytes) => session.write(&bytes),
                        Command::Resize(cols, rows) => session.resize(cols, rows),
                        Command::Cd(dir) => {
                            session.cd(&dir);
                            Ok(())
                        }
                    };
                    if let Err(e) = result {
                        let _ = tx.send(Msg::TerminalError {
                            generation,
                            text: format!("Terminal: {e:#}"),
                        });
                    }
                    ctx.request_repaint();
                }
                session.terminate();
            })
        {
            self.starting = false;
            let _ = error_tx.send(Msg::TerminalError {
                generation,
                text: e.to_string(),
            });
            error_ctx.request_repaint();
        }
    }
    pub fn ready(&mut self, generation: u64, session: Arc<Session>, shells: Vec<Shell>) {
        // A hidden pane keeps its session; `close` and restarts bump the generation.
        if generation == self.generation {
            self.session = Some(session);
            self.shells = shells;
            self.starting = false;
        } else {
            session.terminate();
        }
    }
    pub fn changed(&self, generation: u64) {
        if generation == self.generation {
            self.wake_pending.store(false, Ordering::Release);
        }
    }
    pub fn error(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.starting = false;
        true
    }
    fn send(&self, command: Command, tx: &Sender<Msg>, ctx: &egui::Context) {
        if self
            .commands
            .as_ref()
            .is_some_and(|sender| sender.try_send(command).is_err())
        {
            let _ = tx.send(Msg::TerminalError {
                generation: self.generation,
                text: "Terminal input queue is full or closed".into(),
            });
            ctx.request_repaint();
        }
    }
    pub fn follow(
        &mut self,
        cwd: Option<PathBuf>,
        settings: &Settings,
        tx: &Sender<Msg>,
        ctx: &egui::Context,
    ) {
        if !self.open || !settings.terminal_follow_cwd || self.cwd == cwd {
            return;
        }
        if let Some(dir) = cwd {
            self.cwd = Some(dir.clone());
            self.send(Command::Cd(dir), tx, ctx);
        }
    }
    pub fn input(
        &mut self,
        ctx: &egui::Context,
        enabled: bool,
        settings: &Settings,
        tx: &Sender<Msg>,
    ) {
        if !enabled || !self.focused(ctx) {
            return;
        }
        let events = ctx.input_mut(|i| {
            let events = i
                .events
                .iter()
                .filter(|e| keyboard_event(e))
                .cloned()
                .collect::<Vec<_>>();
            i.events.retain(|e| !keyboard_event(e));
            events
        });
        let modifiers = ctx.input(|i| i.modifiers);
        if !self.starting && self.session.as_ref().is_none_or(|s| !s.is_alive()) {
            if events.iter().any(|e| {
                matches!(
                    e,
                    Event::Key {
                        key: Key::Enter,
                        pressed: true,
                        ..
                    }
                )
            }) {
                if let Some(cwd) = self.cwd.clone() {
                    self.restart(cwd, settings, tx, ctx);
                }
            }
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let (application, bracketed) = {
            let grid = session.grid();
            (
                grid.screen().application_cursor(),
                grid.screen().bracketed_paste(),
            )
        };
        // egui-winit turns Ctrl/Cmd+C/X/V (any Shift) into Copy/Cut/Paste with no Key
        // event, and Shift+Delete into Cut on Windows. Ctrl+Shift+C/V (Cmd+C/V on macOS)
        // copy/paste; plain Ctrl+C/X/V reach the child as control bytes.
        let clipboard = modifiers.shift || modifiers.mac_cmd;
        let mut meta = None;
        for event in events {
            let echo = is_meta_echo(meta.take(), &event);
            if let Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = &event
            {
                meta = meta_char(*key, *modifiers);
            }
            if echo {
                continue;
            }
            let bytes = match &event {
                Event::Copy if clipboard => {
                    ctx.copy_text(self.copy_text());
                    continue;
                }
                Event::Cut if modifiers.shift && !modifiers.command => Some(b"\x1b[3~".to_vec()),
                Event::Paste(_) if modifiers.command && !clipboard => Some(vec![22]),
                _ => input_bytes(&event, application, bracketed),
            };
            if let Some(bytes) = bytes {
                session.grid().screen_mut().set_scrollback(0);
                self.send(Command::Write(bytes), tx, ctx);
            }
        }
    }
    fn copy_text(&self) -> String {
        let Some(session) = &self.session else {
            return String::new();
        };
        let grid = session.grid();
        let screen = grid.screen();
        let Some((a, b)) = self.selection else {
            return screen.contents();
        };
        let cols = screen.size().1 as usize;
        let mut text = String::new();
        for index in a.min(b)..=a.max(b) {
            if index != a.min(b) && index % cols == 0 {
                text.push('\n');
            }
            if let Some(cell) = screen.cell((index / cols) as u16, (index % cols) as u16) {
                if !cell.is_wide_continuation() {
                    let s = cell.contents();
                    text.push_str(if s.is_empty() { " " } else { s });
                }
            }
        }
        text
    }
    pub fn panel(&mut self, ctx: &egui::Context, settings: &mut Settings, tx: &Sender<Msg>) {
        if !self.open {
            return;
        }
        let mut restart = false;
        let mut close = false;
        let panel = egui::TopBottomPanel::bottom("terminal")
            .resizable(true)
            .default_height(settings.terminal_height.clamp(100.0, 800.0))
            .min_height(100.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Terminal");
                    let selected = self
                        .shells
                        .iter()
                        .find(|s| s.label == settings.terminal_shell)
                        .or_else(|| self.shells.first())
                        .map(|s| s.label.as_str())
                        .unwrap_or("Loading shells…");
                    egui::ComboBox::from_id_salt("terminal-shell")
                        .selected_text(selected)
                        .show_ui(ui, |ui| {
                            for shell in &self.shells {
                                if ui
                                    .selectable_value(
                                        &mut settings.terminal_shell,
                                        shell.label.clone(),
                                        &shell.label,
                                    )
                                    .changed()
                                {
                                    restart = true;
                                }
                            }
                        });
                    restart |= ui.button("+").on_hover_text("Restart terminal").clicked();
                    ui.checkbox(&mut settings.terminal_follow_cwd, "Follow pane");
                    close |= ui
                        .button("×")
                        .on_hover_text("Close terminal (ends the shell)")
                        .clicked();
                    ui.weak("Ctrl+` hides, F6 / Shift+Esc returns to files");
                });
                if self.starting {
                    ui.label("Starting terminal…");
                } else if self.session.as_ref().is_none_or(|s| !s.is_alive()) {
                    ui.label("[process exited] press Enter to restart");
                }
                self.paint(ui, tx, ctx);
            });
        let height = panel.response.rect.height().round();
        if self.seen_height.is_some_and(|old| old != height) {
            settings.terminal_height = height;
        }
        self.seen_height = Some(height);
        if close {
            self.close(ctx);
        } else if restart {
            if let Some(cwd) = self.cwd.clone() {
                self.restart(cwd, settings, tx, ctx);
                self.focus(ctx);
            }
        }
    }
    fn paint(&mut self, ui: &mut egui::Ui, tx: &Sender<Msg>, ctx: &egui::Context) {
        let font = egui::FontId::monospace(14.0);
        let cell_size = ui.fonts(|f| egui::vec2(f.glyph_width(&font, 'M'), f.row_height(&font)));
        let (rect, _) = ui.allocate_exact_size(
            ui.available_size().max(egui::vec2(1.0, 1.0)),
            Sense::hover(),
        );
        let response = ui.interact(rect, Self::id(), Sense::click_and_drag());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Other, true, "Terminal grid")
        });
        if response.clicked() || response.drag_started() {
            response.request_focus();
        } else if response.clicked_elsewhere() && response.has_focus() {
            response.surrender_focus();
        }
        // Tab, arrows and Esc belong to the shell, not to egui focus traversal.
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                Self::id(),
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            )
        });
        let cols = ((rect.width() / cell_size.x) as u16).clamp(1, 400);
        let rows = ((rect.height() / cell_size.y) as u16).clamp(1, 200);
        if (cols, rows) != self.dimensions {
            self.dimensions = (cols, rows);
            self.send(Command::Resize(cols, rows), tx, ctx);
        }
        let Some(session) = &self.session else {
            return;
        };
        let mut parser = session.grid();
        if response.hovered() {
            let delta = ui.input(|i| i.smooth_scroll_delta.y);
            if delta.abs() >= 1.0 {
                let offset =
                    parser.screen().scrollback() as isize + (delta / cell_size.y).round() as isize;
                parser.screen_mut().set_scrollback(offset.max(0) as usize);
                self.selection = None;
            }
        }
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let position = |p: egui::Pos2| {
            let col = (((p.x - rect.left()) / cell_size.x).max(0.0) as u16).min(cols - 1);
            let row = (((p.y - rect.top()) / cell_size.y).max(0.0) as u16).min(rows - 1);
            row as usize * cols as usize + col as usize
        };
        if let Some(p) = response.interact_pointer_pos() {
            if response.clicked() {
                self.selection = None; // Ctrl+Shift+C then copies the whole screen
            } else if response.drag_started() {
                let index = position(p);
                self.selection = Some((index, index));
            } else if response.dragged() {
                if let Some((_, b)) = &mut self.selection {
                    *b = position(p);
                }
            }
        }
        let cursor = (!screen.hide_cursor() && screen.scrollback() == 0 && self.focused(ctx))
            .then(|| screen.cursor_position());
        let cells = (0..rows)
            .flat_map(|r| {
                (0..cols).filter_map(move |c| screen.cell(r, c).map(|cell| (r, c, cell.clone())))
            })
            .collect::<Vec<_>>();
        drop(parser); // Paint never holds the parser mutex.
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, BACKGROUND);
        for (r, c, cell) in &cells {
            let index = *r as usize * cols as usize + *c as usize;
            let selected = self
                .selection
                .is_some_and(|(a, b)| (a.min(b)..=a.max(b)).contains(&index));
            let (_, mut bg) = cell_colors(cell);
            if selected {
                bg = Color32::from_rgb(45, 70, 110);
            }
            if cursor == Some((*r, *c)) {
                bg = Color32::LIGHT_GRAY;
            }
            if bg == BACKGROUND {
                continue;
            }
            painter.rect_filled(
                Rect::from_min_size(
                    rect.min + egui::vec2(*c as f32 * cell_size.x, *r as f32 * cell_size.y),
                    cell_size,
                ),
                0.0,
                bg,
            );
        }
        for (r, c, cell) in &cells {
            if cell.is_wide_continuation() {
                continue;
            }
            let (mut fg, _) = cell_colors(cell);
            if cursor == Some((*r, *c)) {
                fg = Color32::BLACK;
            }
            let pos = rect.min + egui::vec2(*c as f32 * cell_size.x, *r as f32 * cell_size.y);
            let contents = cell.contents();
            if contents.trim().is_empty() {
                continue;
            }
            painter.text(pos, egui::Align2::LEFT_TOP, contents, font.clone(), fg);
            if cell.bold() {
                painter.text(
                    pos + egui::vec2(0.6, 0.0),
                    egui::Align2::LEFT_TOP,
                    contents,
                    font.clone(),
                    fg,
                );
            }
        }
    }
}
impl Drop for TermPane {
    fn drop(&mut self) {
        if let Some(session) = &self.session {
            session.terminate();
        }
    }
}

fn cell_colors(cell: &vt100::Cell) -> (Color32, Color32) {
    let fg = color(cell.fgcolor(), Color32::from_gray(220));
    let bg = color(cell.bgcolor(), BACKGROUND);
    if cell.inverse() {
        (bg, fg)
    } else {
        (fg, bg)
    }
}
fn color(color: vt100::Color, default: Color32) -> Color32 {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        vt100::Color::Idx(i) => {
            const ANSI: [[u8; 3]; 16] = [
                [0, 0, 0],
                [128, 0, 0],
                [0, 128, 0],
                [128, 128, 0],
                [0, 0, 128],
                [128, 0, 128],
                [0, 128, 128],
                [192, 192, 192],
                [128, 128, 128],
                [255, 0, 0],
                [0, 255, 0],
                [255, 255, 0],
                [0, 0, 255],
                [255, 0, 255],
                [0, 255, 255],
                [255, 255, 255],
            ];
            let [r, g, b] = if i < 16 {
                ANSI[i as usize]
            } else if i >= 232 {
                [8 + (i - 232) * 10; 3]
            } else {
                let i = i - 16;
                let level = |x| if x == 0 { 0 } else { 55 + x * 40 };
                [level(i / 36), level(i / 6 % 6), level(i % 6)]
            };
            Color32::from_rgb(r, g, b)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(key: Key, modifiers: egui::Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }
    #[test]
    fn terminal_keys_and_control_bytes() {
        for (key_code, bytes) in [
            (Key::ArrowUp, b"\x1b[A".as_slice()),
            (Key::Home, b"\x1b[H"),
            (Key::End, b"\x1b[F"),
            (Key::PageUp, b"\x1b[5~"),
            (Key::PageDown, b"\x1b[6~"),
            (Key::Delete, b"\x1b[3~"),
            (Key::Backspace, b"\x7f"),
            (Key::Tab, b"\t"),
            (Key::Enter, b"\r"),
        ] {
            assert_eq!(
                input_bytes(&key(key_code, egui::Modifiers::NONE), false, false).unwrap(),
                bytes
            );
        }
        assert_eq!(
            input_bytes(&key(Key::C, egui::Modifiers::CTRL), false, false),
            Some(vec![3])
        );
        assert_eq!(
            input_bytes(&key(Key::Z, egui::Modifiers::CTRL), false, false),
            Some(vec![26])
        );
        assert_eq!(input_bytes(&Event::Copy, false, false), Some(vec![3]));
        assert_eq!(input_bytes(&Event::Cut, false, false), Some(vec![24]));
        assert_eq!(
            input_bytes(&key(Key::ArrowUp, egui::Modifiers::NONE), true, false),
            Some(b"\x1bOA".to_vec())
        );
    }
    #[test]
    fn text_and_bracketed_paste_preserve_unicode() {
        assert_eq!(
            input_bytes(&Event::Text("λ".into()), false, false),
            Some("λ".as_bytes().into())
        );
        assert_eq!(
            input_bytes(&Event::Paste("a\nb".into()), false, true),
            Some(b"\x1b[200~a\nb\x1b[201~".to_vec())
        );
    }
    /// m17: a pasted ESC[201~ must not end bracketed paste and run the rest as keys.
    #[test]
    fn bracketed_paste_strips_escape() {
        assert_eq!(
            input_bytes(&Event::Paste("a\x1b[201~rm -rf ~\r".into()), false, true),
            Some(b"\x1b[200~a[201~rm -rf ~\r\x1b[201~".to_vec())
        );
    }
    /// m16: AltGr (Ctrl+Alt) is no control byte; Alt+key is meta; modified cursor keys
    /// use xterm parameters.
    #[test]
    fn modifier_combinations() {
        let ctrl_alt = egui::Modifiers {
            ctrl: true,
            command: !cfg!(target_os = "macos"),
            alt: true,
            ..Default::default()
        };
        assert_eq!(input_bytes(&key(Key::Q, ctrl_alt), false, false), None);
        let alt = egui::Modifiers::ALT;
        let alt_shift = egui::Modifiers { shift: true, ..alt };
        if !cfg!(target_os = "macos") {
            assert_eq!(
                input_bytes(&key(Key::B, alt), false, false),
                Some(b"\x1bb".to_vec())
            );
            assert_eq!(
                input_bytes(&key(Key::B, alt_shift), false, false),
                Some(b"\x1bB".to_vec())
            );
            assert_eq!(
                input_bytes(&key(Key::Num1, alt), false, false),
                Some(b"\x1b1".to_vec())
            );
            // egui-winit also sends the Alt+b text; it is dropped once, right after.
            let meta = meta_char(Key::B, alt);
            assert!(is_meta_echo(meta, &Event::Text("b".into())));
            assert!(!is_meta_echo(meta, &Event::Text("c".into())));
            assert!(!is_meta_echo(None, &Event::Text("b".into())));
        }
        let ctrl = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        let shift = egui::Modifiers::SHIFT;
        for (code, modifiers, bytes) in [
            (Key::ArrowLeft, ctrl, b"\x1b[1;5D".as_slice()),
            (Key::ArrowUp, shift, b"\x1b[1;2A"),
            (Key::Home, alt, b"\x1b[1;3H"),
            (
                Key::End,
                egui::Modifiers {
                    shift: true,
                    ..ctrl
                },
                b"\x1b[1;6F",
            ),
            (Key::Delete, ctrl, b"\x1b[3;5~"),
            (Key::PageDown, shift, b"\x1b[6;2~"),
        ] {
            assert_eq!(
                input_bytes(&key(code, modifiers), false, false).as_deref(),
                Some(bytes)
            );
        }
        // Plain Esc reaches the program (vim, less, fzf).
        assert_eq!(
            input_bytes(&key(Key::Escape, egui::Modifiers::NONE), false, false),
            Some(vec![0x1b])
        );
    }
}
