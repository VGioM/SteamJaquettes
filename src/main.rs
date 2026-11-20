//! Steam Jaquettes : habille les jeux non-Steam de la bibliothèque Steam (couverture,
//! grande couverture, arrière-plan, logo, icône) avec les images de SteamGridDB.
//!
//! Les images vont dans `userdata/<compte>/config/grid/`, là où Steam range les
//! illustrations personnalisées ; l'icône est en plus écrite dans
//! `shortcuts.vdf`, ce qui demande que Steam soit fermé (l'app le redémarre).

use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Sense, Stroke};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

mod desktop;
mod selfupdate;
mod sgdb;
mod steam;
mod vdf;

use selfupdate::SelfUpdate;
use sgdb::{Img, SgdbGame};
use steam::{Account, Game, Kind};

const BG: Color32 = Color32::from_rgb(0x15, 0x17, 0x1C);
const CARD: Color32 = Color32::from_rgb(0x1F, 0x22, 0x29);
const CARD2: Color32 = Color32::from_rgb(0x2A, 0x2E, 0x37);
const HOVER: Color32 = Color32::from_rgb(0x33, 0x38, 0x43);
const LINE: Color32 = Color32::from_rgb(0x33, 0x37, 0x40);
const TEXT: Color32 = Color32::from_rgb(0xF1, 0xF3, 0xF6);
const MUTED: Color32 = Color32::from_rgb(0x9C, 0xA3, 0xAF);
const ACCENT: Color32 = Color32::from_rgb(0x5B, 0x9D, 0xFF);
const ACCENT_DIM: Color32 = Color32::from_rgb(0x22, 0x3A, 0x5E);
const ON_ACCENT: Color32 = Color32::from_rgb(0x0A, 0x14, 0x24);
const OK: Color32 = Color32::from_rgb(0x4C, 0xC2, 0x6B);
const WARN: Color32 = Color32::from_rgb(0xFF, 0xB0, 0x4D);
const BAD: Color32 = Color32::from_rgb(0xFF, 0x6B, 0x6B);

// ---------------------------------------------------------------- réglages

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct Match { id: u64, name: String }

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Config {
    api_key: String,
    account: String,
    /// Jeu SteamGridDB choisi pour chaque raccourci (« compte:appid »).
    matches: HashMap<String, Match>,
    /// Icônes posées pendant que Steam était ouvert, à écrire dans shortcuts.vdf.
    pending_icons: HashMap<String, Vec<(u32, String)>>,
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let dir = base.join("SteamJaquettes");
    // Réglages de l'époque où l'app s'appelait Jaquettes (1.0.0 et 1.0.1).
    let old = base.join("Jaquettes");
    if !dir.exists() && old.is_dir() { let _ = std::fs::rename(&old, &dir); }
    Some(dir.join("config.json"))
}

impl Config {
    fn load() -> Self {
        config_path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }
    fn save(&self) {
        let Some(p) = config_path() else { return };
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default()).is_ok() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

fn mkey(account: &str, appid: u32) -> String { format!("{account}:{appid}") }

// ---------------------------------------------------------------- images (textures)

#[derive(Clone)]
enum Source { Url(String), File(PathBuf) }

enum Slot { Loading, Ready(egui::TextureHandle), Failed }

type Jobs = Arc<(Mutex<VecDeque<(String, Source, u32)>>, Condvar)>;

/// Charge les images (vignettes SteamGridDB, fichiers de grid/) en arrière-plan,
/// les réduit, et en fait des textures.
struct Textures { jobs: Jobs, rx: mpsc::Receiver<(String, Option<egui::ColorImage>)>, slots: HashMap<String, Slot> }

impl Textures {
    fn new(ctx: &egui::Context) -> Self {
        let jobs: Jobs = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let (tx, rx) = mpsc::channel();
        for _ in 0..6 {
            let (jobs, tx, ctx) = (jobs.clone(), tx.clone(), ctx.clone());
            std::thread::spawn(move || loop {
                let (key, src, max) = {
                    let (lock, cv) = &*jobs;
                    let mut q = lock.lock().unwrap();
                    loop {
                        // Les dernières demandées d'abord : ce qui est à l'écran maintenant.
                        if let Some(j) = q.pop_back() { break j; }
                        q = cv.wait(q).unwrap();
                    }
                };
                let bytes = match &src { Source::Url(u) => sgdb::download(u).ok(), Source::File(p) => std::fs::read(p).ok() };
                let img = bytes.and_then(|b| decode(&b, max));
                if tx.send((key, img)).is_err() { return; }
                ctx.request_repaint();
            });
        }
        Textures { jobs, rx, slots: HashMap::new() }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok((key, img)) = self.rx.try_recv() {
            let slot = match img {
                Some(img) => Slot::Ready(ctx.load_texture(&key, img, egui::TextureOptions::LINEAR)),
                None => Slot::Failed,
            };
            self.slots.insert(key, slot);
        }
    }

    fn get(&mut self, key: String, src: Source, max: u32) -> Option<&egui::TextureHandle> {
        if !self.slots.contains_key(&key) {
            self.slots.insert(key.clone(), Slot::Loading);
            let (lock, cv) = &*self.jobs;
            lock.lock().unwrap().push_back((key.clone(), src, max));
            cv.notify_one();
        }
        match self.slots.get(&key) { Some(Slot::Ready(t)) => Some(t), _ => None }
    }

    fn failed(&self, key: &str) -> bool { matches!(self.slots.get(key), Some(Slot::Failed)) }

    fn url(&mut self, url: &str) -> Option<&egui::TextureHandle> { self.get(format!("url:{url}"), Source::Url(url.to_string()), 480) }

    fn file(&mut self, path: &Path, max: u32) -> Option<&egui::TextureHandle> {
        let meta = std::fs::metadata(path).ok();
        let stamp = meta.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_millis()).unwrap_or(0);
        let size = meta.map(|m| m.len()).unwrap_or(0);
        self.get(format!("file:{}:{stamp}:{size}:{max}", path.display()), Source::File(path.to_path_buf()), max)
    }
}

fn decode(bytes: &[u8], max: u32) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width() > max || img.height() > max { img.thumbnail(max, max) } else { img };
    let rgba = img.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw()))
}

/// Dessine une texture dans `rect` : en remplissant (rognée) ou en entier (centrée).
fn paint_tex(ui: &egui::Ui, rect: egui::Rect, tex: &egui::TextureHandle, fill: bool, radius: u8) {
    let [w, h] = tex.size();
    let (ta, ra) = (w as f32 / h.max(1) as f32, rect.width() / rect.height().max(1.0));
    if fill {
        let uv = if ta > ra { let u = ra / ta; egui::Rect::from_min_max(egui::pos2(0.5 - u / 2.0, 0.0), egui::pos2(0.5 + u / 2.0, 1.0)) }
                 else { let v = ta / ra; egui::Rect::from_min_max(egui::pos2(0.0, 0.5 - v / 2.0), egui::pos2(1.0, 0.5 + v / 2.0)) };
        egui::Image::new((tex.id(), rect.size())).uv(uv).corner_radius(radius).paint_at(ui, rect);
    } else {
        let size = if ta > ra { egui::vec2(rect.width(), rect.width() / ta) } else { egui::vec2(rect.height() * ta, rect.height()) };
        let r = egui::Rect::from_center_size(rect.center(), size);
        egui::Image::new((tex.id(), size)).corner_radius(radius).paint_at(ui, r);
    }
}

// ---------------------------------------------------------------- tâches

enum Msg {
    Search { appid: u32, term: String, result: Result<Vec<SgdbGame>, String>, pick: bool },
    List { game: u64, kind: Kind, page: u32, result: Result<Vec<Img>, String> },
    Applied { appid: u32, kind: Kind, result: Result<PathBuf, String>, auto: bool },
    Matched { appid: u32, game: SgdbGame },
    AutoProgress { done: usize, total: usize, name: String },
    AutoDone { filled: usize, errors: Vec<String>, cancelled: bool },
    Restarted(Result<(), String>),
}

#[derive(Default)]
struct List { items: Vec<Img>, loading: bool, error: Option<String>, next_page: u32, more: bool }

enum SearchState { Loading, Done(Vec<SgdbGame>), Failed(String) }

struct AutoJob { appid: u32, name: String, term: String, matched: Option<u64>, missing: Vec<Kind> }

struct Auto { done: usize, total: usize, current: String, cancel: Arc<AtomicBool> }

/// Remplit les images manquantes avec les mieux notées de SteamGridDB.
fn auto_fill(key: String, account: Account, jobs: Vec<AutoJob>, tx: mpsc::Sender<Msg>, ctx: egui::Context, cancel: Arc<AtomicBool>) {
    let send = |m: Msg| { let _ = tx.send(m); ctx.request_repaint(); };
    let (total, mut filled, mut errors) = (jobs.len(), 0, Vec::new());
    for (i, job) in jobs.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) { send(Msg::AutoDone { filled, errors, cancelled: true }); return; }
        send(Msg::AutoProgress { done: i, total, name: job.name.clone() });
        let game = match job.matched {
            Some(id) => id,
            None => match sgdb::search(&key, &job.term) {
                Ok(list) if !list.is_empty() => { let g = list[0].clone(); let id = g.id; send(Msg::Matched { appid: job.appid, game: g }); id }
                Ok(_) => { errors.push(format!("{} : introuvable sur SteamGridDB (essaie un autre nom)", job.name)); continue; }
                Err(e) => { errors.push(format!("{} : {e}", job.name)); if e.contains("Clé API") { break; } continue; }
            },
        };
        for kind in job.missing {
            if cancel.load(Ordering::Relaxed) { break; }
            let res = sgdb::images(&key, game, kind, 0).and_then(|l| l.into_iter().next().ok_or_else(|| "aucune image".to_string()))
                .and_then(|img| sgdb::download(&img.url)).and_then(|b| steam::save_art(&account, job.appid, kind, &b));
            match res {
                Ok(p) => { filled += 1; send(Msg::Applied { appid: job.appid, kind, result: Ok(p), auto: true }); }
                Err(e) if e == "aucune image" => {}
                Err(e) => errors.push(format!("{} ({}) : {e}", job.name, kind.label().to_lowercase())),
            }
        }
    }
    send(Msg::AutoDone { filled, errors, cancelled: cancel.load(Ordering::Relaxed) });
}

// ---------------------------------------------------------------- app

struct App {
    cfg: Config,
    accounts: Vec<Account>,
    account: usize,
    games: Vec<Game>,
    games_stamp: Option<SystemTime>,
    load_error: Option<String>,
    art: HashMap<u32, [Option<PathBuf>; 5]>,
    selected: Option<u32>,
    tab: Kind,
    tex: Textures,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    search_text: HashMap<u32, String>,
    searches: HashMap<u32, SearchState>,
    show_results: bool,
    lists: HashMap<(u64, Kind), List>,
    applying: HashSet<(u32, Kind)>,
    auto: Option<Auto>,
    restarting: bool,
    steam_running: bool,
    last_poll: Instant,
    changed: bool,
    status: Option<(String, bool, Instant)>,
    settings: bool,
    key_input: String,
    update: SelfUpdate,
    hook: TestHook,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        style(&cc.egui_ctx);
        let cfg = Config::load();
        let accounts = steam::accounts();
        let account = accounts.iter().position(|a| a.id == cfg.account).unwrap_or(0);
        let (tx, rx) = mpsc::channel();
        let key_input = cfg.api_key.clone();
        let mut app = App {
            cfg, accounts, account, games: vec![], games_stamp: None, load_error: None, art: HashMap::new(), selected: None, tab: Kind::Cover,
            tex: Textures::new(&cc.egui_ctx), tx, rx, search_text: HashMap::new(), searches: HashMap::new(), show_results: false,
            lists: HashMap::new(), applying: HashSet::new(), auto: None, restarting: false, steam_running: steam::running(),
            last_poll: Instant::now(), changed: false, status: None, settings: false, key_input, update: SelfUpdate::new(), hook: TestHook::default(),
        };
        app.load_games();
        app.selected = app.games.first().map(|g| g.appid);
        if let Some(t) = &app.hook.select {
            let t = t.to_lowercase();
            app.selected = app.games.iter().find(|g| g.name.to_lowercase().contains(&t)).map(|g| g.appid).or(app.selected);
        }
        if let Some(k) = app.hook.tab { app.tab = k; }
        // Icônes en attente d'une session précédente : Steam est peut-être fermé maintenant.
        if !app.steam_running && !app.pending().is_empty() { app.write_pending_icons(); }
        app
    }

    fn acc(&self) -> Option<&Account> { self.accounts.get(self.account) }

    fn pending(&self) -> Vec<(u32, String)> {
        self.acc().and_then(|a| self.cfg.pending_icons.get(&a.id)).cloned().unwrap_or_default()
    }

    fn shortcuts_stamp(&self) -> Option<SystemTime> {
        self.acc().and_then(|a| std::fs::metadata(a.shortcuts()).ok()).and_then(|m| m.modified().ok())
    }

    fn load_games(&mut self) {
        let Some(a) = self.acc().cloned() else { self.games.clear(); return };
        self.games_stamp = self.shortcuts_stamp();
        match steam::read_games(&a) {
            Ok(mut g) => {
                if !self.hook.hide.is_empty() { g.retain(|g| !self.hook.hide.iter().any(|h| g.name.to_lowercase().contains(h))); }
                self.games = g;
                self.load_error = None;
            }
            Err(e) => { self.games.clear(); self.load_error = Some(e); }
        }
        self.refresh_art();
        if self.selected.is_some_and(|s| !self.games.iter().any(|g| g.appid == s)) { self.selected = self.games.first().map(|g| g.appid); }
    }

    fn refresh_art(&mut self) {
        let Some(a) = self.acc().cloned() else { return };
        let pending = self.pending();
        self.art = self.games.iter().map(|g| {
            let files = Kind::ALL.map(|k| {
                if k == Kind::Icon { if let Some((_, p)) = pending.iter().find(|(id, _)| *id == g.appid) { return Some(PathBuf::from(p)); } }
                steam::art_file(&a, g, k)
            });
            (g.appid, files)
        }).collect();
    }

    fn art_of(&self, appid: u32, kind: Kind) -> Option<&PathBuf> {
        self.art.get(&appid).and_then(|a| a[Kind::ALL.iter().position(|k| *k == kind).unwrap()].as_ref())
    }

    fn missing(&self, appid: u32) -> Vec<Kind> { Kind::ALL.into_iter().filter(|k| self.art_of(appid, *k).is_none()).collect() }

    fn matched(&self, appid: u32) -> Option<&Match> { self.acc().and_then(|a| self.cfg.matches.get(&mkey(&a.id, appid))) }

    fn set_match(&mut self, appid: u32, g: &SgdbGame) {
        let Some(a) = self.acc() else { return };
        let k = mkey(&a.id, appid);
        self.cfg.matches.insert(k, Match { id: g.id, name: g.name.clone() });
        self.cfg.save();
    }

    fn status(&mut self, text: impl Into<String>, error: bool) { self.status = Some((text.into(), error, Instant::now())); }

    fn spawn<F: FnOnce() -> Msg + Send + 'static>(&self, ctx: &egui::Context, f: F) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || { let _ = tx.send(f()); ctx.request_repaint(); });
    }

    fn search(&mut self, ctx: &egui::Context, appid: u32, term: String, pick: bool) {
        self.searches.insert(appid, SearchState::Loading);
        let key = self.cfg.api_key.clone();
        self.spawn(ctx, move || { let result = sgdb::search(&key, &term); Msg::Search { appid, term, result, pick } });
    }

    fn load_list(&mut self, ctx: &egui::Context, game: u64, kind: Kind) {
        let l = self.lists.entry((game, kind)).or_default();
        if l.loading { return; }
        l.loading = true;
        l.error = None;
        let (key, page) = (self.cfg.api_key.clone(), l.next_page);
        self.spawn(ctx, move || Msg::List { game, kind, page, result: sgdb::images(&key, game, kind, page) });
    }

    fn apply(&mut self, ctx: &egui::Context, appid: u32, kind: Kind, img: &Img) {
        let Some(a) = self.acc().cloned() else { return };
        if !self.applying.insert((appid, kind)) { return; }
        let url = img.url.clone();
        self.spawn(ctx, move || {
            let result = sgdb::download(&url).and_then(|b| steam::save_art(&a, appid, kind, &b));
            Msg::Applied { appid, kind, result, auto: false }
        });
    }

    fn remove(&mut self, appid: u32, kind: Kind) {
        let Some(a) = self.acc().cloned() else { return };
        if kind == Kind::Icon {
            self.set_icon(appid, String::new());
            steam::remove_art(&a, appid, kind);
        } else {
            steam::remove_art(&a, appid, kind);
            self.changed = true;
        }
        self.refresh_art();
    }

    /// Note le nouveau chemin d'icône, et l'écrit tout de suite si Steam est fermé.
    fn set_icon(&mut self, appid: u32, path: String) {
        let Some(id) = self.acc().map(|a| a.id.clone()) else { return };
        let list = self.cfg.pending_icons.entry(id).or_default();
        list.retain(|(a, _)| *a != appid);
        list.push((appid, path));
        self.cfg.save();
        self.changed = true;
        if !steam::running() { self.write_pending_icons(); }
    }

    fn write_pending_icons(&mut self) {
        let Some(a) = self.acc().cloned() else { return };
        let pending = self.pending();
        if pending.is_empty() { return; }
        match steam::write_icons(&a, &pending) {
            Ok(()) => { self.cfg.pending_icons.remove(&a.id); self.cfg.save(); self.load_games(); }
            Err(e) => self.status(format!("Icônes non enregistrées : {e}"), true),
        }
    }

    fn restart_steam(&mut self, ctx: &egui::Context) {
        let Some(a) = self.acc().cloned() else { return };
        self.restarting = true;
        let pending = self.pending();
        self.spawn(ctx, move || {
            let r = steam::shutdown(a.flatpak).and_then(|_| {
                let w = if pending.is_empty() { Ok(()) } else { steam::write_icons(&a, &pending) };
                let s = steam::start(a.flatpak);
                w.and(s)
            });
            Msg::Restarted(r)
        });
    }

    fn start_auto(&mut self, ctx: &egui::Context, only: Option<u32>) {
        let Some(a) = self.acc().cloned() else { return };
        let jobs: Vec<AutoJob> = self.games.iter().filter(|g| only.is_none_or(|o| o == g.appid)).filter_map(|g| {
            let missing = self.missing(g.appid);
            if missing.is_empty() { return None; }
            let term = self.search_text.get(&g.appid).cloned().unwrap_or_else(|| sgdb::search_term(&g.name));
            Some(AutoJob { appid: g.appid, name: g.name.clone(), term, matched: self.matched(g.appid).map(|m| m.id), missing })
        }).collect();
        if jobs.is_empty() { self.status(if only.is_some() { "Ce jeu a déjà toutes ses images." } else { "Tous les jeux ont déjà toutes leurs images." }, false); return; }
        let cancel = Arc::new(AtomicBool::new(false));
        self.auto = Some(Auto { done: 0, total: jobs.len(), current: String::new(), cancel: cancel.clone() });
        let (key, tx, ctx) = (self.cfg.api_key.clone(), self.tx.clone(), ctx.clone());
        std::thread::spawn(move || auto_fill(key, a, jobs, tx, ctx, cancel));
    }

    fn handle(&mut self, ctx: &egui::Context, m: Msg) {
        match m {
            Msg::Search { appid, term, result, pick } => {
                // Une recherche plus récente a pu être lancée entre-temps.
                if self.search_text.get(&appid).is_some_and(|t| t.trim() != term.trim()) && !pick { return; }
                match result {
                    Ok(list) => {
                        if pick && self.matched(appid).is_none() { if let Some(g) = list.first() { let g = g.clone(); self.set_match(appid, &g); } }
                        if !pick { self.show_results = true; }
                        self.searches.insert(appid, SearchState::Done(list));
                    }
                    Err(e) => { self.searches.insert(appid, SearchState::Failed(e)); }
                }
            }
            Msg::List { game, kind, page, result } => {
                let l = self.lists.entry((game, kind)).or_default();
                l.loading = false;
                match result {
                    Ok(items) => {
                        l.more = items.len() >= 50;
                        l.next_page = page + 1;
                        for i in items { if !l.items.iter().any(|x| x.id == i.id) { l.items.push(i); } }
                    }
                    Err(e) => l.error = Some(e),
                }
            }
            Msg::Applied { appid, kind, result, auto } => {
                self.applying.remove(&(appid, kind));
                match result {
                    Ok(path) => {
                        if kind == Kind::Icon { self.set_icon(appid, path.to_string_lossy().into_owned()); } else { self.changed = true; }
                        self.refresh_art();
                    }
                    Err(e) => if !auto { self.status(e, true) },
                }
            }
            Msg::Matched { appid, game } => self.set_match(appid, &game),
            Msg::AutoProgress { done, total, name } => if let Some(a) = &mut self.auto { a.done = done; a.total = total; a.current = name; },
            Msg::AutoDone { filled, errors, cancelled } => {
                self.auto = None;
                self.refresh_art();
                let mut text = match filled { 0 => "Aucune image ajoutée.".to_string(), 1 => "1 image ajoutée.".into(), n => format!("{n} images ajoutées.") };
                if cancelled { text = format!("Arrêté. {text}"); }
                if !errors.is_empty() { text = format!("{text}\n{}", errors.join("\n")); }
                self.status(text, !errors.is_empty());
            }
            Msg::Restarted(r) => {
                self.restarting = false;
                match r {
                    Ok(()) => {
                        if let Some(id) = self.acc().map(|a| a.id.clone()) { self.cfg.pending_icons.remove(&id); self.cfg.save(); }
                        self.changed = false;
                        self.load_games();
                        self.status("Steam redémarre avec les nouvelles images.", false);
                    }
                    Err(e) => self.status(e, true),
                }
            }
        }
        let _ = ctx;
    }

    fn poll(&mut self) {
        if self.last_poll.elapsed() < Duration::from_secs(2) { return; }
        self.last_poll = Instant::now();
        self.steam_running = steam::running();
        // Jeu ajouté ou retiré dans Steam pendant que l'app est ouverte.
        if self.auto.is_none() && !self.restarting && self.shortcuts_stamp() != self.games_stamp { self.load_games(); }
    }
}

// ---------------------------------------------------------------- interface

fn style(ctx: &egui::Context) {
    let mut s = (*ctx.style()).clone();
    s.visuals = egui::Visuals::dark();
    let v = &mut s.visuals;
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = Color32::from_rgb(0x10, 0x12, 0x16);
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT_DIM;
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    v.hyperlink_color = ACCENT;
    v.clip_rect_margin = 0.0;
    v.widgets.inactive.bg_fill = CARD2;
    v.widgets.inactive.weak_bg_fill = CARD2;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, LINE);
    v.widgets.inactive.corner_radius = CornerRadius::same(8);
    v.widgets.hovered.weak_bg_fill = HOVER;
    v.widgets.hovered.bg_fill = HOVER;
    v.widgets.hovered.corner_radius = CornerRadius::same(8);
    v.widgets.active.corner_radius = CornerRadius::same(8);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, LINE);
    s.spacing.item_spacing = egui::vec2(10.0, 8.0);
    s.spacing.button_padding = egui::vec2(12.0, 6.0);
    s.spacing.interact_size.y = 26.0;
    for (ts, size) in [(egui::TextStyle::Body, 14.5), (egui::TextStyle::Button, 14.5), (egui::TextStyle::Small, 12.0), (egui::TextStyle::Heading, 20.0)] {
        if let Some(f) = s.text_styles.get_mut(&ts) { f.size = size; }
    }
    ctx.set_style(s);
}

fn small(ui: &mut egui::Ui, text: impl Into<String>) { ui.label(RichText::new(text).size(12.5).color(MUTED)); }

fn button(ui: &mut egui::Ui, enabled: bool, text: &str, primary: bool) -> bool {
    let rt = RichText::new(text).strong().color(if primary { ON_ACCENT } else { TEXT });
    let mut b = egui::Button::new(rt).min_size(egui::vec2(0.0, 32.0)).corner_radius(CornerRadius::same(9));
    if primary { b = b.fill(ACCENT); }
    ui.add_enabled(enabled, b).clicked()
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new().fill(CARD).stroke(Stroke::new(1.0_f32, LINE)).corner_radius(CornerRadius::same(12)).inner_margin(Margin::same(14))
        .show(ui, |ui| { ui.set_width(ui.available_width()); add(ui) }).inner
}

/// Une case vide (image manquante).
fn placeholder(ui: &egui::Ui, rect: egui::Rect, text: &str) {
    ui.painter().rect_filled(rect, 8, CARD2);
    ui.painter().rect_stroke(rect, 8, Stroke::new(1.0_f32, LINE), egui::StrokeKind::Inside);
    if !text.is_empty() {
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(12.0), MUTED);
    }
}

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Steam Jaquettes").size(22.0).strong());
            small(ui, "images des jeux non-Steam");
            if self.accounts.len() > 1 && !self.hook.demo {
                ui.add_space(8.0);
                let mut idx = self.account;
                let label = |a: &Account| if a.games > 0 { format!("{} ({} jeux)", a.name, a.games) } else { format!("{} (aucun jeu non-Steam)", a.name) };
                egui::ComboBox::from_id_salt("account").selected_text(label(&self.accounts[idx])).width(240.0).show_ui(ui, |ui| {
                    for (i, a) in self.accounts.iter().enumerate() { ui.selectable_value(&mut idx, i, label(a)); }
                });
                if idx != self.account {
                    self.account = idx;
                    self.cfg.account = self.accounts[idx].id.clone();
                    self.cfg.save();
                    self.selected = None;
                    self.load_games();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(egui::Button::new("Réglages").corner_radius(CornerRadius::same(9)).min_size(egui::vec2(0.0, 32.0))).clicked() {
                    self.key_input = self.cfg.api_key.clone();
                    self.settings = true;
                }
                let ready = !self.cfg.api_key.is_empty() && self.auto.is_none() && !self.games.is_empty();
                if button(ui, ready, "Remplir tout ce qui manque", true) { self.start_auto(ui.ctx(), None); }
            });
        });
    }

    fn banners(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if let Some(a) = &self.auto {
            let (done, total, current, cancel) = (a.done, a.total, a.current.clone(), a.cancel.clone());
            egui::Frame::new().fill(ACCENT_DIM).corner_radius(10).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(16.0).color(ACCENT));
                    ui.label(format!("Remplissage automatique : {current} ({}/{total})", done + 1));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Arrêter").clicked() { cancel.store(true, Ordering::Relaxed); }
                        ui.add(egui::ProgressBar::new(done as f32 / total.max(1) as f32).desired_width(180.0));
                    });
                });
            });
            ui.add_space(6.0);
        }
        let pending = self.pending().len();
        if self.restarting || (self.steam_running && (self.changed || pending > 0)) {
            egui::Frame::new().fill(Color32::from_rgb(0x3A, 0x2E, 0x18)).corner_radius(10).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    if self.restarting {
                        ui.add(egui::Spinner::new().size(16.0).color(WARN));
                        ui.label("Redémarrage de Steam…");
                    } else {
                        let text = if pending > 0 {
                            format!("Steam est ouvert : {} attendent sa fermeture, et les nouvelles images s'afficheront après un redémarrage.", if pending == 1 { "1 icône".to_string() } else { format!("{pending} icônes") })
                        } else { "Steam est ouvert : redémarre-le pour voir les nouvelles images.".to_string() };
                        ui.add(egui::Label::new(RichText::new(text).color(WARN)).wrap());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if button(ui, self.auto.is_none(), "Redémarrer Steam", false) { self.restart_steam(&ctx); }
                        });
                    }
                });
            });
            ui.add_space(6.0);
        }
        if let Some((text, err, at)) = &self.status {
            if at.elapsed() > Duration::from_secs(if *err { 30 } else { 6 }) { self.status = None; return; }
            let (text, err) = (text.clone(), *err);
            let mut close = false;
            egui::Frame::new().fill(if err { Color32::from_rgb(0x3D, 0x1F, 0x22) } else { Color32::from_rgb(0x1B, 0x35, 0x24) }).corner_radius(10).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.add(egui::Label::new(RichText::new(text).color(if err { BAD } else { OK })).wrap());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| { if ui.small_button("✕").clicked() { close = true; } });
                });
            });
            if close { self.status = None; }
            ui.add_space(6.0);
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    fn game_list(&mut self, ui: &mut egui::Ui) {
        let complete = self.games.iter().filter(|g| self.missing(g.appid).is_empty()).count();
        ui.horizontal(|ui| {
            small(ui, match self.games.len() { 0 => "Aucun jeu non-Steam".to_string(), 1 => "1 jeu non-Steam".into(), n => format!("{n} jeux non-Steam") });
            if !self.games.is_empty() { small(ui, format!("· {complete} complet{}", if complete > 1 { "s" } else { "" })); }
        });
        ui.add_space(4.0);
        let mut clicked = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let games = self.games.clone();
            for g in &games {
                let selected = self.selected == Some(g.appid);
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 66.0), Sense::click());
                let fill = if selected { ACCENT_DIM } else if resp.hovered() { CARD } else { Color32::TRANSPARENT };
                ui.painter().rect_filled(rect, 10, fill);
                let thumb = egui::Rect::from_min_size(rect.min + egui::vec2(8.0, 5.0), egui::vec2(37.0, 56.0));
                match self.art_of(g.appid, Kind::Cover).cloned() {
                    Some(p) => match self.tex.file(&p, 160) { Some(t) => paint_tex(ui, thumb, t, true, 4), None => placeholder(ui, thumb, "") },
                    None => placeholder(ui, thumb, "?"),
                }
                let x = thumb.max.x + 10.0;
                let name_rect = egui::Rect::from_min_max(egui::pos2(x, rect.min.y + 9.0), egui::pos2(rect.max.x - 6.0, rect.min.y + 30.0));
                let galley = ui.painter().layout(g.name.clone(), egui::FontId::proportional(14.5), TEXT, f32::INFINITY);
                let painter = ui.painter().with_clip_rect(name_rect);
                painter.galley(name_rect.min, galley, TEXT);
                let mut cx = x;
                for k in Kind::ALL {
                    let has = self.art_of(g.appid, k).is_some();
                    let busy = self.applying.contains(&(g.appid, k));
                    let txt = k.short();
                    let w = 10.0 + 7.5 * txt.len() as f32;
                    let chip = egui::Rect::from_min_size(egui::pos2(cx, rect.min.y + 36.0), egui::vec2(w, 18.0));
                    let (bg, fg) = if busy { (ACCENT_DIM, ACCENT) } else if has { (Color32::from_rgb(0x1E, 0x3B, 0x28), OK) } else { (CARD2, MUTED) };
                    ui.painter().rect_filled(chip, 5, bg);
                    ui.painter().text(chip.center(), egui::Align2::CENTER_CENTER, txt, egui::FontId::proportional(11.0), fg);
                    cx += w + 4.0;
                }
                let resp = resp.on_hover_text(Kind::ALL.iter().map(|k| format!("{} : {}", k.label(), if self.art_of(g.appid, *k).is_some() { "oui" } else { "manquante" })).collect::<Vec<_>>().join("\n"));
                if resp.clicked() { clicked = Some(g.appid); }
            }
        });
        if let Some(c) = clicked { if self.selected != Some(c) { self.selected = Some(c); self.show_results = false; } }
    }

    /// Aperçu façon bibliothèque Steam : couverture, arrière-plan + logo, grande couverture, icône.
    fn preview(&mut self, ui: &mut egui::Ui, g: &Game) {
        let w = ui.available_width();
        let h = (w * 0.27).clamp(170.0, 250.0);
        let cover_w = h * 2.0 / 3.0;
        let mut go = None;
        ui.horizontal(|ui| {
            // Couverture.
            let (r, resp) = ui.allocate_exact_size(egui::vec2(cover_w, h), Sense::click());
            match self.art_of(g.appid, Kind::Cover).cloned() {
                Some(p) => match self.tex.file(&p, 600) { Some(t) => paint_tex(ui, r, t, true, 8), None => placeholder(ui, r, "") },
                None => placeholder(ui, r, "Couverture\nmanquante"),
            }
            if resp.on_hover_text("Couverture").clicked() { go = Some(Kind::Cover); }
            // Arrière-plan et logo.
            let bw = ui.available_width();
            let (r, resp) = ui.allocate_exact_size(egui::vec2(bw, h), Sense::click());
            match self.art_of(g.appid, Kind::Hero).cloned() {
                Some(p) => match self.tex.file(&p, 1600) { Some(t) => paint_tex(ui, r, t, true, 8), None => placeholder(ui, r, "") },
                None => placeholder(ui, r, ""),
            }
            let logo_r = egui::Rect::from_min_max(r.min + egui::vec2(r.width() * 0.05, r.height() * 0.18), egui::pos2(r.min.x + r.width() * 0.45, r.max.y - r.height() * 0.18));
            match self.art_of(g.appid, Kind::Logo).cloned() {
                Some(p) => if let Some(t) = self.tex.file(&p, 900) { paint_tex(ui, logo_r, t, false, 0) },
                None => { ui.painter().text(logo_r.left_center(), egui::Align2::LEFT_CENTER, &g.name, egui::FontId::proportional(24.0), TEXT); }
            }
            if self.art_of(g.appid, Kind::Hero).is_none() {
                ui.painter().text(r.right_bottom() - egui::vec2(10.0, 10.0), egui::Align2::RIGHT_BOTTOM, "Arrière-plan manquant", egui::FontId::proportional(12.0), MUTED);
            }
            if resp.on_hover_text("Arrière-plan et logo").clicked() { go = Some(if self.art_of(g.appid, Kind::Hero).is_none() { Kind::Hero } else { Kind::Logo }); }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let (r, resp) = ui.allocate_exact_size(egui::vec2(150.0, 70.0), Sense::click());
            match self.art_of(g.appid, Kind::Wide).cloned() {
                Some(p) => match self.tex.file(&p, 480) { Some(t) => paint_tex(ui, r, t, true, 6), None => placeholder(ui, r, "") },
                None => placeholder(ui, r, "Grande couverture\nmanquante"),
            }
            if resp.on_hover_text("Grande couverture").clicked() { go = Some(Kind::Wide); }
            let (r, resp) = ui.allocate_exact_size(egui::vec2(70.0, 70.0), Sense::click());
            match self.art_of(g.appid, Kind::Icon).cloned() {
                Some(p) => { placeholder(ui, r, ""); if let Some(t) = self.tex.file(&p, 256) { paint_tex(ui, r.shrink(8.0), t, false, 0) } }
                None => placeholder(ui, r, "Icône\nmanquante"),
            }
            if resp.on_hover_text("Icône").clicked() { go = Some(Kind::Icon); }
            ui.vertical(|ui| {
                ui.label(RichText::new(&g.name).size(18.0).strong());
                let exe = Path::new(g.exe.trim_matches('"')).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                small(ui, format!("{exe} · identifiant {}", g.appid));
            });
        });
        if let Some(k) = go { self.tab = k; }
    }

    fn match_row(&mut self, ui: &mut egui::Ui, g: &Game) {
        let ctx = ui.ctx().clone();
        let appid = g.appid;
        let current = self.matched(appid).cloned();
        if current.is_none() && !self.searches.contains_key(&appid) && !self.cfg.api_key.is_empty() {
            self.search(&ctx, appid, sgdb::search_term(&g.name), true);
        }
        ui.horizontal(|ui| {
            small(ui, "Jeu sur SteamGridDB :");
            match &current {
                Some(m) => { ui.label(RichText::new(&m.name).strong()); }
                None => match self.searches.get(&appid) {
                    Some(SearchState::Loading) => { ui.add(egui::Spinner::new().size(14.0)); }
                    Some(SearchState::Failed(e)) => { ui.label(RichText::new(e).color(BAD)); }
                    _ => { ui.label(RichText::new("pas trouvé : cherche-le ci-dessous").color(WARN)); }
                },
            }
            if let Some(m) = &current {
                if ui.small_button("Voir sur le site").clicked() { selfupdate::open_url(&format!("https://www.steamgriddb.com/game/{}", m.id)); }
            }
        });
        ui.horizontal(|ui| {
            let text = self.search_text.entry(appid).or_insert_with(|| sgdb::search_term(&g.name));
            let edit = ui.add(egui::TextEdit::singleline(text).hint_text("Nom du jeu à chercher").desired_width((ui.available_width() - 330.0).max(160.0)));
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let term = text.clone();
            if (ui.button("Chercher").clicked() || enter) && !term.trim().is_empty() { self.search(&ctx, appid, term, false); }
            let can = self.auto.is_none() && !self.missing(appid).is_empty();
            if button(ui, can, "Remplir ce qui manque", false) { self.start_auto(&ctx, Some(appid)); }
        });
        if self.show_results {
            if let Some(SearchState::Done(list)) = self.searches.get(&appid) {
                let list = list.clone();
                let mut pick = None;
                let mut close = false;
                egui::Frame::new().fill(CARD2).corner_radius(10).inner_margin(Margin::same(8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if list.is_empty() { small(ui, "Aucun résultat : essaie un nom plus court ou en anglais."); }
                    for s in list.iter().take(10) {
                        let year = s.year().map(|y| format!("  ({y})")).unwrap_or_default();
                        let is_cur = current.as_ref().is_some_and(|m| m.id == s.id);
                        if ui.selectable_label(is_cur, format!("{}{year}{}", s.name, if s.verified { "  ✔" } else { "" })).clicked() { pick = Some(s.clone()); }
                    }
                    if ui.small_button("Fermer").clicked() { close = true; }
                });
                if let Some(s) = pick { self.set_match(appid, &s); self.show_results = false; }
                if close { self.show_results = false; }
            }
        }
    }

    fn candidates(&mut self, ui: &mut egui::Ui, g: &Game) {
        let ctx = ui.ctx().clone();
        let appid = g.appid;
        ui.horizontal(|ui| {
            for k in Kind::ALL {
                let has = self.art_of(appid, k).is_some();
                let txt = RichText::new(format!("{}{}", k.label(), if has { "  ✔" } else { "" })).color(if self.tab == k { TEXT } else if has { OK } else { MUTED });
                if ui.selectable_label(self.tab == k, txt).clicked() { self.tab = k; }
            }
        });
        let kind = self.tab;
        ui.horizontal(|ui| {
            small(ui, match kind {
                Kind::Cover => "Couverture verticale de la bibliothèque (600×900). Clique sur une image pour l'appliquer.",
                Kind::Wide => "Couverture horizontale, montrée dans « Jeux récents » (920×430).",
                Kind::Hero => "Grande image en haut de la page du jeu (1920×620).",
                Kind::Logo => "Logo posé sur l'arrière-plan.",
                Kind::Icon => "Petite icône de la liste des jeux (écrite dans Steam au prochain redémarrage s'il est ouvert).",
            });
            if self.art_of(appid, kind).is_some() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Retirer l'image actuelle").clicked() { self.remove(appid, kind); }
                });
            }
        });
        ui.add_space(4.0);
        let Some(m) = self.matched(appid).cloned() else {
            small(ui, "Choisis d'abord le jeu sur SteamGridDB.");
            return;
        };
        if !self.lists.contains_key(&(m.id, kind)) { self.load_list(&ctx, m.id, kind); }
        let (items, loading, error, more) = {
            let l = &self.lists[&(m.id, kind)];
            (l.items.clone(), l.loading, l.error.clone(), l.more)
        };
        if let Some(e) = &error {
            ui.horizontal(|ui| {
                ui.label(RichText::new(e).color(BAD));
                if ui.small_button("Réessayer").clicked() { self.load_list(&ctx, m.id, kind); }
            });
        }
        if items.is_empty() && !loading && error.is_none() { small(ui, format!("Aucune image de ce type pour « {} ».", m.name)); }
        let (tw, th) = match kind {
            Kind::Cover => (132.0, 198.0), Kind::Wide => (246.0, 115.0), Kind::Hero => (330.0, 106.0), Kind::Logo => (230.0, 110.0), Kind::Icon => (86.0, 86.0),
        };
        let current_file = self.art_of(appid, kind).cloned();
        let busy = self.applying.contains(&(appid, kind));
        let mut apply = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                for img in &items {
                    let (r, resp) = ui.allocate_exact_size(egui::vec2(tw, th), Sense::click());
                    if !ui.is_rect_visible(r) { continue; }
                    let key = format!("url:{}", img.thumb);
                    if matches!(kind, Kind::Logo | Kind::Icon) { ui.painter().rect_filled(r, 8, CARD2); }
                    match self.tex.url(&img.thumb) {
                        Some(t) => paint_tex(ui, if matches!(kind, Kind::Logo | Kind::Icon) { r.shrink(8.0) } else { r }, t, !matches!(kind, Kind::Logo | Kind::Icon), 8),
                        None => { if !matches!(kind, Kind::Logo | Kind::Icon) { placeholder(ui, r, ""); } if !self.tex.failed(&key) { egui::Spinner::new().size(16.0).paint_at(ui, egui::Rect::from_center_size(r.center(), egui::vec2(16.0, 16.0))); } }
                    }
                    let hovered = resp.contains_pointer();
                    if hovered { ui.painter().rect_stroke(r, 8, Stroke::new(2.0_f32, ACCENT), egui::StrokeKind::Inside); }
                    let author = img.author.as_ref().and_then(|a| a.name.clone()).unwrap_or_default();
                    let resp = resp.on_hover_text(format!("{}×{}{}\nClique pour appliquer", img.width, img.height, if author.is_empty() { String::new() } else { format!(" · par {author}") }));
                    if resp.clicked() && !busy { apply = Some(img.clone()); }
                }
            });
            if loading { ui.add_space(8.0); ui.horizontal(|ui| { ui.add(egui::Spinner::new().size(16.0)); small(ui, "Chargement des images…"); }); }
            if more && !loading { ui.add_space(8.0); if ui.button("Plus d'images").clicked() { self.load_list(&ctx, m.id, kind); } }
            ui.add_space(12.0);
        });
        if busy { ctx.request_repaint_after(Duration::from_millis(100)); }
        if let Some(img) = apply { self.apply(&ctx, appid, kind, &img); }
        let _ = current_file;
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            ui.set_max_width(560.0);
            card(ui, |ui| ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.label(RichText::new("Bienvenue dans Steam Jaquettes").size(20.0).strong());
                ui.add_space(4.0);
                ui.label("Steam Jaquettes cherche sur SteamGridDB les couvertures, arrière-plans, logos et icônes de tes jeux non-Steam et les installe dans Steam.");
                ui.add_space(6.0);
                ui.label("SteamGridDB demande une clé personnelle, gratuite :");
                ui.label("1. Connecte-toi sur steamgriddb.com (avec ton compte Steam).");
                ui.label("2. Ouvre Préférences, onglet API, et génère une clé.");
                ui.label("3. Colle-la ici.");
                ui.add_space(6.0);
                if ui.button("Ouvrir la page de la clé API").clicked() { selfupdate::open_url(sgdb::KEY_PAGE); }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.key_input).hint_text("Clé API SteamGridDB").desired_width(330.0));
                    if button(ui, !self.key_input.trim().is_empty(), "Enregistrer", true) {
                        self.cfg.api_key = self.key_input.trim().to_string();
                        self.cfg.save();
                        self.searches.clear();
                    }
                });
            }));
        });
    }

    fn settings_dialog(&mut self, ctx: &egui::Context) {
        if !self.settings { return; }
        let mut close = false;
        egui::Modal::new(egui::Id::new("settings")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(RichText::new("Réglages").size(18.0).strong());
            ui.add_space(4.0);
            ui.label("Clé API SteamGridDB");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.key_input).password(true).desired_width(300.0));
                if ui.small_button("Obtenir une clé").clicked() { selfupdate::open_url(sgdb::KEY_PAGE); }
            });
            ui.add_space(6.0);
            if let Some(a) = self.acc() {
                small(ui, format!("Images rangées dans {}", a.grid().display()));
                small(ui, "Une copie de shortcuts.vdf est faite avant la première modification (shortcuts.vdf.jaquettes-sauvegarde).");
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                small(ui, format!("Steam Jaquettes {}", env!("CARGO_PKG_VERSION")));
                let label = if self.update.busy() { "Recherche…" } else { "Rechercher les mises à jour" };
                if ui.add_enabled(!self.update.busy(), egui::Button::new(RichText::new(label).size(12.5)).small()).clicked() { self.update.check_now(ui.ctx()); }
                let mut auto = self.update.prefs.automatic;
                if ui.checkbox(&mut auto, RichText::new("automatiquement").size(12.5).color(MUTED)).changed() {
                    self.update.prefs.automatic = auto;
                    self.update.prefs.save();
                }
            });
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if button(ui, true, "Enregistrer", true) {
                    let key = self.key_input.trim().to_string();
                    if key != self.cfg.api_key { self.cfg.api_key = key; self.cfg.save(); self.searches.clear(); self.lists.clear(); }
                    close = true;
                }
                if ui.button("Annuler").clicked() { close = true; }
            });
        });
        if close { self.settings = false; }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tex.poll(ctx);
        while let Ok(m) = self.rx.try_recv() { self.handle(ctx, m); }
        self.poll();
        ctx.request_repaint_after(Duration::from_secs(2));

        egui::TopBottomPanel::top("top").frame(egui::Frame::new().fill(BG).inner_margin(Margin { left: 16, right: 16, top: 12, bottom: 6 })).show(ctx, |ui| {
            self.top_bar(ui);
            ui.add_space(4.0);
            self.banners(ui);
        });

        egui::TopBottomPanel::bottom("bottom").frame(egui::Frame::new().fill(BG).inner_margin(Margin { left: 16, right: 16, top: 4, bottom: 6 })).show(ctx, |ui| {
            ui.horizontal(|ui| {
                small(ui, format!("Steam Jaquettes {}", env!("CARGO_PKG_VERSION")));
                let label = if self.update.busy() { "Recherche…" } else { "Rechercher les mises à jour" };
                if ui.add_enabled(!self.update.busy(), egui::Button::new(RichText::new(label).size(12.5)).small()).clicked() { self.update.check_now(ui.ctx()); }
                let mut auto = self.update.prefs.automatic;
                if ui.checkbox(&mut auto, RichText::new("automatiquement").size(12.5).color(MUTED)).changed() {
                    self.update.prefs.automatic = auto;
                    self.update.prefs.save();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Label::new(RichText::new("Images : SteamGridDB").size(12.5).color(MUTED)).sense(Sense::click())).on_hover_text("steamgriddb.com").clicked() {
                        selfupdate::open_url("https://www.steamgriddb.com/");
                    }
                });
            });
        });

        if self.accounts.is_empty() || self.games.is_empty() {
            egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin::same(16))).show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.set_max_width(560.0);
                    card(ui, |ui| {
                        if self.accounts.is_empty() {
                            ui.label(RichText::new("Steam introuvable").size(18.0).strong());
                            ui.label("Aucun dossier Steam avec des comptes n'a été trouvé (~/.local/share/Steam ou la version Flatpak). Lance Steam une fois et connecte-toi.");
                        } else if let Some(e) = &self.load_error {
                            ui.label(RichText::new("Impossible de lire les jeux non-Steam").size(18.0).strong());
                            ui.label(e);
                        } else {
                            ui.label(RichText::new("Aucun jeu non-Steam sur ce compte").size(18.0).strong());
                            ui.label("Dans Steam : Jeux, puis « Ajouter un jeu non-Steam à ma bibliothèque ». La liste se met à jour toute seule.");
                        }
                    });
                });
            });
        } else {
            egui::SidePanel::left("games").resizable(false).exact_width(300.0).frame(egui::Frame::new().fill(BG).inner_margin(Margin { left: 10, right: 6, top: 6, bottom: 10 })).show(ctx, |ui| {
                self.game_list(ui);
            });
            egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin { left: 10, right: 16, top: 6, bottom: 10 })).show(ctx, |ui| {
                if self.cfg.api_key.is_empty() { self.welcome(ui); return; }
                let Some(g) = self.selected.and_then(|s| self.games.iter().find(|g| g.appid == s)).cloned() else {
                    small(ui, "Choisis un jeu dans la liste.");
                    return;
                };
                self.preview(ui, &g);
                ui.add_space(6.0);
                card(ui, |ui| self.match_row(ui, &g));
                ui.add_space(6.0);
                self.candidates(ui, &g);
            });
        }
        self.settings_dialog(ctx);
        self.update.ui(ctx, ACCENT, ON_ACCENT, CARD2);
        self.hook.ui(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) { self.update.relaunch_if_needed(); }
}

// ---------------------------------------------------------------- tests

/// Pour les tests seulement : `APP_SCREENSHOT=/chemin.png` enregistre une
/// capture après `APP_SCREENSHOT_DELAY` s (4 par défaut) puis ferme l'app ;
/// `JAQUETTES_TEST=sel:<nom>,tab:<cover|wide|hero|logo|icon>,demo,hide:a|b` choisit le jeu
/// et l'onglet ; `demo` cache le compte Steam, `hide` retire des jeux de la liste (captures).
struct TestHook { path: Option<String>, delay: f32, start: Instant, requested: bool, select: Option<String>, tab: Option<Kind>, demo: bool, hide: Vec<String> }

impl Default for TestHook {
    fn default() -> Self {
        let spec = std::env::var("JAQUETTES_TEST").unwrap_or_default();
        let mut select = None;
        let mut tab = None;
        let (mut demo, mut hide) = (false, Vec::new());
        for part in spec.split(',') {
            if part == "demo" { demo = true; }
            if let Some(h) = part.strip_prefix("hide:") { hide = h.split('|').map(|s| s.to_lowercase()).filter(|s| !s.is_empty()).collect(); }
            if let Some(s) = part.strip_prefix("sel:") { select = Some(s.to_string()); }
            if let Some(t) = part.strip_prefix("tab:") { tab = Kind::ALL.into_iter().find(|k| k.key() == t); }
        }
        TestHook {
            path: std::env::var("APP_SCREENSHOT").ok().filter(|s| !s.is_empty()),
            delay: std::env::var("APP_SCREENSHOT_DELAY").ok().and_then(|s| s.parse().ok()).unwrap_or(4.0),
            start: Instant::now(), requested: false, select, tab, demo, hide,
        }
    }
}

impl TestHook {
    fn ui(&mut self, ctx: &egui::Context) {
        let Some(path) = self.path.clone() else { return };
        if !self.requested && self.start.elapsed().as_secs_f32() >= self.delay {
            self.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        ctx.request_repaint_after(Duration::from_millis(200));
        let shot = ctx.input(|i| i.events.iter().find_map(|e| if let egui::Event::Screenshot { image, .. } = e { Some(image.clone()) } else { None }));
        if let Some(img) = shot {
            let [w, h] = img.size;
            let raw: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, raw) { let _ = buf.save(&path); }
            self.path = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

fn main() -> eframe::Result {
    let icon_png = include_bytes!("../resources/icon.png");
    // Raccourci laissé par les versions 1.0.0 et 1.0.1 (alors appelées Jaquettes).
    desktop::remove_legacy("jaquettes");
    desktop::ensure(&desktop::Entry { app_id: "steamjaquettes", name: "Steam Jaquettes", comment: "Images des jeux non-Steam (SteamGridDB)", categories: "Game;Utility;", icon_png });
    let icon = image::load_from_memory(icon_png).ok().map(|i| {
        let i = i.to_rgba8();
        let (w, h) = i.dimensions();
        egui::IconData { rgba: i.into_raw(), width: w, height: h }
    });
    let mut viewport = egui::ViewportBuilder::default().with_title("Steam Jaquettes").with_app_id("steamjaquettes").with_inner_size([1240.0, 860.0]).with_min_inner_size([900.0, 600.0]);
    if let Some(i) = icon { viewport = viewport.with_icon(i); }
    let options = eframe::NativeOptions { viewport, renderer: eframe::Renderer::Glow, vsync: true, ..Default::default() };
    eframe::run_native("Steam Jaquettes", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
