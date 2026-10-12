//! Compact native search viewport, owned by the main process/profile.
use crate::{keys::Action, state::AppState, tab::Tab};

pub fn id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("keel-quick-search")
}

pub struct QuickSearch {
    state: AppState,
    profile: String,
    pub open: bool,
    /// The window level last sent to this window (sent again after it reopens).
    on_top_applied: Option<bool>,
}

impl QuickSearch {
    pub fn new(parent: &AppState) -> Self {
        // Only the look and the omissions come from the main window: remotes and clouds stay
        // empty here so this state never re-registers the shared router's providers.
        let settings = crate::settings::Settings {
            theme: parent.settings.theme.clone(),
            column_widths: parent.settings.column_widths.clone(),
            omissions: parent.settings.omissions.clone(),
            always_on_top: parent.settings.always_on_top,
            ..Default::default()
        };
        let session = crate::session::Session::single(parent.home.clone());
        let mut state = AppState::restore(
            parent.ctx.clone(),
            parent.router.clone(),
            session,
            settings,
            parent.home.clone(),
        );
        state.panes[0].tabs = vec![Tab::search(parent.home.clone())];
        state.panes[0].active = 0;
        state.panes[0].focus_filter = true;
        state.dual = false;
        Self {
            state,
            profile: crate::profiles::current(),
            open: true,
            on_top_applied: None,
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, parent: &mut AppState) {
        if self.profile != crate::profiles::current() {
            *self = Self::new(parent);
        }
        if !self.open {
            self.on_top_applied = None;
            return;
        }
        let s = &mut self.state;
        // Each window keeps its own Manage omissions dialog; the lists are shared.
        if !s.settings.omissions.same_lists(&parent.settings.omissions) {
            s.settings.omissions.copy_lists(&parent.settings.omissions);
            s.refresh_omissions();
        }
        s.searcher = parent.searcher.clone();
        s.search_reason = parent.search_reason.clone();
        s.theme = parent.theme.clone();
        let keys = parent.keys.clone();
        s.settings.always_on_top = parent.settings.always_on_top;
        ctx.show_viewport_immediate(
            id(),
            egui::ViewportBuilder::default()
                .with_title("Keel Quick Search")
                .with_inner_size([720.0, 420.0])
                .with_min_inner_size([420.0, 220.0]),
            |ctx, _| {
                let on_top = s.settings.always_on_top;
                if self.on_top_applied != Some(on_top) {
                    self.on_top_applied = Some(on_top);
                    ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level(on_top)));
                }
                if ctx.input(|i| i.viewport().close_requested()) {
                    self.open = false;
                }
                // Esc hides the window (it keeps its query for the next hotkey press).
                if s.dialog.is_none()
                    && !s.settings.omissions.manage
                    && !ctx.is_context_menu_open()
                    && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
                {
                    self.open = false;
                }
                s.ctx = ctx.clone();
                s.drain();
                s.tick();
                let mut actions = Vec::new();
                let keys_on = !ctx.is_context_menu_open() && !s.settings.omissions.manage;
                actions.extend(crate::keys::actions_with_terminal(
                    ctx, keys_on, false, &keys,
                ));
                egui::CentralPanel::default().show(ctx, |ui| {
                    // No backend picker, chips, sidebar, tab strip or file-manager toolbar.
                    let pane = &mut s.panes[0];
                    let focus = std::mem::take(&mut pane.focus_filter);
                    if let crate::tab::TabKind::Search { query, due, .. } = &mut pane.tab_mut().kind
                    {
                        let r = ui.add(
                            egui::TextEdit::singleline(query)
                                .hint_text("Search files")
                                .desired_width(f32::INFINITY),
                        );
                        if focus {
                            r.request_focus();
                        }
                        if r.changed() {
                            *due = Some(std::time::Instant::now() + crate::search_tab::DEBOUNCE);
                            ctx.request_repaint_after(crate::search_tab::DEBOUNCE);
                        }
                        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            *due = Some(std::time::Instant::now());
                        }
                        r.context_menu(|ui| {
                            if ui.button("Manage omissions").clicked() {
                                actions.push(Action::ManageOmissions);
                                ui.close_menu();
                            }
                            if ui.button("Always on top (toggle)").clicked() {
                                actions.push(Action::ToggleOnTop);
                                ui.close_menu();
                            }
                        });
                    }
                    if let Some(error) = &pane.tab().error {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                    let mut cx = crate::pane::ViewCx {
                        theme: &s.theme,
                        show_hidden: parent.show_hidden,
                        thumbs: &mut s.thumbs,
                        active: false,
                        banner: None,
                        preview: None,
                        column_widths: &mut s.settings.column_widths,
                        library: &s.library,
                        drives: &s.sidebar.drives,
                        searcher: s.searcher.as_ref().map(|x| x.name()),
                        tags_column: false,
                        media: &mut s.media,
                        sizes: &s.sizes,
                        keys: true,
                    };
                    crate::view_details::ui(ui, (0, 0), pane.tab_mut(), &mut cx, &mut actions);
                });
                for action in actions {
                    match action {
                        Action::Enter => {
                            // Folders and Open location belong in the main file manager.
                            if s.tab(0)
                                .targets()
                                .first()
                                .is_some_and(|e| e.kind == keel_vfs::Kind::Dir)
                            {
                                let path = s.tab(0).targets()[0].path.clone();
                                parent.run(parent.active, Action::NewTabAt(path));
                                crate::single_instance::bring_to_front(&parent.ctx);
                            } else {
                                s.run(0, action);
                            }
                        }
                        Action::OpenLocation => {
                            if let Some(e) = s.tab(0).targets().first() {
                                if let Some(dir) = e.path.parent() {
                                    parent.reveal_in(
                                        parent.active,
                                        dir,
                                        e.path.name().to_owned(),
                                        true,
                                    );
                                    crate::single_instance::bring_to_front(&parent.ctx);
                                }
                            }
                        }
                        Action::OmitResults(_)
                        | Action::ManageOmissions
                        | Action::ShowOmitted
                        | Action::ToggleOnTop
                        | Action::Move(..)
                        | Action::SelectAll
                        | Action::ToggleSelect
                        | Action::CopyPath
                        | Action::Copy
                        | Action::Refresh
                        | Action::Delete
                        | Action::Type(_)
                        | Action::Backspace => s.run(0, action),
                        Action::Search | Action::FocusFilter => s.panes[0].focus_filter = true,
                        _ => {} // File-manager actions are available in the main window.
                    }
                }
                if s.settings.omissions.ui(ctx) {
                    s.refresh_omissions();
                }
                if let Some(action) = crate::dialogs::show(ctx, &mut s.dialog) {
                    s.run(0, action);
                }
                let color = ctx.style().visuals.error_fg_color;
                if let Some(action) = s.toasts.show(ctx, color) {
                    s.run(0, action);
                }
            },
        );
        if !parent.settings.omissions.same_lists(&s.settings.omissions) {
            parent.settings.omissions.copy_lists(&s.settings.omissions);
            parent.refresh_omissions();
        }
        parent.settings.always_on_top = s.settings.always_on_top;
    }
}

pub fn level(on_top: bool) -> egui::WindowLevel {
    if on_top {
        egui::WindowLevel::AlwaysOnTop
    } else {
        egui::WindowLevel::Normal
    }
}

pub fn new_window(folder: Option<&keel_vfs::VPath>, search: Option<&str>) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(["--new-window", "--profile", &crate::profiles::current()]);
    if let Some(path) = folder.and_then(|p| p.to_local_path()) {
        cmd.arg(path);
    }
    if let Some(query) = search {
        cmd.args(["--search", query]);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    cmd.spawn().map(|_| ())
}
