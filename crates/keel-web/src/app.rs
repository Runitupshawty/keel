//! The web client's UI: a token prompt, then the library sidebar (sources, devices,
//! jobs), two panes or the search tab, and the preview panel with previewed operations
//! (rename, delete, tag: preview, then execute; a job shows as done only when the daemon
//! says it finished).

use crate::conn::{Conn, Reply, State};
use crate::types::*;
use crate::util::{human, parent};
use crate::web::{self, Events, Socket};
use base64::Engine;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::HashMap;

/// Preview images kept (by content id, else path + size + mtime).
const CACHE: usize = 64;

/// What an outstanding call was for.
enum Want {
    Version,
    Sources,
    Devices,
    Jobs,
    List { pane: usize, path: String },
    Stat(String),
    Preview { path: String, key: String },
    Thumb { path: String, key: String },
    Search,
    Plan,
    Execute,
    Job,
    Link,
}

#[derive(Default)]
struct Pane {
    path: String,
    edit: String,
    entries: Vec<EntryInfo>,
    truncated: bool,
    error: Option<String>,
}

enum Shown {
    Nothing,
    Loading,
    Text { text: String, truncated: bool },
    Image(egui::TextureHandle),
    Message(String),
}

enum PlanState {
    Review,
    Executing,
    Running { job: i64, progress: f32 },
    Done(String),
    Failed(String),
}

struct PlanDialog {
    preview: PlanPreview,
    state: PlanState,
}

#[derive(PartialEq)]
enum Tab {
    Browse,
    Search,
}

pub struct WebApp {
    conn: Conn<Socket>,
    events: Events,
    gen: u64,
    ctx: egui::Context,
    wants: HashMap<u64, Want>,
    // sign-in
    token_input: String,
    remember: bool,
    notice: Option<String>,
    // library
    library: String,
    sources: Vec<SourceInfo>,
    devices: Result<Devices, String>,
    jobs: Vec<JobInfo>,
    jobs_asked: f64,
    /// When a running plan's job was last asked about.
    polled: f64,
    // browsing
    tab: Tab,
    panes: [Pane; 2],
    active: usize,
    query: String,
    hits: Vec<Hit>,
    // preview
    selected: Option<EntryInfo>,
    stat: Option<StatInfo>,
    shown: Shown,
    images: HashMap<String, egui::TextureHandle>,
    rename_to: String,
    tag: String,
    plan: Option<PlanDialog>,
    error: Option<String>,
}

fn parse<T: DeserializeOwned>(r: Result<Value, String>) -> Result<T, String> {
    serde_json::from_value(r?).map_err(|e| format!("unexpected answer: {e}"))
}

fn is_media(name: &str) -> bool {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    matches!(
        ext.as_deref(),
        Some(
            "jpg"
                | "jpeg"
                | "png"
                | "gif"
                | "webp"
                | "bmp"
                | "tif"
                | "tiff"
                | "heic"
                | "mp4"
                | "mov"
                | "mkv"
                | "avi"
                | "webm"
        )
    )
}

impl WebApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = WebApp {
            conn: Conn::default(),
            events: Events::default(),
            gen: 0,
            ctx: cc.egui_ctx.clone(),
            wants: HashMap::new(),
            token_input: String::new(),
            remember: false,
            notice: None,
            library: String::new(),
            sources: Vec::new(),
            devices: Err(String::new()),
            jobs: Vec::new(),
            jobs_asked: 0.0,
            polled: 0.0,
            tab: Tab::Browse,
            panes: Default::default(),
            active: 0,
            query: String::new(),
            hits: Vec::new(),
            selected: None,
            stat: None,
            shown: Shown::Nothing,
            images: HashMap::new(),
            rename_to: String::new(),
            tag: String::new(),
            plan: None,
            error: None,
        };
        if web::scrub_address() {
            app.notice = Some(
                "The address carried a token: it was ignored and removed. Enter the token below."
                    .into(),
            );
        }
        if let Some(token) = web::remembered_token() {
            app.remember = true;
            app.sign_in(token);
        }
        app
    }

    fn sign_in(&mut self, token: String) {
        self.gen += 1;
        let (gen, events, ctx) = (self.gen, self.events.clone(), self.ctx.clone());
        self.conn
            .sign_in(token, move || Socket::open(gen, events, ctx));
    }

    fn call(&mut self, method: &str, params: Value, want: Want) {
        if let Some(id) = self.conn.call(method, params) {
            self.wants.insert(id, want);
        }
    }

    fn open(&mut self, pane: usize, path: String) {
        self.active = pane;
        self.panes[pane].edit = path.clone();
        self.call(
            "list",
            json!({"path": path, "max": 5000}),
            Want::List { pane, path },
        );
    }

    fn refresh(&mut self) {
        if let Some(path) = self.selected.as_ref().map(|e| e.path.clone()) {
            self.call("stat", json!({ "path": path }), Want::Stat(path));
        }
        self.call("sources.list", json!({}), Want::Sources);
        self.call("jobs.list", json!({}), Want::Jobs);
        for pane in 0..2 {
            let path = self.panes[pane].path.clone();
            if !path.is_empty() {
                self.open(pane, path);
            }
        }
    }

    fn select(&mut self, e: EntryInfo) {
        self.stat = None;
        self.rename_to = e.name.clone();
        self.call("stat", json!({"path": e.path}), Want::Stat(e.path.clone()));
        if e.is_dir {
            self.shown = Shown::Nothing;
        } else {
            let key = format!("{}|{}|{:?}", e.path, e.size, e.modified);
            match self.images.get(&key) {
                Some(t) => self.shown = Shown::Image(t.clone()),
                None => {
                    self.shown = Shown::Loading;
                    let path = e.path.clone();
                    if is_media(&e.name) {
                        let params = json!({"path": path, "size": "thumb1024"});
                        self.call("media.thumb", params, Want::Thumb { path, key });
                    } else {
                        let params = json!({"path": path, "max_px": 1024});
                        self.call("preview.render", params, Want::Preview { path, key });
                    }
                }
            }
        }
        self.selected = Some(e);
    }

    fn texture(&mut self, key: String, b64: &str) -> Shown {
        let image = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| e.to_string())
            .and_then(|b| image::load_from_memory(&b).map_err(|e| e.to_string()));
        match image {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                let tex = self.ctx.load_texture(&key, color, Default::default());
                if self.images.len() >= CACHE {
                    self.images.clear();
                }
                self.images.insert(key, tex.clone());
                Shown::Image(tex)
            }
            Err(e) => Shown::Message(format!("cannot show this image: {e}")),
        }
    }

    fn is_selected(&self, path: &str) -> bool {
        self.selected.as_ref().is_some_and(|s| s.path == path)
    }

    fn plan(&mut self, method: &str, params: Value) {
        self.plan = None;
        self.call(method, params, Want::Plan);
    }

    fn on_reply(&mut self, r: Reply) {
        let Some(want) = self.wants.remove(&r.id) else {
            return;
        };
        match want {
            Want::Version => {
                if let Ok(v) = parse::<VersionInfo>(r.result) {
                    self.library = v.library;
                }
            }
            Want::Sources => match parse::<Vec<SourceInfo>>(r.result) {
                Ok(s) => self.sources = s,
                Err(e) => self.error = Some(e),
            },
            Want::Devices => self.devices = parse(r.result),
            Want::Jobs => {
                if let Ok(mut jobs) = parse::<Vec<JobInfo>>(r.result) {
                    jobs.truncate(20);
                    self.jobs = jobs;
                }
            }
            Want::List { pane, path } => {
                let p = &mut self.panes[pane];
                match parse::<Listing>(r.result) {
                    Ok(l) => {
                        p.path = path.clone();
                        p.edit = path;
                        p.entries = l.entries;
                        p.truncated = l.truncated;
                        p.error = None;
                    }
                    Err(e) => p.error = Some(e),
                }
            }
            Want::Stat(path) => {
                if self.is_selected(&path) {
                    self.stat = parse(r.result).ok();
                }
            }
            Want::Thumb { path, key } => {
                if !self.is_selected(&path) {
                    return;
                }
                match parse::<Thumb>(r.result) {
                    Ok(t) => self.shown = self.texture(key, &t.data),
                    // Not a photo the sidecar store can thumbnail: the previewer then.
                    Err(_) => {
                        let params = json!({"path": path, "max_px": 1024});
                        self.call("preview.render", params, Want::Preview { path, key });
                    }
                }
            }
            Want::Preview { path, key } => {
                if !self.is_selected(&path) {
                    return;
                }
                self.shown = match parse::<Rendered>(r.result) {
                    Ok(p) => match p.kind {
                        RenderKind::Text => Shown::Text {
                            text: p.text.unwrap_or_default(),
                            truncated: p.truncated,
                        },
                        RenderKind::Image => {
                            let key = p.content_id.unwrap_or(key);
                            self.texture(key, p.png.as_deref().unwrap_or_default())
                        }
                        RenderKind::None => Shown::Message(p.message.unwrap_or_default()),
                    },
                    Err(e) => Shown::Message(e),
                };
            }
            Want::Search => match parse::<Vec<Hit>>(r.result) {
                Ok(h) => self.hits = h,
                Err(e) => self.error = Some(e),
            },
            Want::Plan => match parse::<PlanPreview>(r.result) {
                Ok(preview) => {
                    self.plan = Some(PlanDialog {
                        preview,
                        state: PlanState::Review,
                    })
                }
                Err(e) => self.error = Some(e),
            },
            Want::Execute => {
                let Some(dialog) = &mut self.plan else { return };
                dialog.state = match parse::<Executed>(r.result) {
                    Ok(Executed { job: Some(job), .. }) => {
                        PlanState::Running { job, progress: 0.0 }
                    }
                    Ok(_) => PlanState::Done("Done.".into()),
                    Err(e) => PlanState::Failed(e),
                };
                if matches!(dialog.state, PlanState::Done(_)) {
                    self.refresh();
                }
            }
            Want::Job => {
                let Ok(info) = parse::<JobInfo>(r.result) else {
                    return;
                };
                let Some(dialog) = &mut self.plan else { return };
                if let PlanState::Running { job, .. } = dialog.state {
                    if job == info.id && info.finished() {
                        // The selection may have moved or gone.
                        self.selected = None;
                        self.stat = None;
                        self.shown = Shown::Nothing;
                        dialog.state = match info.status.as_str() {
                            "done" => PlanState::Done("Done.".into()),
                            s => PlanState::Failed(format!(
                                "{s}: {}",
                                info.log.unwrap_or_default().trim()
                            )),
                        };
                        self.refresh();
                    }
                }
            }
            Want::Link => match parse::<FileLink>(r.result) {
                Ok(link) => web::download(&link.url),
                Err(e) => self.error = Some(e),
            },
        }
    }

    fn on_note(&mut self, method: &str, params: &Value) {
        match method {
            "library.changed" => self.refresh(),
            "job.progress" => {
                let now = web::now();
                if now - self.jobs_asked > 1.0 {
                    self.jobs_asked = now;
                    self.call("jobs.list", json!({}), Want::Jobs);
                }
                let Some(dialog) = &mut self.plan else { return };
                if let PlanState::Running { job, progress } = &mut dialog.state {
                    if params["id"].as_i64() == Some(*job) {
                        *progress = params["progress"].as_f64().unwrap_or(0.0) as f32;
                        // Only the daemon's job record says how it ended.
                        let status = params["status"].as_str().unwrap_or_default();
                        if ["done", "failed", "cancelled"].contains(&status) {
                            let id = *job;
                            self.call("jobs.info", json!({ "id": id }), Want::Job);
                        }
                    }
                }
            }
            "net.event" => self.call("devices.list", json!({}), Want::Devices),
            _ => {}
        }
    }

    fn pump(&mut self) {
        let now = web::now();
        let events: Vec<_> = self.events.borrow_mut().drain(..).collect();
        for (gen, ev) in events {
            if gen != self.gen {
                continue;
            }
            let was_online = self.conn.state == State::Online;
            self.conn.on_event(ev, now);
            if !was_online && self.conn.state == State::Online {
                self.wants.clear();
                self.call("version", json!({}), Want::Version);
                self.call("devices.list", json!({}), Want::Devices);
                self.refresh();
            }
            if let State::Refused(why) = &self.conn.state {
                // A remembered token that no longer works is forgotten.
                web::remember_token(None);
                self.notice = Some(format!("The daemon refused the token ({why})."));
                self.conn.sign_out();
            }
        }
        for r in self.conn.take_replies() {
            self.on_reply(r);
        }
        for (method, params) in self.conn.take_notes() {
            self.on_note(&method, &params);
        }
        if let State::Retrying { at } = self.conn.state {
            if now >= at {
                self.gen += 1;
                let (gen, events, ctx) = (self.gen, self.events.clone(), self.ctx.clone());
                self.conn.tick(now, move || Socket::open(gen, events, ctx));
            } else {
                let wait = std::time::Duration::from_secs_f64(at - now);
                self.ctx.request_repaint_after(wait);
            }
        }
        // A finished job's notification may be missed: ask now and then as well.
        if let Some(PlanState::Running { job, .. }) = self.plan.as_ref().map(|d| &d.state) {
            let job = *job;
            if now - self.polled > 2.0 {
                self.polled = now;
                self.call("jobs.info", json!({ "id": job }), Want::Job);
            }
            self.ctx
                .request_repaint_after(std::time::Duration::from_secs(2));
        }
    }

    fn login(&mut self, ui: &mut egui::Ui) {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.heading("Keel");
            ui.label("Enter the token from daemon.token in Keel's configuration folder on the daemon's machine.");
            if let Some(n) = &self.notice {
                ui.colored_label(ui.visuals().warn_fg_color, n);
            }
            ui.add_space(8.0);
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.token_input)
                    .password(true)
                    .hint_text("token")
                    .desired_width(360.0),
            );
            ui.checkbox(&mut self.remember, "Remember on this device")
                .on_hover_text("Keeps the token in this browser's local storage. Leave it off on a shared computer.");
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("Connect").clicked() || enter) && !self.token_input.trim().is_empty() {
                let token = std::mem::take(&mut self.token_input).trim().to_owned();
                web::remember_token(self.remember.then_some(token.as_str()));
                self.notice = None;
                self.sign_in(token);
            }
        });
    }

    fn top(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Keel");
            if !self.library.is_empty() {
                ui.label(format!("library {}", self.library));
            }
            ui.separator();
            ui.selectable_value(&mut self.tab, Tab::Browse, "Browse");
            ui.selectable_value(&mut self.tab, Tab::Search, "Search");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Sign out").clicked() {
                    web::remember_token(None);
                    self.conn.sign_out();
                    self.wants.clear();
                }
                let state = &self.conn.state;
                let color = match state {
                    State::Online => egui::Color32::from_rgb(60, 160, 80),
                    State::Refused(_) => ui.visuals().error_fg_color,
                    _ => ui.visuals().warn_fg_color,
                };
                ui.colored_label(color, format!("● {}", state.label()));
            });
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Sources");
            let sources = self.sources.clone();
            for s in sources {
                let text = format!("{} ({})", s.label, s.status);
                if ui
                    .selectable_label(false, text)
                    .on_hover_text(&s.root)
                    .clicked()
                {
                    self.tab = Tab::Browse;
                    self.open(self.active, format!("library://{}/", s.id));
                }
            }
            if self.sources.is_empty() {
                ui.weak("No sources yet.");
            }
            ui.separator();
            ui.heading("Devices");
            match &self.devices {
                Ok(d) => {
                    ui.label(format!("{} (this device)", d.label));
                    for p in &d.peers {
                        ui.label(format!("{}: {}", p.label, p.link));
                    }
                }
                Err(e) if e.is_empty() => {
                    ui.weak("…");
                }
                Err(e) => {
                    ui.weak(e);
                }
            }
            ui.separator();
            ui.heading("Jobs");
            for j in &self.jobs {
                ui.horizontal(|ui| {
                    ui.label(format!("#{} {}", j.id, j.kind));
                    if j.finished() {
                        ui.weak(&j.status);
                    } else {
                        ui.add(egui::ProgressBar::new(j.progress).desired_width(80.0));
                    }
                });
            }
        });
    }

    fn pane(&mut self, ui: &mut egui::Ui, i: usize) {
        let mut go = None;
        ui.horizontal(|ui| {
            if ui.button("⬆").on_hover_text("Up").clicked() {
                go = parent(&self.panes[i].path);
            }
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.panes[i].edit)
                    .desired_width(f32::INFINITY)
                    .hint_text("library://… or a path on the daemon's machine"),
            );
            if edit.lost_focus() && ui.input(|k| k.key_pressed(egui::Key::Enter)) {
                go = Some(self.panes[i].edit.trim().to_owned());
            }
            if edit.gained_focus() {
                self.active = i;
            }
        });
        if let Some(e) = &self.panes[i].error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        let mut pick = None;
        egui::ScrollArea::vertical()
            .id_salt(("pane", i))
            .auto_shrink(false)
            .show(ui, |ui| {
                for e in &self.panes[i].entries {
                    let label = if e.is_dir {
                        format!("📁 {}", e.name)
                    } else {
                        e.name.clone()
                    };
                    ui.horizontal(|ui| {
                        let r = ui.selectable_label(self.is_selected(&e.path), label);
                        if r.clicked() {
                            pick = Some(e.clone());
                        }
                        if r.double_clicked() && e.is_dir {
                            go = Some(e.path.clone());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if let Some(m) = e.modified {
                                ui.weak(web::date(m));
                            }
                            if !e.is_dir {
                                ui.weak(human(e.size));
                            }
                        });
                    });
                }
                if self.panes[i].truncated {
                    ui.weak("(more entries not shown)");
                }
            });
        if let Some(e) = pick {
            self.active = i;
            self.select(e);
        }
        if let Some(path) = go.filter(|p| !p.is_empty()) {
            self.open(i, path);
        }
    }

    fn search(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .desired_width(f32::INFINITY)
                    .hint_text("words, \"phrases\", kind:, ext:, size:, dm:, source:, tag:"),
            );
            if field.lost_focus() && ui.input(|k| k.key_pressed(egui::Key::Enter)) {
                let q = self.query.clone();
                self.call("search", json!({"query": q, "max": 500}), Want::Search);
            }
        });
        let mut pick = None;
        let mut go = None;
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                for h in &self.hits {
                    ui.horizontal(|ui| {
                        let r = ui
                            .selectable_label(self.is_selected(&h.path), &h.name)
                            .on_hover_text(&h.path);
                        if r.clicked() {
                            pick = Some(EntryInfo {
                                name: h.name.clone(),
                                path: h.path.clone(),
                                is_dir: h.is_dir,
                                size: h.size,
                                modified: h.modified,
                                hidden: false,
                            });
                        }
                        if r.double_clicked() {
                            go = Some(if h.is_dir {
                                h.path.clone()
                            } else {
                                parent(&h.path).unwrap_or_default()
                            });
                        }
                        ui.weak(&h.source_label);
                    });
                }
            });
        if let Some(e) = pick {
            self.select(e);
        }
        if let Some(path) = go.filter(|p| !p.is_empty()) {
            self.tab = Tab::Browse;
            self.open(self.active, path);
        }
    }

    fn preview(&mut self, ui: &mut egui::Ui) {
        let Some(e) = self.selected.clone() else {
            ui.weak("Select a file to preview it.");
            return;
        };
        ui.add(egui::Label::new(egui::RichText::new(&e.name).heading()).wrap());
        ui.add(egui::Label::new(egui::RichText::new(&e.path).weak()).wrap());
        if !e.is_dir {
            ui.label(human(e.size));
        }
        if let Some(info) = self.stat.as_ref().and_then(|s| s.indexed.as_ref()) {
            ui.label(format!("in {}", info.source_label));
            if !info.tags.is_empty() {
                ui.label(format!("tags: {}", info.tags.join(", ")));
            }
        }
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if !e.is_dir && ui.button("Download").clicked() {
                self.call("file.get", json!({"path": e.path}), Want::Link);
            }
            if ui.button("Delete…").clicked() {
                self.plan("plan", json!({"op": "delete", "paths": [e.path]}));
            }
        });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.rename_to).desired_width(160.0));
            if ui.button("Rename…").clicked() && !self.rename_to.trim().is_empty() {
                let name = self.rename_to.trim().to_owned();
                self.plan(
                    "plan",
                    json!({"op": "rename", "paths": [e.path], "new_name": name}),
                );
            }
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.tag)
                    .hint_text("tag")
                    .desired_width(160.0),
            );
            if ui.button("Tag…").clicked() && !self.tag.trim().is_empty() {
                let tag = self.tag.trim().to_owned();
                self.plan("tags.add", json!({"tag": tag, "paths": [e.path]}));
            }
        });
        ui.separator();
        egui::ScrollArea::both()
            .auto_shrink(false)
            .show(ui, |ui| match &self.shown {
                Shown::Nothing => {}
                Shown::Loading => {
                    ui.spinner();
                }
                Shown::Text { text, truncated } => {
                    ui.add(egui::Label::new(egui::RichText::new(text).monospace()).wrap());
                    if *truncated {
                        ui.weak("(cut short)");
                    }
                }
                Shown::Image(t) => {
                    let size = t.size_vec2();
                    let scale = (ui.available_width() / size.x).min(1.0);
                    ui.image((t.id(), size * scale));
                }
                Shown::Message(m) => {
                    ui.weak(m);
                }
            });
    }

    fn plan_window(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.plan else { return };
        let mut close = false;
        let mut execute = None;
        egui::Window::new("Preview")
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let p = &dialog.preview;
                ui.strong(&p.summary);
                for c in p.changes.iter().take(50) {
                    let to =
                        c.to.as_deref()
                            .map(|t| format!(" -> {t}"))
                            .unwrap_or_default();
                    ui.label(format!(
                        "{} {}{to}",
                        c.action,
                        c.path.as_deref().unwrap_or_default()
                    ));
                }
                if p.changes.len() > 50 {
                    ui.weak(format!("… {} more", p.changes.len() - 50));
                }
                for w in &p.warnings {
                    ui.colored_label(ui.visuals().warn_fg_color, format!("⚠ {}", w.message));
                }
                ui.separator();
                match &dialog.state {
                    PlanState::Review => {
                        ui.horizontal(|ui| {
                            if ui.button("Execute").clicked() {
                                execute = Some(json!({
                                    "plan_id": p.plan_id,
                                    "input_hash": p.input_hash,
                                }));
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    }
                    PlanState::Executing => {
                        ui.spinner();
                    }
                    PlanState::Running { job, progress } => {
                        ui.label(format!("Job #{job} running…"));
                        ui.add(egui::ProgressBar::new(*progress).show_percentage());
                    }
                    PlanState::Done(m) => {
                        ui.label(m);
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    }
                    PlanState::Failed(m) => {
                        ui.colored_label(ui.visuals().error_fg_color, m);
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    }
                }
            });
        if let Some(params) = execute {
            dialog.state = PlanState::Executing;
            self.call("execute", params, Want::Execute);
        }
        if close {
            self.plan = None;
        }
    }
}

impl eframe::App for WebApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        if matches!(self.conn.state, State::NeedToken | State::Refused(_)) {
            egui::CentralPanel::default().show(ctx, |ui| self.login(ui));
            return;
        }
        egui::TopBottomPanel::top("top").show(ctx, |ui| self.top(ui));
        if let Some(e) = self.error.clone() {
            egui::TopBottomPanel::bottom("error").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                    if ui.small_button("✕").clicked() {
                        self.error = None;
                    }
                });
            });
        }
        egui::SidePanel::left("library")
            .resizable(true)
            .default_width(220.0)
            .show(ctx, |ui| self.sidebar(ui));
        egui::SidePanel::right("preview")
            .resizable(true)
            .default_width(360.0)
            .show(ctx, |ui| self.preview(ui));
        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Browse => {
                ui.columns(2, |cols| {
                    self.pane(&mut cols[0], 0);
                    self.pane(&mut cols[1], 1);
                });
            }
            Tab::Search => self.search(ui),
        });
        self.plan_window(ctx);
    }
}
