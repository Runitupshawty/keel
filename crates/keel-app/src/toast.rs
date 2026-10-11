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
    /// Shown since (slides in, Task 23).
    born: Instant,
    /// A button on the toast (label, what it runs).
    pub action: Option<(String, Action)>,
    /// A second button that only closes the toast ("Not now").
    pub dismiss: Option<String>,
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

    /// An info toast with a button (kept longer, so it can be clicked).
    pub fn offer(&mut self, text: impl Into<String>, button: &str, action: Action) {
        self.push(text.into(), Level::Info, Duration::from_secs(30));
        if let Some(t) = self.list.last_mut() {
            t.action = Some((button.into(), action));
        }
    }

    /// Closes the toasts whose button runs `action` (it no longer can).
    pub fn drop_action(&mut self, action: &Action) {
        self.list
            .retain(|t| t.action.as_ref().is_none_or(|(_, a)| a != action));
    }

    /// A question: `button` runs `action`, `dismiss` closes it (kept 30 s).
    pub fn ask(&mut self, text: impl Into<String>, button: &str, action: Action, dismiss: &str) {
        self.offer(text, button, action);
        if let Some(t) = self.list.last_mut() {
            t.dismiss = Some(dismiss.into());
        }
    }

    fn push(&mut self, text: String, level: Level, ttl: Duration) {
        // Repeats (e.g. a watcher refresh failing twice) extend the old toast.
        self.list.retain(|t| t.text != text);
        self.list.push(Toast {
            text,
            level,
            until: Instant::now() + ttl,
            born: Instant::now(),
            action: None,
            dismiss: None,
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
        // One area per toast, stacked up from the bottom; a new one slides in from the
        // right edge.
        let dur = crate::anim::duration(ctx);
        let mut y = -36.0;
        for (i, t) in self.list.iter().enumerate().rev() {
            let age = now.saturating_duration_since(t.born).as_secs_f32();
            let slide = 1.0 - crate::anim::at(0.0, 1.0, age, dur);
            if slide > 0.0 {
                ctx.request_repaint();
            }
            let shown = egui::Area::new(egui::Id::new(("keel-toast", t.text.as_str())))
                .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0 + slide * 440.0, y])
                .constrain(false)
                .order(egui::Order::Foreground)
                .interactable(t.action.is_some())
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(420.0);
                        let text = egui::RichText::new(&t.text);
                        let r = ui.label(if t.level == Level::Error {
                            text.color(error_color)
                        } else {
                            text
                        });
                        // Screen readers announce it (errors interrupt).
                        crate::a11y::live_label(&r, t.level == Level::Error);
                        ui.horizontal(|ui| {
                            if let Some((button, action)) = &t.action {
                                if ui.button(button.as_str()).clicked() {
                                    clicked = Some((i, Some(action.clone())));
                                }
                            }
                            if let Some(dismiss) = &t.dismiss {
                                if ui.button(dismiss.as_str()).clicked() {
                                    clicked = Some((i, None));
                                }
                            }
                        });
                    });
                });
            y -= shown.response.rect.height() + 4.0;
        }
        let (i, action) = clicked?;
        self.list.remove(i);
        action
    }
}
