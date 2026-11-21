//! L'icône de l'app sous Wayland : le compositeur (KDE, GNOME) ignore l'icône que
//! la fenêtre fournit et cherche un fichier `<app_id>.desktop` installé. Sans lui,
//! la barre de titre et la barre des tâches montrent l'icône générique « W ».
//!
//! Une épingle vise ce fichier, donc chaque AppImage garde le sien : un raccourci
//! existant n'est jamais redirigé. Une autre copie reçoit une entrée cachée à son
//! nom et une identité de fenêtre propre, qu'on peut épingler séparément.

use std::path::{Path, PathBuf};

pub struct Entry<'a> { pub app_id: &'a str, pub name: &'a str, pub comment: &'a str, pub categories: &'a str, pub icon_png: &'a [u8] }

/// À appeler une fois au lancement. Renvoie l'identité à donner à la fenêtre.
pub fn ensure(e: &Entry) -> String {
    let Some(data) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"))) else { return e.app_id.to_string() };
    // L'AppImage elle-même (pas le dossier temporaire où elle se monte), sinon le programme.
    let exe = std::env::var_os("APPIMAGE").map(PathBuf::from).or_else(|| std::env::current_exe().ok());
    let Some(exe) = exe else { return e.app_id.to_string() };
    let mut changed = false;

    let icon = data.join("icons/hicolor/512x512/apps").join(format!("{}.png", e.app_id));
    if std::fs::read(&icon).map(|b| b != e.icon_png).unwrap_or(true) {
        let _ = std::fs::create_dir_all(icon.parent().unwrap());
        changed |= std::fs::write(&icon, e.icon_png).is_ok();
    }

    let apps = data.join("applications");
    let main = std::fs::read_to_string(apps.join(format!("{}.desktop", e.app_id))).ok();
    let id = match main.as_deref() {
        Some(t) if keeps_target(t) && !exec_target(t).is_some_and(|p| same_file(&p, &exe)) => copy_id(e.app_id, &exe),
        _ => e.app_id.to_string(),
    };
    let hidden = id != e.app_id;
    let file = apps.join(format!("{id}.desktop"));
    let owned = std::fs::read_to_string(&file).is_ok_and(|t| exec_target(&t).is_some_and(|p| same_file(&p, &exe)));
    if !owned {
        let _ = std::fs::create_dir_all(&apps);
        changed |= std::fs::write(&file, entry_text(e, &id, &exe, hidden)).is_ok();
    }
    changed |= remove_stale_copies(&apps, e.app_id, &id);

    // Que le bureau le connaisse AVANT que la fenêtre s'ouvre : KWin cherche
    // l'icône une seule fois, à l'apparition de la fenêtre (KDE : kbuildsycoca ;
    // ailleurs : update-desktop-database). Une seconde, et seulement après un changement.
    if changed {
        for (cmd, args) in [("kbuildsycoca6", vec![]), ("kbuildsycoca5", vec![]), ("update-desktop-database", vec![apps.to_string_lossy().to_string()])] {
            let _ = std::process::Command::new(cmd).args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
        }
    }
    id
}

fn entry_text(e: &Entry, id: &str, exe: &Path, hidden: bool) -> String {
    let exec = exe.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let no_display = if hidden { "NoDisplay=true\n" } else { "" };
    format!("[Desktop Entry]\nType=Application\nName={}\nComment={}\nExec=\"{exec}\" %U\nIcon={}\nCategories={}\nStartupWMClass={id}\nTerminal=false\n{no_display}",
        e.name, e.comment, e.app_id, e.categories)
}

/// Identité stable d'une copie, calculée sur son chemin (FNV-1a).
fn copy_id(app_id: &str, exe: &Path) -> String {
    let path = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    let hash = path.to_string_lossy().bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{app_id}-{hash:016x}")
}

fn is_copy_entry(file_name: &str, app_id: &str) -> bool {
    file_name.strip_prefix(app_id).and_then(|s| s.strip_prefix('-')).and_then(|s| s.strip_suffix(".desktop"))
        .is_some_and(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Retire les entrées de copies dont le fichier n'existe plus.
fn remove_stale_copies(apps: &Path, app_id: &str, current: &str) -> bool {
    let Ok(dir) = std::fs::read_dir(apps) else { return false };
    let mut removed = false;
    for f in dir.flatten() {
        let name = f.file_name().to_string_lossy().to_string();
        if !is_copy_entry(&name, app_id) || name == format!("{current}.desktop") { continue }
        if !std::fs::read_to_string(f.path()).is_ok_and(|t| keeps_target(&t)) {
            removed |= std::fs::remove_file(f.path()).is_ok();
        }
    }
    removed
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// Le fichier visé par la ligne `Exec=` d'un raccourci.
fn exec_target(desktop: &str) -> Option<PathBuf> {
    let exec = desktop.lines().find_map(|l| l.strip_prefix("Exec="))?.trim();
    let Some(quoted) = exec.strip_prefix('"') else {
        return exec.split_whitespace().next().map(PathBuf::from);
    };
    let mut path = String::new();
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(PathBuf::from(path)),
            '\\' => path.push(chars.next()?),
            c => path.push(c),
        }
    }
    None
}

/// Un disque externe ou secondaire peut être simplement démonté : son absence
/// ne veut pas dire que l'AppImage a été déplacée.
fn on_removable_mount(path: &Path) -> bool {
    ["/run/media", "/media", "/mnt"].iter().any(|m| path.starts_with(m))
}

/// Vrai si le raccourci vise un fichier à respecter.
fn keeps_target(desktop: &str) -> bool {
    exec_target(desktop).is_some_and(|p| p.exists() || on_removable_mount(&p))
}

/// Retire le raccourci et l'icône d'un ancien nom de l'app.
#[allow(dead_code)]
pub fn remove_legacy(app_id: &str) {
    let Some(data) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"))) else { return };
    let file = data.join("applications").join(format!("{app_id}.desktop"));
    if file.exists() {
        let _ = std::fs::remove_file(file);
        let _ = std::fs::remove_file(data.join("icons/hicolor/512x512/apps").join(format!("{app_id}.png")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_quoted_and_plain_exec() {
        assert_eq!(exec_target("[Desktop Entry]\nExec=\"/a b/c\\\"d.AppImage\" %U\n"), Some(PathBuf::from("/a b/c\"d.AppImage")));
        assert_eq!(exec_target("Exec=/a/b.AppImage %U\n"), Some(PathBuf::from("/a/b.AppImage")));
        assert_eq!(exec_target("Name=x\n"), None);
    }

    #[test]
    fn respects_pinned_target() {
        assert!(keeps_target("Exec=\"/\" %U\n"));
        assert!(keeps_target("Exec=\"/run/media/system/Disque_demonte/App.AppImage\" %U\n"));
        assert!(!keeps_target("Exec=\"/var/tmp/app-disparue/App.AppImage\" %U\n"));
        assert!(!keeps_target("Name=sans exec\n"));
    }

    #[test]
    fn copy_ids_are_stable_and_distinct() {
        let a = copy_id("app", Path::new("/var/tmp/absente/a.AppImage"));
        assert_eq!(a, copy_id("app", Path::new("/var/tmp/absente/a.AppImage")));
        assert_ne!(a, copy_id("app", Path::new("/var/tmp/absente/b.AppImage")));
        assert!(is_copy_entry(&format!("{a}.desktop"), "app"));
        assert!(!is_copy_entry("app.desktop", "app"));
        assert!(!is_copy_entry("app-handler.desktop", "app"));
    }
}
