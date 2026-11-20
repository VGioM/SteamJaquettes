//! Client de l'API SteamGridDB (https://www.steamgriddb.com/api/v2).
//! Une clé personnelle (gratuite) est nécessaire : steamgriddb.com, profil,
//! Préférences, onglet API.

use crate::steam::Kind;
use serde::Deserialize;
use std::io::Read;
use std::time::Duration;

const BASE: &str = "https://www.steamgriddb.com/api/v2";
pub const KEY_PAGE: &str = "https://www.steamgriddb.com/profile/preferences/api";

#[derive(Clone, Debug, Deserialize)]
pub struct SgdbGame {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub release_date: Option<i64>,
    #[serde(default)]
    pub verified: bool,
}

impl SgdbGame {
    pub fn year(&self) -> Option<i32> {
        self.release_date.filter(|d| *d > 0).map(|d| 1970 + (d / 31_556_952) as i32)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Author { #[serde(default)] pub name: Option<String> }

#[derive(Clone, Debug, Deserialize)]
pub struct Img {
    pub id: u64,
    pub url: String,
    pub thumb: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub author: Option<Author>,
}

#[derive(Deserialize)]
struct Resp<T> { success: bool, data: Option<T>, #[serde(default)] errors: Vec<String> }

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(40)))
        .http_status_as_error(false)
        .user_agent(concat!("SteamJaquettes/", env!("CARGO_PKG_VERSION")))
        .build().into()
}

fn get<T: for<'de> Deserialize<'de>>(key: &str, path: &str) -> Result<T, String> {
    let key = key.trim();
    if key.is_empty() { return Err("Il faut une clé API SteamGridDB (réglages).".into()); }
    let mut resp = agent().get(&format!("{BASE}{path}")).header("Authorization", &format!("Bearer {key}"))
        .call().map_err(|e| format!("SteamGridDB ne répond pas ({e})."))?;
    let status = resp.status().as_u16();
    if status == 401 || status == 403 { return Err("Clé API SteamGridDB refusée : vérifie-la dans les réglages.".into()); }
    if status == 404 { return Err("Introuvable sur SteamGridDB.".into()); }
    if status == 429 { return Err("SteamGridDB demande de ralentir : réessaie dans un instant.".into()); }
    let body: Resp<T> = resp.body_mut().read_json().map_err(|_| format!("Réponse illisible de SteamGridDB (code {status})."))?;
    if !body.success { return Err(if body.errors.is_empty() { format!("Erreur SteamGridDB (code {status}).") } else { body.errors.join(", ") }); }
    body.data.ok_or_else(|| "Réponse vide de SteamGridDB.".into())
}

/// Encode un morceau de chemin d'URL.
fn enc(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{b:02X}"),
    }).collect()
}

pub fn search(key: &str, term: &str) -> Result<Vec<SgdbGame>, String> {
    let t = term.trim();
    if t.is_empty() { return Ok(vec![]); }
    get(key, &format!("/search/autocomplete/{}", enc(t)))
}

/// Les images d'un jeu pour un type, les mieux notées d'abord. Images fixes
/// seulement (pas d'animations), sans contenu adulte ni humoristique.
pub fn images(key: &str, game: u64, kind: Kind, page: u32) -> Result<Vec<Img>, String> {
    let (path, extra) = match kind {
        Kind::Cover => ("grids", "&dimensions=600x900,342x482,660x930&mimes=image/png,image/jpeg"),
        Kind::Wide => ("grids", "&dimensions=920x430,460x215&mimes=image/png,image/jpeg"),
        Kind::Hero => ("heroes", "&mimes=image/png,image/jpeg"),
        Kind::Logo => ("logos", "&mimes=image/png"),
        Kind::Icon => ("icons", "&mimes=image/png,image/vnd.microsoft.icon"),
    };
    get(key, &format!("/{path}/game/{game}?types=static&nsfw=false&humor=false&epilepsy=false&page={page}{extra}"))
}

/// Télécharge une image (30 Mo au plus).
pub fn download(url: &str) -> Result<Vec<u8>, String> {
    if !url.starts_with("https://") { return Err("Adresse d'image invalide.".into()); }
    let resp = agent().get(url).call().map_err(|e| format!("Téléchargement impossible ({e})."))?;
    if !resp.status().is_success() { return Err(format!("Téléchargement impossible (code {}).", resp.status().as_u16())); }
    let mut out = Vec::new();
    resp.into_body().into_reader().take(30 << 20).read_to_end(&mut out).map_err(|e| format!("Téléchargement interrompu ({e})."))?;
    Ok(out)
}

/// Nom à chercher : sans les préfixes de tri (« z », « zz ») ni les ™ ®.
pub fn search_term(name: &str) -> String {
    let cleaned: String = name.chars().filter(|c| !matches!(c, '™' | '®' | '©')).collect();
    let mut words: Vec<&str> = cleaned.split_whitespace().collect();
    while words.len() > 1 && words[0].len() <= 3 && words[0].chars().all(|c| c == 'z' || c == 'Z') { words.remove(0); }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terms() {
        assert_eq!(search_term("zz Mon Wow"), "Mon Wow");
        assert_eq!(search_term("z Warmane"), "Warmane");
        assert_eq!(search_term("Zelda"), "Zelda");
        assert_eq!(search_term("Metal Gear Solid 4 "), "Metal Gear Solid 4");
        assert_eq!(search_term("DOOM™ Eternal"), "DOOM Eternal");
    }
    #[test]
    fn encoding() { assert_eq!(enc("Tony Hawk's: été"), "Tony%20Hawk%27s%3A%20%C3%A9t%C3%A9"); }
}
