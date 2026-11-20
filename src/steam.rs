//! Ce que l'app sait de Steam : où il est installé, les comptes, les jeux
//! non-Steam (`userdata/<compte>/config/shortcuts.vdf`) et leurs images
//! personnalisées (`userdata/<compte>/config/grid/`).
//!
//! Noms des fichiers dans `grid/` (identifiant = appid du raccourci, non signé) :
//! - `<id>p.png`      couverture (portrait, 600×900)
//! - `<id>.png`       grande couverture (paysage, 920×430)
//! - `<id>_hero.png`  arrière-plan (1920×620)
//! - `<id>_logo.png`  logo
//! - l'icône est un fichier quelconque dont le chemin est écrit dans le champ
//!   `icon` du raccourci ; l'app la range dans `grid/<id>_icon.png`.

use crate::vdf;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind { Cover, Wide, Hero, Logo, Icon }

impl Kind {
    pub const ALL: [Kind; 5] = [Kind::Cover, Kind::Wide, Kind::Hero, Kind::Logo, Kind::Icon];

    pub fn label(self) -> &'static str {
        match self { Kind::Cover => "Couverture", Kind::Wide => "Grande couverture", Kind::Hero => "Arrière-plan", Kind::Logo => "Logo", Kind::Icon => "Icône" }
    }
    pub fn short(self) -> &'static str {
        match self { Kind::Cover => "C", Kind::Wide => "GC", Kind::Hero => "AP", Kind::Logo => "L", Kind::Icon => "I" }
    }
    /// Suffixe du fichier dans grid/ (avant l'extension).
    fn suffix(self) -> &'static str {
        match self { Kind::Cover => "p", Kind::Wide => "", Kind::Hero => "_hero", Kind::Logo => "_logo", Kind::Icon => "_icon" }
    }
    pub fn key(self) -> &'static str {
        match self { Kind::Cover => "cover", Kind::Wide => "wide", Kind::Hero => "hero", Kind::Logo => "logo", Kind::Icon => "icon" }
    }
}

const EXTS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];

// ---------------------------------------------------------------- installation

/// Dossiers où Steam peut être installé (natif, puis Flatpak).
pub fn roots() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return vec![] };
    let mut out: Vec<PathBuf> = Vec::new();
    for p in [home.join(".local/share/Steam"), home.join(".steam/steam"), home.join(".var/app/com.valvesoftware.Steam/data/Steam"), home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam")] {
        if !p.join("userdata").is_dir() { continue; }
        let canon = p.canonicalize().unwrap_or(p.clone());
        if !out.iter().any(|o| o.canonicalize().unwrap_or(o.clone()) == canon) { out.push(p); }
    }
    out
}

#[derive(Clone, Debug)]
pub struct Account { pub id: String, pub name: String, pub dir: PathBuf, pub flatpak: bool, pub games: usize }

impl Account {
    pub fn config(&self) -> PathBuf { self.dir.join("config") }
    pub fn shortcuts(&self) -> PathBuf { self.config().join("shortcuts.vdf") }
    pub fn grid(&self) -> PathBuf { self.config().join("grid") }
}

/// Lit `config/loginusers.vdf` (texte) : identifiant 64 bits → pseudo.
fn persona_names(root: &Path) -> Vec<(u64, String)> {
    enum Tok { S(String), Open, Close }
    let text = std::fs::read_to_string(root.join("config/loginusers.vdf")).unwrap_or_default();
    let mut toks = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    if c == '\\' { if let Some(n) = chars.next() { s.push(n); } continue; }
                    if c == '"' { break; }
                    s.push(c);
                }
                toks.push(Tok::S(s));
            }
            '{' => toks.push(Tok::Open),
            '}' => toks.push(Tok::Close),
            _ => {}
        }
    }
    let (mut out, mut stack, mut i) = (Vec::new(), Vec::<String>::new(), 0);
    while i < toks.len() {
        match (&toks[i], toks.get(i + 1)) {
            (Tok::S(k), Some(Tok::Open)) => { stack.push(k.clone()); i += 2; }
            (Tok::S(k), Some(Tok::S(v))) => {
                if stack.len() == 2 && k.eq_ignore_ascii_case("PersonaName") {
                    if let Ok(id) = stack[1].parse() { out.push((id, v.clone())); }
                }
                i += 2;
            }
            (Tok::Close, _) => { stack.pop(); i += 1; }
            _ => i += 1,
        }
    }
    out
}

/// Tous les comptes ; ceux qui ont des jeux non-Steam d'abord.
pub fn accounts() -> Vec<Account> {
    let mut out = Vec::new();
    for root in roots() {
        let flatpak = root.to_string_lossy().contains("com.valvesoftware.Steam");
        let names = persona_names(&root);
        let Ok(dirs) = std::fs::read_dir(root.join("userdata")) else { continue };
        for d in dirs.flatten() {
            let id = d.file_name().to_string_lossy().to_string();
            let Ok(n) = id.parse::<u64>() else { continue };
            if n == 0 { continue; }
            let name = names.iter().find(|(s, _)| s.wrapping_sub(76561197960265728) == n).map(|(_, p)| p.clone()).unwrap_or_else(|| format!("Compte {id}"));
            let mut a = Account { id, name, dir: d.path(), flatpak, games: 0 };
            a.games = read_games(&a).map(|g| g.len()).unwrap_or(0);
            out.push(a);
        }
    }
    out.sort_by(|a, b| b.games.cmp(&a.games).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    out
}

// ---------------------------------------------------------------- jeux

#[derive(Clone, Debug)]
pub struct Game {
    /// Identifiant du raccourci (non signé), celui des fichiers de grid/.
    pub appid: u32,
    pub name: String,
    pub exe: String,
    pub icon: String,
}

/// Identifiant qu'aurait donné Steam (anciens raccourcis sans « appid »).
pub fn legacy_appid(exe: &[u8], name: &[u8]) -> u32 {
    let mut d = exe.to_vec();
    d.extend_from_slice(name);
    vdf::crc32(&d) | 0x8000_0000
}

fn game_of(entry: &vdf::Value) -> Option<Game> {
    let name = entry.str("AppName")?;
    let exe_raw = entry.raw_str("Exe").unwrap_or(b"");
    let appid = match entry.int("appid") {
        Some(i) if i != 0 => i as u32,
        _ => legacy_appid(exe_raw, entry.raw_str("AppName").unwrap_or(b"")),
    };
    Some(Game { appid, name, exe: String::from_utf8_lossy(exe_raw).into_owned(), icon: entry.str("icon").unwrap_or_default() })
}

pub fn read_games(a: &Account) -> Result<Vec<Game>, String> {
    let data = match std::fs::read(a.shortcuts()) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(format!("Lecture de shortcuts.vdf impossible : {e}")),
    };
    let (root, _) = vdf::parse_file(&data).map_err(|e| format!("shortcuts.vdf illisible : {e}"))?;
    let list = root.get("shortcuts").and_then(|s| s.as_map()).cloned().unwrap_or_default();
    let mut games: Vec<Game> = list.iter().filter_map(|(_, v)| game_of(v)).collect();
    games.sort_by_key(|g| sort_key(&g.name));
    Ok(games)
}

fn sort_key(name: &str) -> String { name.to_lowercase() }

/// Écrit le chemin des icônes dans shortcuts.vdf (Steam doit être fermé, sinon il
/// réécrit le fichier à sa façon). Une copie de sécurité est faite la première fois.
pub fn write_icons(a: &Account, icons: &[(u32, String)]) -> Result<(), String> {
    let path = a.shortcuts();
    let data = std::fs::read(&path).map_err(|e| format!("Lecture de shortcuts.vdf impossible : {e}"))?;
    let (mut root, end) = vdf::parse_file(&data).map_err(|e| format!("shortcuts.vdf illisible : {e}"))?;
    if vdf::write(&root, end) != data { return Err("shortcuts.vdf a un format inattendu : il n'a pas été modifié.".into()); }
    let Some(list) = root.as_map_mut().and_then(|m| m.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(b"shortcuts"))).and_then(|(_, v)| v.as_map_mut()) else {
        return Err("shortcuts.vdf ne contient pas de raccourcis.".into());
    };
    let mut done = 0;
    for (_, entry) in list.iter_mut() {
        let Some(g) = game_of(entry) else { continue };
        if let Some((_, icon)) = icons.iter().find(|(id, _)| *id == g.appid) { entry.set_str("icon", icon); done += 1; }
    }
    if done == 0 { return Ok(()); }
    let backup = path.with_file_name("shortcuts.vdf.jaquettes-sauvegarde");
    if !backup.exists() { std::fs::copy(&path, &backup).map_err(|e| format!("Copie de sécurité impossible : {e}"))?; }
    let tmp = path.with_file_name(format!(".shortcuts.vdf.{}", std::process::id()));
    std::fs::write(&tmp, vdf::write(&root, end)).map_err(|e| format!("Écriture impossible : {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| { let _ = std::fs::remove_file(&tmp); format!("Écriture impossible : {e}") })
}

// ---------------------------------------------------------------- images

/// Fichier actuel d'une image (s'il existe).
pub fn art_file(a: &Account, g: &Game, kind: Kind) -> Option<PathBuf> {
    if kind == Kind::Icon {
        let p = PathBuf::from(&g.icon);
        let image = matches!(p.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref(), Some("png" | "jpg" | "jpeg" | "ico" | "webp" | "bmp"));
        if image && p.is_file() { return Some(p); }
        // Icône posée mais pas encore écrite dans shortcuts.vdf.
        return None;
    }
    EXTS.iter().map(|e| a.grid().join(format!("{}{}.{e}", g.appid, kind.suffix()))).find(|p| p.is_file())
}

/// Où ranger une nouvelle image de ce type.
pub fn art_target(a: &Account, appid: u32, kind: Kind, ext: &str) -> PathBuf {
    a.grid().join(format!("{appid}{}.{ext}", kind.suffix()))
}

/// Enregistre une image (en retirant les autres formats du même type).
pub fn save_art(a: &Account, appid: u32, kind: Kind, bytes: &[u8]) -> Result<PathBuf, String> {
    let (ext, data): (&str, std::borrow::Cow<[u8]>) = match image::guess_format(bytes) {
        Ok(image::ImageFormat::Png) => ("png", bytes.into()),
        Ok(image::ImageFormat::Jpeg) if kind != Kind::Icon => ("jpg", bytes.into()),
        Ok(_) => {
            // Autre format (WebP, ICO…) : converti en PNG, que Steam lit partout.
            let img = image::load_from_memory(bytes).map_err(|e| format!("Image illisible : {e}"))?;
            let mut out = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).map_err(|e| e.to_string())?;
            ("png", out.into())
        }
        Err(_) => return Err("Le fichier reçu n'est pas une image.".into()),
    };
    std::fs::create_dir_all(a.grid()).map_err(|e| format!("Dossier grid impossible à créer : {e}"))?;
    let target = art_target(a, appid, kind, ext);
    let tmp = target.with_file_name(format!(".{}.part", target.file_name().unwrap().to_string_lossy()));
    std::fs::write(&tmp, &data).map_err(|e| format!("Écriture impossible : {e}"))?;
    std::fs::rename(&tmp, &target).map_err(|e| format!("Écriture impossible : {e}"))?;
    for e in EXTS {
        let p = art_target(a, appid, kind, e);
        if p != target { let _ = std::fs::remove_file(p); }
    }
    Ok(target)
}

/// Retire l'image d'un type (fichiers de grid/ seulement).
pub fn remove_art(a: &Account, appid: u32, kind: Kind) {
    for e in EXTS { let _ = std::fs::remove_file(art_target(a, appid, kind, e)); }
}

// ---------------------------------------------------------------- Steam lancé ?

fn steam_pids() -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else { return out };
    for e in dir.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        let comm = std::fs::read_to_string(e.path().join("comm")).unwrap_or_default();
        if comm.trim() != "steam" { continue; }
        // Un processus terminé mais pas encore « ramassé » (zombie) ne compte pas.
        let stat = std::fs::read_to_string(e.path().join("stat")).unwrap_or_default();
        let state = stat.rsplit_once(')').and_then(|(_, r)| r.split_whitespace().next()).unwrap_or("");
        if state == "Z" || state == "X" { continue; }
        out.push(pid);
    }
    out
}

pub fn running() -> bool { !steam_pids().is_empty() }

/// Une commande sans l'environnement propre à l'AppImage.
fn host_command(program: &str) -> Command {
    let mut c = Command::new(program);
    if std::env::var_os("APPIMAGE").is_some() {
        for v in ["LD_LIBRARY_PATH", "LD_PRELOAD", "APPDIR", "APPIMAGE", "ARGV0", "OWD"] { c.env_remove(v); }
    }
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    c
}

fn in_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

/// `launch` : la commande du menu des applications (sur Bazzite, `bazzite-steam`).
fn steam_command(flatpak: bool, launch: bool) -> Command {
    if flatpak {
        let mut c = host_command("flatpak");
        c.args(["run", "com.valvesoftware.Steam"]);
        c
    } else if launch && in_path("bazzite-steam") {
        host_command("bazzite-steam")
    } else {
        host_command("steam")
    }
}

/// Lance la commande et la « ramasse » quand elle se termine, pour qu'elle ne
/// reste pas en zombie.
fn spawn_reaped(mut c: Command) -> std::io::Result<()> {
    let mut child = c.spawn()?;
    std::thread::spawn(move || { let _ = child.wait(); });
    Ok(())
}

/// Ferme Steam proprement et attend qu'il soit fermé (2 minutes au plus).
pub fn shutdown(flatpak: bool) -> Result<(), String> {
    if !running() { return Ok(()); }
    let mut c = steam_command(flatpak, false);
    c.arg("-shutdown");
    spawn_reaped(c).map_err(|e| format!("Impossible de fermer Steam : {e}"))?;
    let start = Instant::now();
    while running() {
        if start.elapsed() > Duration::from_secs(120) { return Err("Steam ne s'est pas fermé (2 minutes d'attente).".into()); }
        std::thread::sleep(Duration::from_millis(500));
    }
    // Steam finit d'écrire ses fichiers juste avant de quitter.
    std::thread::sleep(Duration::from_secs(1));
    Ok(())
}

/// Lance Steam, détaché de l'app.
pub fn start(flatpak: bool) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let mut c = steam_command(flatpak, true);
    unsafe { c.pre_exec(|| { setsid(); Ok(()) }); }
    spawn_reaped(c).map_err(|e| format!("Impossible de lancer Steam : {e}"))
}

extern "C" { fn setsid() -> i32; }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_id() {
        // Exemple connu (Steam ROM Manager) : "\"/usr/bin/foo\"" + "Foo".
        let id = legacy_appid(b"\"/usr/bin/foo\"", b"Foo");
        assert!(id & 0x8000_0000 != 0);
    }

    /// Le bug de la 1.0.0 : `steam -shutdown` terminé mais jamais ramassé restait
    /// un zombie nommé « steam », et l'app croyait Steam toujours ouvert.
    #[test]
    fn zombie_is_not_steam() {
        let dir = std::env::temp_dir().join(format!("jaq-zombie-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("steam");
        std::fs::copy("/usr/bin/true", &fake).unwrap();
        let child = Command::new(&fake).spawn().unwrap();
        let pid = child.id();
        std::thread::sleep(Duration::from_millis(300));
        assert!(std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap().contains(") Z"));
        assert!(!steam_pids().contains(&pid));
        drop(child);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loginusers() {
        let dir = std::env::temp_dir().join(format!("jaq-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(dir.join("config/loginusers.vdf"), "\"users\"\n{\n\t\"76561198113450896\"\n\t{\n\t\t\"AccountName\"\t\t\"x\"\n\t\t\"PersonaName\"\t\t\"Reality\"\n\t}\n\t\"76561199086504585\"\n\t{\n\t\t\"PersonaName\"\t\t\"Dream\"\n\t}\n}\n").unwrap();
        let n = persona_names(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(n, vec![(76561198113450896, "Reality".into()), (76561199086504585, "Dream".into())]);
        assert_eq!(76561198113450896u64 - 76561197960265728, 153185168);
    }
}
