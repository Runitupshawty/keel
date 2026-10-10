//! The web client's UI: a token prompt, then either the desktop layout (the library
//! sidebar with sources, devices, the Spacedrop inbox and jobs; two panes or the search
//! tab; the preview panel) or, below `layout::PHONE_BELOW` of width, the phone layout (one
//! pane or a media grid, a bottom bar with Browse / Search / Library / Devices, the preview
//! as a full-screen sheet with pinch zoom and swipes, long-press menus, pull to refresh).
//! Operations are previewed (rename, delete, tag, Spacedrop: preview, then execute; a job
//! shows as done only when the daemon says it finished). A share from the phone's share
//! sheet is claimed once signed in and sent to the device the user picks.

use crate::conn::{Conn, Reply, State};
use crate::gesture::{self, Pull};
use crate::layout::{Screen, Tab as PhoneTab};
use crate::share::{self, SharedFile};
use crate::types::*;
use crate::util::{human, parent};
use crate::web::{self, Events, Socket};
use base64::Engine;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Preview images kept (by content id, else path + size + mtime).
const CACHE: usize = 64;
/// Grid thumbnails kept.
const THUMBS: usize = 512;
/// Grid thumbnails asked for at once.
const THUMBS_IN_FLIGHT: usize = 6;

/// What an outstanding call was for.
enum Want {
    Version,
    Sources,
    Devices,
    Inbox,
    Jobs,
    List { pane: usize, path: String },
    Stat(String),
    Preview { path: String, key: String },
    Thumb { path: String, key: String },
    GridThumb { key: String },
    Search,
    Plan,
    Execute,
    Job,
    Link,
    Claim,
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

/// What a row's tap or long-press menu asked for (applied after the list is drawn).
enum Act {
    Open(EntryInfo),
    Select(EntryInfo),
    Download(String),
    Delete(String),
    Send(EntryInfo),
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
    /// Plain http to a non-loopback host: the token and files cross the network in the clear.
    insecure: bool,
    // layout
    screen: Screen,
    base_style: Arc<egui::Style>,
    // library
    library: String,
    sources: Vec<SourceInfo>,
    devices: Result<Devices, String>,
    inbox: Result<Inbox, String>,
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
    // phone
    grid: bool,
    tile: f32,
    thumbs: HashMap<String, egui::TextureHandle>,
    thumbs_asked: HashSet<String>,
    pull: Pull,
    zoom: f32,
    // preview
    selected: Option<EntryInfo>,
    stat: Option<StatInfo>,
    shown: Shown,
    images: HashMap<String, egui::TextureHandle>,
    rename_to: String,
    tag: String,
    plan: Option<PlanDialog>,
    error: Option<String>,
    // Spacedrop from the share sheet (or a file's "Send to device…")
    share: share::Flow,
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

/// Cache key of an entry's images.
fn image_key(e: &EntryInfo) -> String {
    format!("{}|{}|{:?}", e.path, e.size, e.modified)
}

fn hit_entry(h: &Hit) -> EntryInfo {
    EntryInfo {
        name: h.name.clone(),
        path: h.path.clone(),
        is_dir: h.is_dir,
        size: h.size,
        modified: h.modified,
        hidden: false,
    }
}

/// Bigger targets and text for fingers.
fn phone_style(base: &egui::Style) -> egui::Style {
    let mut s = base.clone();
    s.spacing.interact_size = egui::vec2(48.0, 44.0);
    s.spacing.button_padding = egui::vec2(14.0, 10.0);
    s.spacing.item_spacing = egui::vec2(10.0, 10.0);
    s.spacing.scroll.bar_width = 10.0;
    for font in s.text_styles.values_mut() {
        font.size += 3.0;
    }
    s
}

impl WebApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let width = cc.egui_ctx.screen_rect().width();
        let mut app = WebApp {
            conn: Conn::default(),
            events: Events::default(),
            gen: 0,
            ctx: cc.egui_ctx.clone(),
            wants: HashMap::new(),
            token_input: String::new(),
            remember: false,
            notice: None,
            insecure: web::insecure(),
            screen: Screen::new(width),
            base_style: cc.egui_ctx.style(),
            library: String::new(),
            sources: Vec::new(),
            devices: Err(String::new()),
            inbox: Err(String::new()),
            jobs: Vec::new(),
            jobs_asked: 0.0,
            polled: 0.0,
            tab: Tab::Browse,
            panes: Default::default(),
            active: 0,
            query: String::new(),
            hits: Vec::new(),
            grid: false,
            tile: 128.0,
            thumbs: HashMap::new(),
            thumbs_asked: HashSet::new(),
            pull: Pull::default(),
            zoom: 1.0,
            selected: None,
            stat: None,
            shown: Shown::Nothing,
            images: HashMap::new(),
            rename_to: String::new(),
            tag: String::new(),
            plan: None,
            error: None,
            share: share::Flow::from_query(""),
        };
        if web::scrub_address() {
            app.notice = Some(
                "The address carried a token: it was ignored and removed. Enter the token below."
                    .into(),
            );
        }
        app.share = share::Flow::from_query(&web::take_query());
        app.apply_style();
        if let Some(token) = web::remembered_token() {
            app.remember = true;
            app.sign_in(token);
        }
        app
    }

    fn apply_style(&self) {
        if self.screen.phone() {
            self.ctx.set_style(phone_style(&self.base_style));
        } else {
            self.ctx.set_style(self.base_style.clone());
        }
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
        self.call("spacedrop.inbox", json!({}), Want::Inbox);
        for pane in 0..2 {
            let path = self.panes[pane].path.clone();
            if !path.is_empty() {
                self.open(pane, path);
            }
        }
    }

    fn select(&mut self, e: EntryInfo) {
        self.stat = None;
        self.zoom = 1.0;
        self.rename_to = e.name.clone();
        self.call("stat", json!({"path": e.path}), Want::Stat(e.path.clone()));
        if e.is_dir {
            self.shown = Shown::Nothing;
        } else {
            let key = image_key(&e);
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

    fn decode(b64: &str) -> Result<egui::ColorImage, String> {
        let image = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| e.to_string())
            .and_then(|b| image::load_from_memory(&b).map_err(|e| e.to_string()))?;
        let rgba = image.to_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        Ok(egui::ColorImage::from_rgba_unmultiplied(
            size,
            rgba.as_raw(),
        ))
    }

    fn texture(&mut self, key: String, b64: &str) -> Shown {
        match Self::decode(b64) {
            Ok(color) => {
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

    /// A plan finished: the daemon's job record (or answer) said so.
    fn plan_done(&mut self) {
        if self
            .plan
            .as_ref()
            .is_some_and(|d| d.preview.operation == "spacedrop.send")
        {
            self.share.close();
        }
        self.refresh();
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
            Want::Inbox => self.inbox = parse(r.result),
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
            Want::GridThumb { key } => {
                if let Ok(color) = parse::<Thumb>(r.result).and_then(|t| Self::decode(&t.data)) {
                    if self.thumbs.len() >= THUMBS {
                        self.thumbs.clear();
                        self.thumbs_asked.clear();
                    }
                    let tex = self.ctx.load_texture(&key, color, Default::default());
                    self.thumbs.insert(key, tex);
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
                    self.plan_done();
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
                        self.screen.back();
                        dialog.state = match info.status.as_str() {
                            "done" => PlanState::Done("Done.".into()),
                            s => PlanState::Failed(format!(
                                "{s}: {}",
                                info.log.unwrap_or_default().trim()
                            )),
                        };
                        if matches!(dialog.state, PlanState::Done(_)) {
                            self.plan_done();
                        } else {
                            self.refresh();
                        }
                    }
                }
            }
            Want::Link => match parse::<FileLink>(r.result) {
                Ok(link) => web::download(&link.url),
                Err(e) => self.error = Some(e),
            },
            Want::Claim => {
                self.share.on_claimed(r.result);
                self.call("devices.list", json!({}), Want::Devices);
            }
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
            "net.event" => {
                self.call("devices.list", json!({}), Want::Devices);
                self.call("spacedrop.inbox", json!({}), Want::Inbox);
            }
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
                if let Some((method, params)) = self.share.claim() {
                    self.call(method, params, Want::Claim);
                }
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

    fn insecure_banner(&self, ui: &mut egui::Ui) {
        if self.insecure {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(
                        "⚠ Not encrypted: this page came over plain http from another machine, so \
                         the token and your files cross the network readable. Use https (a reverse \
                         proxy, or the tailnet's certificate) or a private network: see README, Phones.",
                    )
                    .color(ui.visuals().warn_fg_color),
                )
                .wrap(),
            );
        }
    }

    fn login(&mut self, ui: &mut egui::Ui) {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.heading("Keel");
            self.insecure_banner(ui);
            ui.label("Enter the token from daemon.token in Keel's configuration folder on the daemon's machine.");
            if self.share.active() {
                ui.label("Sign in to send the shared files.");
            }
            if let Some(n) = &self.notice {
                ui.colored_label(ui.visuals().warn_fg_color, n);
            }
            ui.add_space(8.0);
            let width = (ui.available_width() - 16.0).min(360.0);
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.token_input)
                    .password(true)
                    .hint_text("token")
                    .desired_width(width),
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

    fn state_dot(&self, ui: &mut egui::Ui) {
        let state = &self.conn.state;
        let color = match state {
            State::Online => egui::Color32::from_rgb(60, 160, 80),
            State::Refused(_) => ui.visuals().error_fg_color,
            _ => ui.visuals().warn_fg_color,
        };
        ui.colored_label(color, format!("● {}", state.label()));
    }

    fn sign_out(&mut self) {
        web::remember_token(None);
        self.conn.sign_out();
        self.wants.clear();
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
                    self.sign_out();
                }
                self.state_dot(ui);
            });
        });
        self.insecure_banner(ui);
    }

    fn sources_ui(&mut self, ui: &mut egui::Ui) {
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
                self.screen.show(PhoneTab::Browse);
                self.open(self.active, format!("library://{}/", s.id));
            }
        }
        if self.sources.is_empty() {
            ui.weak("No sources yet.");
        }
    }

    fn jobs_ui(&mut self, ui: &mut egui::Ui) {
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
    }

    /// Devices and the Spacedrop inbox: offers waiting for an answer, what arrived.
    fn devices_ui(&mut self, ui: &mut egui::Ui) {
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
                ui.add(egui::Label::new(egui::RichText::new(e).weak()).wrap());
            }
        }
        if self.devices.is_err() {
            return;
        }
        ui.separator();
        ui.heading("Inbox");
        let mut act = None;
        let mut answer = None;
        match &self.inbox {
            Ok(inbox) => {
                for o in &inbox.pending {
                    ui.label(format!(
                        "{} offers {} file(s), {}",
                        o.label,
                        o.files,
                        human(o.bytes)
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Accept…").clicked() {
                            answer = Some((o.peer.clone(), o.id.clone(), true));
                        }
                        if ui.button("Decline…").clicked() {
                            answer = Some((o.peer.clone(), o.id.clone(), false));
                        }
                    });
                }
                for e in &inbox.entries {
                    ui.horizontal(|ui| {
                        let label = if e.is_dir {
                            format!("📁 {}", e.name)
                        } else {
                            format!("{} ({})", e.name, human(e.size))
                        };
                        ui.label(label).on_hover_text(&e.path);
                        if e.is_dir {
                            if ui.small_button("Open").clicked() {
                                act = Some(Act::Open(e.clone()));
                            }
                        } else if ui.small_button("Download").clicked() {
                            act = Some(Act::Download(e.path.clone()));
                        }
                    });
                }
                if inbox.pending.is_empty() && inbox.entries.is_empty() {
                    ui.weak("Nothing received yet.");
                }
            }
            Err(e) if e.is_empty() => {
                ui.weak("…");
            }
            Err(e) => {
                ui.add(egui::Label::new(egui::RichText::new(e).weak()).wrap());
            }
        }
        if let Some((peer, id, accept)) = answer {
            self.plan(
                "spacedrop.answer",
                json!({"peer": peer, "id": id, "accept": accept}),
            );
        }
        if let Some(a) = act {
            self.act(0, a);
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            self.sources_ui(ui);
            ui.separator();
            self.devices_ui(ui);
            ui.separator();
            self.jobs_ui(ui);
        });
    }

    fn act(&mut self, pane: usize, a: Act) {
        match a {
            Act::Open(e) => {
                self.tab = Tab::Browse;
                self.screen.show(PhoneTab::Browse);
                self.open(pane, e.path);
            }
            Act::Select(e) => {
                self.active = pane;
                self.select(e);
                self.screen.open_preview();
            }
            Act::Download(path) => self.call("file.get", json!({ "path": path }), Want::Link),
            Act::Delete(path) => self.plan("plan", json!({"op": "delete", "paths": [path]})),
            Act::Send(e) => {
                let file = SharedFile {
                    name: e.name,
                    path: e.path,
                    size: e.size,
                };
                if self.share.send_one(file) {
                    self.call("devices.list", json!({}), Want::Devices);
                } else {
                    self.notice = Some("Finish or cancel the shared files first.".into());
                }
            }
        }
    }

    /// The long-press (or right-click) menu of an entry.
    fn entry_menu(r: &egui::Response, e: &EntryInfo, act: &mut Option<Act>) {
        r.context_menu(|ui| {
            if e.is_dir {
                if ui.button("Open").clicked() {
                    *act = Some(Act::Open(e.clone()));
                    ui.close_menu();
                }
            } else {
                if ui.button("Preview").clicked() {
                    *act = Some(Act::Select(e.clone()));
                    ui.close_menu();
                }
                if ui.button("Download").clicked() {
                    *act = Some(Act::Download(e.path.clone()));
                    ui.close_menu();
                }
            }
            if ui.button("Send to device…").clicked() {
                *act = Some(Act::Send(e.clone()));
                ui.close_menu();
            }
            if ui.button("Delete…").clicked() {
                *act = Some(Act::Delete(e.path.clone()));
                ui.close_menu();
            }
        });
    }

    fn path_bar(&mut self, ui: &mut egui::Ui, i: usize) {
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
        if let Some(path) = go.filter(|p| !p.is_empty()) {
            self.open(i, path);
        }
    }

    fn pane(&mut self, ui: &mut egui::Ui, i: usize) {
        self.path_bar(ui, i);
        let mut act = None;
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
                            act = Some(Act::Select(e.clone()));
                        }
                        if r.double_clicked() && e.is_dir {
                            act = Some(Act::Open(e.clone()));
                        }
                        Self::entry_menu(&r, e, &mut act);
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
        if let Some(a) = act {
            self.act(i, a);
        }
    }

    /// The phone's one pane: a list or a media grid, pull to refresh, long-press menus.
    fn phone_pane(&mut self, ui: &mut egui::Ui) {
        let i = self.active;
        self.path_bar(ui, i);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.grid, false, "List");
            ui.selectable_value(&mut self.grid, true, "Grid");
            if self.pull.progress() > 0.0 {
                ui.weak(if self.pull.progress() >= 1.0 {
                    "↑ release to refresh"
                } else {
                    "↓ pull to refresh"
                });
            }
        });
        let mut act = None;
        let mut want = Vec::new();
        let in_flight = self
            .wants
            .values()
            .filter(|w| matches!(w, Want::GridThumb { .. }))
            .count();
        let grid = self.grid;
        let tile = self.tile;
        let out = egui::ScrollArea::vertical()
            .id_salt(("phone-pane", i))
            .auto_shrink(false)
            .show(ui, |ui| {
                if grid {
                    ui.horizontal_wrapped(|ui| {
                        for e in &self.panes[i].entries {
                            let size = egui::vec2(tile, tile + 20.0);
                            let (rect, r) = ui.allocate_exact_size(size, egui::Sense::click());
                            if !ui.is_rect_visible(rect) {
                                continue;
                            }
                            let key = image_key(e);
                            let img = egui::Rect::from_min_size(rect.min, egui::vec2(tile, tile));
                            let painter = ui.painter();
                            let fill = if self.is_selected(&e.path) {
                                ui.visuals().selection.bg_fill
                            } else {
                                ui.visuals().faint_bg_color
                            };
                            painter.rect_filled(img.shrink(2.0), 6.0, fill);
                            match self.thumbs.get(&key) {
                                Some(t) => {
                                    let s = t.size_vec2();
                                    let fit = (img.width() - 4.0) / s.x.max(s.y);
                                    let r2 = egui::Rect::from_center_size(img.center(), s * fit);
                                    let uv = egui::Rect::from_min_max(
                                        egui::pos2(0.0, 0.0),
                                        egui::pos2(1.0, 1.0),
                                    );
                                    painter.image(t.id(), r2, uv, egui::Color32::WHITE);
                                }
                                None => {
                                    let glyph = if e.is_dir { "📁" } else { "📄" };
                                    painter.text(
                                        img.center(),
                                        egui::Align2::CENTER_CENTER,
                                        glyph,
                                        egui::FontId::proportional(tile / 3.0),
                                        ui.visuals().text_color(),
                                    );
                                    if !e.is_dir
                                        && is_media(&e.name)
                                        && !self.thumbs_asked.contains(&key)
                                        && in_flight + want.len() < THUMBS_IN_FLIGHT
                                    {
                                        want.push((e.path.clone(), key));
                                    }
                                }
                            }
                            let name: String = e.name.chars().take((tile / 8.0) as usize).collect();
                            painter.text(
                                egui::pos2(rect.center().x, rect.max.y - 10.0),
                                egui::Align2::CENTER_CENTER,
                                name,
                                egui::FontId::proportional(12.0),
                                ui.visuals().text_color(),
                            );
                            if r.clicked() {
                                act = Some(if e.is_dir {
                                    Act::Open(e.clone())
                                } else {
                                    Act::Select(e.clone())
                                });
                            }
                            Self::entry_menu(&r, e, &mut act);
                        }
                    });
                } else {
                    for e in &self.panes[i].entries {
                        let label = if e.is_dir {
                            format!("📁 {}", e.name)
                        } else {
                            format!("{}   {}", e.name, human(e.size))
                        };
                        let r = ui.add_sized(
                            [ui.available_width(), 44.0],
                            egui::SelectableLabel::new(self.is_selected(&e.path), label),
                        );
                        if r.clicked() {
                            act = Some(if e.is_dir {
                                Act::Open(e.clone())
                            } else {
                                Act::Select(e.clone())
                            });
                        }
                        Self::entry_menu(&r, e, &mut act);
                    }
                }
                if self.panes[i].truncated {
                    ui.weak("(more entries not shown)");
                }
            });
        for (path, key) in want {
            self.thumbs_asked.insert(key.clone());
            let params = json!({"path": path, "size": "thumb256"});
            self.call("media.thumb", params, Want::GridThumb { key });
        }
        // Pinch the grid to change the tile size.
        if self.grid && ui.rect_contains_pointer(out.inner_rect) {
            let z = ui.input(|i| i.zoom_delta());
            if z != 1.0 {
                self.tile = gesture::tile(self.tile, z);
            }
        }
        self.pull_to_refresh(ui, out.inner_rect, out.state.offset.y);
        if let Some(a) = act {
            self.act(i, a);
        }
    }

    fn pull_to_refresh(&mut self, ui: &egui::Ui, rect: egui::Rect, offset: f32) {
        let (down, dy) = ui.input(|i| (i.pointer.primary_down(), i.pointer.delta().y));
        let over = ui.rect_contains_pointer(rect);
        if self
            .pull
            .update_on(self.screen.phone(), offset <= 1.0, down && over, dy)
        {
            self.refresh();
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
        let phone = self.screen.phone();
        let mut act = None;
        let out = egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                for h in &self.hits {
                    let e = hit_entry(h);
                    ui.horizontal(|ui| {
                        let r = ui
                            .selectable_label(self.is_selected(&h.path), &h.name)
                            .on_hover_text(&h.path);
                        if r.clicked() {
                            act = Some(Act::Select(e.clone()));
                        }
                        if r.double_clicked() {
                            let path = if h.is_dir {
                                h.path.clone()
                            } else {
                                parent(&h.path).unwrap_or_default()
                            };
                            if !path.is_empty() {
                                act = Some(Act::Open(EntryInfo { path, ..e.clone() }));
                            }
                        }
                        Self::entry_menu(&r, &e, &mut act);
                        if !phone {
                            ui.weak(&h.source_label);
                        }
                    });
                }
            });
        if phone {
            let (rect, offset) = (out.inner_rect, out.state.offset.y);
            if self.pull.update(
                offset <= 1.0,
                ui.input(|i| i.pointer.primary_down()) && ui.rect_contains_pointer(rect),
                ui.input(|i| i.pointer.delta().y),
            ) && !self.query.trim().is_empty()
            {
                let q = self.query.clone();
                self.call("search", json!({"query": q, "max": 500}), Want::Search);
            }
        }
        if let Some(a) = act {
            let pane = self.active;
            self.act(pane, a);
        }
    }

    /// The files a swipe in the viewer moves between: the search hits on the phone's
    /// Search tab, else the active pane's files.
    fn neighbours(&self) -> Vec<EntryInfo> {
        if self.screen.phone() && self.screen.tab == PhoneTab::Search {
            self.hits
                .iter()
                .filter(|h| !h.is_dir)
                .map(hit_entry)
                .collect()
        } else {
            self.panes[self.active]
                .entries
                .iter()
                .filter(|e| !e.is_dir)
                .cloned()
                .collect()
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
        let mut act = None;
        ui.horizontal_wrapped(|ui| {
            if !e.is_dir && ui.button("Download").clicked() {
                act = Some(Act::Download(e.path.clone()));
            }
            if ui.button("Send to device…").clicked() {
                act = Some(Act::Send(e.clone()));
            }
            if ui.button("Delete…").clicked() {
                act = Some(Act::Delete(e.path.clone()));
            }
        });
        if let Some(a) = act {
            let pane = self.active;
            self.act(pane, a);
        }
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
        if self.screen.phone() {
            self.viewer(ui);
            return;
        }
        egui::ScrollArea::both()
            .auto_shrink(false)
            .show(ui, |ui| self.shown_ui(ui, 1.0));
    }

    fn shown_ui(&self, ui: &mut egui::Ui, zoom: f32) {
        match &self.shown {
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
                let scale = (ui.available_width() / size.x).min(1.0) * zoom;
                ui.image((t.id(), size * scale));
            }
            Shown::Message(m) => {
                ui.weak(m);
            }
        }
    }

    /// The phone's viewer: pinch to zoom (double-tap toggles), drag to pan when zoomed,
    /// swipe left or right for the next or previous file.
    fn viewer(&mut self, ui: &mut egui::Ui) {
        let zoom = self.zoom;
        let out = egui::ScrollArea::both()
            .id_salt("viewer")
            .auto_shrink(false)
            // Zoomed in, a drag pans; at 1x it is a swipe.
            .drag_to_scroll(zoom > 1.0)
            .show(ui, |ui| self.shown_ui(ui, zoom));
        let rect = out.inner_rect;
        let r = ui.interact(
            rect,
            ui.id().with("viewer-gestures"),
            egui::Sense::click_and_drag(),
        );
        if ui.rect_contains_pointer(rect) {
            let z = ui.input(|i| i.zoom_delta());
            if z != 1.0 {
                self.zoom = gesture::zoom(self.zoom, z);
            }
        }
        if r.double_clicked() {
            self.zoom = if self.zoom > 1.0 { 1.0 } else { 2.0 };
        }
        if r.drag_stopped() && zoom <= 1.0 {
            let total = ui.input(|i| {
                i.pointer
                    .press_origin()
                    .zip(i.pointer.interact_pos())
                    .map(|(a, b)| b - a)
            });
            if let Some(d) = total {
                let step = gesture::swipe(d.x, d.y);
                if step != 0 {
                    let files = self.neighbours();
                    let at = self
                        .selected
                        .as_ref()
                        .and_then(|s| files.iter().position(|f| f.path == s.path));
                    if let Some(at) = at {
                        let next = gesture::step(at, step, files.len());
                        if next != at {
                            self.select(files[next].clone());
                        }
                    }
                }
            }
        }
    }

    /// Send files to one paired device: claimed share-sheet files, or one file's "Send to
    /// device…". The user picks the device; `spacedrop.send` previews exactly those files to
    /// exactly that device, and the plan dialog executes it.
    fn share_window(&mut self, ctx: &egui::Context) {
        if !self.share.active() {
            return;
        }
        let mut close = false;
        let mut send = None;
        let mut pick = None;
        let mut open = false;
        let width = (ctx.screen_rect().width() - 24.0).min(420.0);
        egui::Window::new("Send with Spacedrop")
            .collapsible(false)
            .resizable(false)
            .max_width(width)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| match &self.share.state {
                share::State::None => {}
                share::State::Asking { files, bytes, .. } => {
                    let what = match (files, bytes) {
                        (Some(n), Some(b)) => format!("Open {n} shared file(s), {}?", human(*b)),
                        (Some(n), None) => format!("Open {n} shared file(s)?"),
                        _ => "Open the shared files?".to_owned(),
                    };
                    ui.strong(what);
                    ui.label("Only if you just shared them to Keel from this device.");
                    ui.horizontal(|ui| {
                        if ui.button("Open").clicked() {
                            open = true;
                        }
                        if ui.button("Dismiss").clicked() {
                            close = true;
                        }
                    });
                }
                share::State::Waiting(_) | share::State::Claiming(_) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Opening the shared files…");
                    });
                }
                share::State::Failed(why) => {
                    ui.colored_label(ui.visuals().error_fg_color, why);
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                }
                share::State::Claimed { files, peer } => {
                    let total = files.iter().fold(0u64, |n, f| n.saturating_add(f.size));
                    ui.strong(format!("{} file(s), {}", files.len(), human(total)));
                    egui::ScrollArea::vertical()
                        .max_height(160.0)
                        .show(ui, |ui| {
                            for f in files {
                                ui.label(format!("{} ({})", f.name, human(f.size)));
                            }
                        });
                    ui.separator();
                    ui.label("Send to:");
                    match &self.devices {
                        Ok(d) if d.peers.is_empty() => {
                            ui.weak("No paired devices: pair one in Keel first.");
                        }
                        Ok(d) => {
                            for p in &d.peers {
                                let on = peer.as_deref() == Some(p.id.as_str());
                                let text = format!("{} ({})", p.label, p.link);
                                if ui.selectable_label(on, text).clicked() {
                                    pick = Some(p.id.clone());
                                }
                            }
                        }
                        Err(e) if e.is_empty() => {
                            ui.spinner();
                        }
                        Err(e) => {
                            ui.add(egui::Label::new(egui::RichText::new(e).weak()).wrap());
                        }
                    }
                    ui.horizontal(|ui| {
                        let ready = self.share.send_params();
                        if ui
                            .add_enabled(ready.is_some(), egui::Button::new("Send…"))
                            .clicked()
                        {
                            send = ready;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
            });
        if open {
            self.share.accept();
            if self.conn.state == State::Online {
                if let Some((method, params)) = self.share.claim() {
                    self.call(method, params, Want::Claim);
                }
            }
        }
        if let Some(id) = pick {
            self.share.pick(&id);
        }
        if let Some(params) = send {
            self.plan("spacedrop.send", params);
        }
        if close {
            self.share.close();
        }
    }

    fn plan_window(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.plan else { return };
        let mut close = false;
        let mut execute = None;
        let width = (ctx.screen_rect().width() - 24.0).max(200.0);
        egui::Window::new("Preview")
            .collapsible(false)
            .resizable(true)
            .max_width(width)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let p = &dialog.preview;
                ui.add(egui::Label::new(egui::RichText::new(&p.summary).strong()).wrap());
                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .show(ui, |ui| {
                        for c in p.changes.iter().take(50) {
                            let to =
                                c.to.as_deref()
                                    .map(|t| format!(" -> {t}"))
                                    .unwrap_or_default();
                            let size = c
                                .bytes
                                .map(|b| format!(" ({})", human(b)))
                                .unwrap_or_default();
                            let detail = c
                                .detail
                                .as_deref()
                                .map(|d| format!(" {d}"))
                                .unwrap_or_default();
                            ui.label(format!(
                                "{} {}{to}{size}{detail}",
                                c.action,
                                c.path.as_deref().unwrap_or_default()
                            ));
                        }
                        if p.changes.len() > 50 {
                            ui.weak(format!("… {} more", p.changes.len() - 50));
                        }
                    });
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

    fn error_bar(&mut self, ctx: &egui::Context) {
        if let Some(e) = self.error.clone() {
            egui::TopBottomPanel::bottom("error").show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                    if ui.small_button("✕").clicked() {
                        self.error = None;
                    }
                });
            });
        }
    }

    fn desktop(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| self.top(ui));
        self.error_bar(ctx);
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
    }

    fn phone(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.screen.back();
        }
        egui::TopBottomPanel::top("phone-top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if self.screen.sheet {
                    if ui.button("← Back").clicked() {
                        self.screen.back();
                    }
                } else {
                    ui.strong("Keel");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    self.state_dot(ui);
                });
            });
            self.insecure_banner(ui);
        });
        egui::TopBottomPanel::bottom("phone-tabs").show(ctx, |ui| {
            ui.columns(PhoneTab::ALL.len(), |cols| {
                for (col, tab) in cols.iter_mut().zip(PhoneTab::ALL) {
                    let on = self.screen.tab == tab && !self.screen.sheet;
                    let b = egui::Button::new(tab.label()).selected(on);
                    if col.add_sized([col.available_width(), 48.0], b).clicked() {
                        self.screen.show(tab);
                    }
                }
            });
        });
        self.error_bar(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.screen.sheet {
                egui::ScrollArea::vertical()
                    .id_salt("sheet")
                    .auto_shrink(false)
                    .show(ui, |ui| self.preview(ui));
                return;
            }
            match self.screen.tab {
                PhoneTab::Browse => self.phone_pane(ui),
                PhoneTab::Search => self.search(ui),
                PhoneTab::Library => {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        if !self.library.is_empty() {
                            ui.label(format!("library {}", self.library));
                        }
                        self.sources_ui(ui);
                        ui.separator();
                        self.jobs_ui(ui);
                        ui.separator();
                        if ui.button("Sign out").clicked() {
                            self.sign_out();
                        }
                    });
                }
                PhoneTab::Devices => {
                    let out = egui::ScrollArea::vertical()
                        .auto_shrink(false)
                        .show(ui, |ui| self.devices_ui(ui));
                    let (rect, offset) = (out.inner_rect, out.state.offset.y);
                    if self.pull.update(
                        offset <= 1.0,
                        ui.input(|i| i.pointer.primary_down()) && ui.rect_contains_pointer(rect),
                        ui.input(|i| i.pointer.delta().y),
                    ) {
                        self.call("devices.list", json!({}), Want::Devices);
                        self.call("spacedrop.inbox", json!({}), Want::Inbox);
                    }
                }
            }
        });
    }
}

impl eframe::App for WebApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        if self.screen.resize(ctx.screen_rect().width()) {
            self.apply_style();
        }
        if matches!(self.conn.state, State::NeedToken | State::Refused(_)) {
            egui::CentralPanel::default().show(ctx, |ui| self.login(ui));
            return;
        }
        if self.screen.phone() {
            self.phone(ctx);
        } else {
            self.desktop(ctx);
        }
        self.share_window(ctx);
        self.plan_window(ctx);
    }
}
