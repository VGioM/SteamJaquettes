//! Mise à jour depuis les releases GitHub de VGioM/SteamJaquettes (même principe que
//! Biblifilm, Biblijeu et Recomps).
//!
//! Au lancement (au plus une fois toutes les 20 h, sauf si la recherche
//! automatique est désactivée), l'app lit les releases du dépôt et prend la
//! plus récente qui contient `SteamJaquettes-<version>-linux-x86_64.AppImage`. Si elle
//! est plus récente, l'app propose de l'installer : l'AppImage est téléchargée
//! à côté de celle en place, vérifiée (empreinte SHA-256 publiée par GitHub,
//! en-tête ELF, taille), rendue exécutable, puis échangée avec l'ancienne par
//! un simple renommage, et l'app se relance.

use eframe::egui::{self, RichText};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;

const REPO: &str = "VGioM/SteamJaquettes";
const APP: &str = "Steam Jaquettes";

// ---------------------------------------------------------------- préférences

fn prefs_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("SteamJaquettes").join("update.json"))
}

#[derive(Default)]
pub struct Prefs { pub automatic: bool, last_check: u64, skipped: String }

impl Prefs {
    pub fn load() -> Self {
        let v: serde_json::Value = prefs_path().and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Prefs {
            automatic: v["automatic"].as_bool().unwrap_or(true),
            last_check: v["lastCheck"].as_u64().unwrap_or(0),
            skipped: v["skippedVersion"].as_str().unwrap_or("").to_string(),
        }
    }
    pub fn save(&self) {
        let Some(p) = prefs_path() else { return };
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        let v = serde_json::json!({ "automatic": self.automatic, "lastCheck": self.last_check, "skippedVersion": self.skipped });
        let _ = std::fs::write(p, serde_json::to_string_pretty(&v).unwrap_or_default());
    }
}

fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }

// ---------------------------------------------------------------- releases

#[derive(Clone, Debug)]
pub struct Release { pub version: String, pub notes: String, pub page: String, pub download: String, pub size: u64, pub digest: Option<String> }

#[derive(Deserialize)]
struct GhAsset { name: String, browser_download_url: String, size: u64, digest: Option<String> }
#[derive(Deserialize)]
struct GhRelease { tag_name: String, html_url: Option<String>, body: Option<String>, assets: Vec<GhAsset>, #[serde(default)] draft: bool, #[serde(default)] prerelease: bool }

pub fn clean(tag: &str) -> String {
    let t = tag.trim();
    t.strip_prefix('v').or_else(|| t.strip_prefix('V')).unwrap_or(t).to_string()
}

fn parts(v: &str) -> Vec<u64> {
    clean(v).split('.').map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0)).collect()
}

/// Vrai quand `a` est strictement plus récente que `b`.
pub fn is_newer(a: &str, b: &str) -> bool {
    let (x, y) = (parts(a), parts(b));
    for i in 0..x.len().max(y.len()) {
        let (p, q) = (x.get(i).copied().unwrap_or(0), y.get(i).copied().unwrap_or(0));
        if p != q { return p > q; }
    }
    false
}

fn linux_asset(names: &[String]) -> Option<usize> {
    names.iter().position(|n| n.to_lowercase().ends_with("-linux-x86_64.appimage"))
        .or_else(|| names.iter().position(|n| n.to_lowercase().ends_with(".appimage")))
}

/// Les notes telles qu'on les lit, sans le Markdown.
fn readable_notes(body: &str) -> String {
    body.replace("\r\n", "\n").lines().map(|l| {
        let mut s = l.trim_start_matches('#').to_string();
        if let Some(r) = s.strip_prefix("- ") { s = format!("• {r}"); }
        s.replace("**", "").replace('`', "").trim().to_string()
    }).collect::<Vec<_>>().join("\n").trim().to_string()
}

fn pick(releases: &[GhRelease]) -> Option<(usize, usize)> {
    releases.iter().enumerate().filter(|(_, r)| !r.draft && !r.prerelease)
        .filter_map(|(i, r)| linux_asset(&r.assets.iter().map(|a| a.name.clone()).collect::<Vec<_>>()).map(|a| (i, a)))
        .fold(None, |best: Option<(usize, usize)>, (i, a)| match best {
            Some((b, _)) if !is_newer(&releases[i].tag_name, &releases[b].tag_name) => best,
            _ => Some((i, a)),
        })
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .user_agent(concat!("SteamJaquettes-updater/", env!("CARGO_PKG_VERSION")))
        .build().into()
}

fn latest() -> Result<Release, String> {
    let mut resp = agent().get(&format!("https://api.github.com/repos/{REPO}/releases?per_page=30"))
        .header("Accept", "application/vnd.github+json")
        .call().map_err(|e| format!("GitHub n'a pas répondu correctement ({e})."))?;
    let rels: Vec<GhRelease> = resp.body_mut().read_json().map_err(|_| "GitHub n'a pas répondu correctement (réponse illisible).".to_string())?;
    let (r, i) = pick(&rels).ok_or("Aucune release ne contient d'AppImage.")?;
    let (rel, a) = (&rels[r], &rels[r].assets[i]);
    Ok(Release {
        version: clean(&rel.tag_name),
        notes: readable_notes(rel.body.as_deref().unwrap_or("")),
        page: rel.html_url.clone().unwrap_or_else(|| format!("https://github.com/{REPO}/releases")),
        download: a.browser_download_url.clone(),
        size: a.size,
        digest: a.digest.as_ref().and_then(|d| d.strip_prefix("sha256:")).map(|d| d.to_lowercase()),
    })
}

/// Télécharge, vérifie et met la nouvelle AppImage à la place de `target`.
fn install(rel: &Release, target: &Path, progress: impl Fn(f32)) -> Result<(), String> {
    let dir = target.parent().ok_or("emplacement inconnu")?;
    let writable = |p: &Path| std::fs::metadata(p).map(|m| !m.permissions().readonly()).unwrap_or(false);
    if !writable(dir) || !writable(target) {
        return Err("Impossible de remplacer l'app à son emplacement actuel (droits insuffisants).".into());
    }
    let staging = dir.join(format!(".{}.update-{}", target.file_name().unwrap_or_default().to_string_lossy(), std::process::id()));
    let result = (|| -> Result<(), String> {
        let resp = agent().get(&rel.download).call().map_err(|e| format!("Téléchargement impossible ({e})."))?;
        let mut reader = resp.into_body().into_reader();
        let mut out = std::fs::File::create(&staging).map_err(|e| e.to_string())?;
        let mut hash = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        let mut done: u64 = 0;
        loop {
            let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 { break; }
            hash.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            done += n as u64;
            if rel.size > 0 { progress(done as f32 / rel.size as f32); }
        }
        out.sync_all().ok();
        drop(out);
        if rel.size > 0 && done != rel.size { return Err("Le fichier téléchargé est incomplet.".into()); }
        let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if let Some(d) = &rel.digest { if &hex != d { return Err("L'empreinte du fichier téléchargé ne correspond pas à celle publiée sur GitHub.".into()); } }
        let mut head = [0u8; 4];
        std::fs::File::open(&staging).and_then(|mut f| f.read_exact(&mut head)).map_err(|e| e.to_string())?;
        if head != [0x7f, b'E', b'L', b'F'] { return Err("Le fichier téléchargé n'est pas une AppImage.".into()); }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        std::fs::rename(&staging, target).map_err(|e| e.to_string())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&staging); }
    result
}

/// Une commande sans l'environnement propre à l'AppImage.
fn clean_command(program: &str) -> Command {
    let mut c = Command::new(program);
    if std::env::var_os("APPIMAGE").is_some() {
        for v in ["LD_LIBRARY_PATH", "LD_PRELOAD"] { c.env_remove(v); }
    }
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    c
}

pub fn open_url(url: &str) {
    if url.starts_with("https://") { let _ = clean_command("xdg-open").arg(url).spawn(); }
}

fn bytes(n: u64) -> String {
    let mo = n as f64 / 1_000_000.0;
    format!("{} Mo", format!("{mo:.1}").replace('.', ","))
}

// ---------------------------------------------------------------- interface

enum Msg { Found(Release, bool), UpToDate(bool), Failed(String, bool), Progress(f32), Installed, InstallFailed(String) }

enum Dialog { None, Offer(Release), Downloading(f32), Info(String, String), InstallError(String, String) }

pub struct SelfUpdate {
    pub prefs: Prefs,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    dialog: Dialog,
    busy: bool,
    started: std::time::Instant,
    auto_done: bool,
    relaunch: bool,
}

impl SelfUpdate {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        // Pas de vérification automatique pendant les tests (captures).
        let auto_done = std::env::var_os("APP_SCREENSHOT").is_some();
        SelfUpdate { prefs: Prefs::load(), tx, rx, dialog: Dialog::None, busy: false, started: std::time::Instant::now(), auto_done, relaunch: false }
    }

    pub fn busy(&self) -> bool { self.busy }

    pub fn check_now(&mut self, ctx: &egui::Context) { self.check(ctx, true); }

    fn check(&mut self, ctx: &egui::Context, interactive: bool) {
        if self.busy { return; }
        self.busy = true;
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let msg = match latest() {
                Ok(r) if is_newer(&r.version, env!("CARGO_PKG_VERSION")) => Msg::Found(r, interactive),
                Ok(_) => Msg::UpToDate(interactive),
                Err(e) => Msg::Failed(e, interactive),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// À appeler à chaque image : lance la vérification du jour, affiche les dialogues.
    pub fn ui(&mut self, ctx: &egui::Context, accent: egui::Color32, on_accent: egui::Color32, panel: egui::Color32) {
        if !self.auto_done && self.started.elapsed().as_secs() >= 3 {
            self.auto_done = true;
            if self.prefs.automatic && now().saturating_sub(self.prefs.last_check) > 20 * 3600 { self.check(ctx, false); }
        } else if !self.auto_done { ctx.request_repaint_after(std::time::Duration::from_secs(1)); }

        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Found(r, interactive) => {
                    self.busy = false;
                    self.prefs.last_check = now();
                    self.prefs.save();
                    if interactive || self.prefs.skipped != r.version { self.dialog = Dialog::Offer(r); }
                }
                Msg::UpToDate(i) => {
                    self.busy = false;
                    self.prefs.last_check = now();
                    self.prefs.save();
                    if i { self.dialog = Dialog::Info(format!("{APP} est à jour"), format!("Tu as la dernière version ({}).", env!("CARGO_PKG_VERSION"))); }
                }
                Msg::Failed(e, i) => { self.busy = false; if i { self.dialog = Dialog::Info("Impossible de vérifier les mises à jour".into(), e); } }
                Msg::Progress(f) => self.dialog = Dialog::Downloading(f),
                Msg::Installed => { self.relaunch = true; ctx.send_viewport_cmd(egui::ViewportCommand::Close); }
                Msg::InstallFailed(e) => self.dialog = Dialog::InstallError(e, format!("https://github.com/{REPO}/releases/latest")),
            }
        }

        let primary = |ui: &mut egui::Ui, text: &str| ui.add(egui::Button::new(RichText::new(text).strong().color(on_accent)).fill(accent)).clicked();
        let modal = |id: &str, width: f32, body: &mut dyn FnMut(&mut egui::Ui)| {
            egui::Modal::new(egui::Id::new(id)).show(ctx, |ui| { ui.set_width(width); body(ui); });
        };

        let mut next: Option<Dialog> = None;
        match &self.dialog {
            Dialog::None => {}
            Dialog::Info(title, text) => {
                let (title, text) = (title.clone(), text.clone());
                modal("upd-info", 380.0, &mut |ui| {
                    ui.label(RichText::new(&title).strong().size(16.0));
                    ui.label(&text);
                    ui.add_space(6.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { if ui.button("OK").clicked() { next = Some(Dialog::None); } });
                });
            }
            Dialog::Offer(r) => {
                let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
                let r = r.clone();
                let mut choice = 0;
                modal("upd-offer", 460.0, &mut |ui| {
                    ui.label(RichText::new(format!("{APP} {} est disponible", r.version)).strong().size(16.0));
                    let size = if r.size > 0 { format!(" ({})", bytes(r.size)) } else { String::new() };
                    ui.label(if appimage.is_some() {
                        format!("Tu as la version {}. La mise à jour télécharge la nouvelle version{size}, remplace l'AppImage et relance l'app. Tes réglages restent en place.", env!("CARGO_PKG_VERSION"))
                    } else {
                        format!("Tu as la version {}. L'app n'est pas lancée depuis son AppImage : la page de la release va s'ouvrir.", env!("CARGO_PKG_VERSION"))
                    });
                    if !r.notes.is_empty() {
                        egui::Frame::new().fill(panel).corner_radius(8).inner_margin(8).show(ui, |ui| {
                            egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| { ui.set_width(ui.available_width()); ui.label(RichText::new(&r.notes).size(12.5)); });
                        });
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Ignorer cette version").clicked() { choice = 3; }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if primary(ui, if appimage.is_some() { "Mettre à jour" } else { "Ouvrir la page" }) { choice = 1; }
                            if ui.button("Plus tard").clicked() { choice = 2; }
                        });
                    });
                });
                match choice {
                    1 => match appimage {
                        Some(target) => {
                            let (tx, ctx2) = (self.tx.clone(), ctx.clone());
                            std::thread::spawn(move || {
                                let (tx2, c3) = (tx.clone(), ctx2.clone());
                                let res = install(&r, &target, move |f| { let _ = tx2.send(Msg::Progress(f)); c3.request_repaint(); });
                                let _ = tx.send(match res { Ok(()) => Msg::Installed, Err(e) => Msg::InstallFailed(e) });
                                ctx2.request_repaint();
                            });
                            next = Some(Dialog::Downloading(0.0));
                        }
                        None => { open_url(&r.page); next = Some(Dialog::None); }
                    },
                    2 => next = Some(Dialog::None),
                    3 => { self.prefs.skipped = r.version.clone(); self.prefs.save(); next = Some(Dialog::None); }
                    _ => {}
                }
            }
            Dialog::Downloading(f) => {
                let f = *f;
                modal("upd-dl", 360.0, &mut |ui| {
                    ui.label(format!("Téléchargement de {APP}…"));
                    ui.add(egui::ProgressBar::new(f).show_percentage());
                });
            }
            Dialog::InstallError(e, page) => {
                let (e, page) = (e.clone(), page.clone());
                let mut c = 0;
                modal("upd-err", 420.0, &mut |ui| {
                    ui.label(RichText::new("La mise à jour n'a pas pu être installée").strong().size(16.0));
                    ui.label(format!("{e}\n\nTu peux la télécharger à la main depuis la page de la release."));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if primary(ui, "Ouvrir la page") { c = 1; }
                        if ui.button("Fermer").clicked() { c = 2; }
                    });
                });
                if c == 1 { open_url(&page); }
                if c > 0 { next = Some(Dialog::None); }
            }
        }
        if let Some(n) = next { self.dialog = n; }
    }

    /// Après la fermeture : relance la nouvelle AppImage si une mise à jour vient d'être posée.
    pub fn relaunch_if_needed(&self) {
        if !self.relaunch { return; }
        if let Some(app) = std::env::var_os("APPIMAGE") {
            use std::os::unix::process::CommandExt;
            let pid = std::process::id();
            let mut c = clean_command("sh");
            c.args(["-c", &format!("while kill -0 {pid} 2>/dev/null; do sleep 0.2; done; exec \"$0\""), &app.to_string_lossy()]);
            unsafe { c.pre_exec(|| { setsid(); Ok(()) }); }
            let _ = c.spawn();
        }
    }
}

extern "C" { fn setsid() -> i32; }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions() {
        assert!(is_newer("1.0.10", "1.0.9"));
        assert!(is_newer("v1.0.1", "1.0.0"));
        assert!(!is_newer("v1.0.1", "1.0.1"));
        assert!(!is_newer("1.0", "1.0.0"));
    }
    #[test]
    fn newest_release_with_an_appimage() {
        let rel = |tag: &str, files: &[&str], pre: bool| GhRelease { tag_name: tag.into(), html_url: None, body: None, draft: false, prerelease: pre,
            assets: files.iter().map(|n| GhAsset { name: n.to_string(), browser_download_url: String::new(), size: 0, digest: None }).collect() };
        let list = vec![
            rel("v1.0.3", &["notes.txt"], false),
            rel("v1.0.4", &["SteamJaquettes-1.0.4-linux-x86_64.AppImage"], true),
            rel("v1.0.2", &["SteamJaquettes-1.0.2-linux-x86_64.AppImage"], false),
            rel("v1.0.1", &["SteamJaquettes-1.0.1-linux-x86_64.AppImage"], false),
        ];
        assert_eq!(pick(&list), Some((2, 0)));
        assert_eq!(pick(&list[..1]), None);
    }
    #[test]
    fn notes() { assert_eq!(readable_notes("## Nouveautés\n- **Un** `deux`"), "Nouveautés\n• Un deux"); }
}
