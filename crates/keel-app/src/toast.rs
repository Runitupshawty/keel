//! Transient notifications, bottom-right.

use crate::keys::Action;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Error,
}

pub struct Toast {
    pub text: String,
    pub level: Level,
    until: Instant,
    /// A button on the toast (label, what it runs).
    pub action: Option<(String, Action)>,
}

#[derive(Default)]
pub struct Toasts {
    pub list: Vec<Toast>,
}

impl Toasts {
    pub fn info(&mut self, text: impl Into<String>) {
        self.push(text.into(), Level::Info, Duration::from_secs(3));
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.push(text.into(), Level::Error, Duration::from_secs(6));
    }

    /// Replaces the toast starting with `prefix` (a progress line) with `text`.
    pub fn replace(&mut self, prefix: &str, text: String) {
        self.list.retain(|t| !t.text.starts_with(prefix));
        self.info(text);
    }

    /// An error toast with a button that runs `action` (kept longer, so it can be clicked).
    pub fn with_action(&mut self, text: impl Into<String>, button: &str, action: Action) {
        self.push(text.into(), Level::Error, Duration::from_secs(30));
        if let Some(t) = self.list.last_mut() {
            t.action = Some((button.into(), action));
        }
    }

    pub fn not_yet(&mut self, what: &str) {
        self.info(format!("{what}: not yet"));
    }

    fn push(&mut self, text: String, level: Level, ttl: Duration) {
        // Repeats (e.g. a watcher refresh failing twice) extend the old toast.
        self.list.retain(|t| t.text != text);
        self.list.push(Toast {
            text,
            level,
            until: Instant::now() + ttl,
            action: None,
        });
        if self.list.len() > 5 {
            self.list.remove(0);
        }
    }

    /// Draws the toasts; returns the action of a clicked toast button (that toast closes).
    pub fn show(&mut self, ctx: &egui::Context, error_color: egui::Color32) -> Option<Action> {
        let now = Instant::now();
        self.list.retain(|t| t.until > now);
        let next = self.list.iter().map(|t| t.until).min()?;
        let mut clicked = None;
        ctx.request_repaint_after(next - now);
        egui::Area::new(egui::Id::new("keel-toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -36.0])
            .order(egui::Order::Foreground)
            .interactable(self.list.iter().any(|t| t.action.is_some()))
            .show(ctx, |ui| {
                for (i, t) in self.list.iter().enumerate() {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(420.0);
                        let text = egui::RichText::new(&t.text);
                        ui.label(if t.level == Level::Error {
                            text.color(error_color)
                        } else {
                            text
                        });
                        if let Some((button, action)) = &t.action {
                            if ui.button(button.as_str()).clicked() {
                                clicked = Some((i, action.clone()));
                            }
                        }
                    });
                }
            });
        let (i, action) = clicked?;
        self.list.remove(i);
        Some(action)
    }
}
