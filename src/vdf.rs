//! Le format VDF binaire de Steam (celui de `shortcuts.vdf`).
//!
//! Chaque entrée est : un octet de type, un nom terminé par 0, puis la valeur.
//! Types : 0 = sous-liste (terminée par l'octet 8), 1 = texte terminé par 0,
//! 2 = entier 32 bits, 3 = flottant, 4 = pointeur, 5 = texte large, 6 = couleur,
//! 7 = entier 64 bits non signé, 10 = entier 64 bits. Les textes sont gardés en
//! octets bruts pour réécrire le fichier exactement comme Steam l'a écrit.

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Map(Vec<(Vec<u8>, Value)>),
    Str(Vec<u8>),
    Int(i32),
    Float([u8; 4]),
    Ptr(i32),
    WStr(Vec<u8>),
    Color(i32),
    U64(u64),
    I64(i64),
}

impl Value {
    pub fn as_map(&self) -> Option<&Vec<(Vec<u8>, Value)>> { if let Value::Map(m) = self { Some(m) } else { None } }
    pub fn as_map_mut(&mut self) -> Option<&mut Vec<(Vec<u8>, Value)>> { if let Value::Map(m) = self { Some(m) } else { None } }

    /// Valeur d'une clé (sans tenir compte des majuscules : Steam écrit « AppName » ou « appname »).
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_map()?.iter().find(|(k, _)| k.eq_ignore_ascii_case(key.as_bytes())).map(|(_, v)| v)
    }

    pub fn str(&self, key: &str) -> Option<String> {
        match self.get(key)? { Value::Str(s) => Some(String::from_utf8_lossy(s).into_owned()), _ => None }
    }

    pub fn raw_str(&self, key: &str) -> Option<&[u8]> {
        match self.get(key)? { Value::Str(s) => Some(s), _ => None }
    }

    pub fn int(&self, key: &str) -> Option<i32> {
        match self.get(key)? { Value::Int(i) => Some(*i), _ => None }
    }

    /// Remplace le texte d'une clé (ou l'ajoute à la fin s'il n'existe pas).
    pub fn set_str(&mut self, key: &str, value: &str) {
        let Some(m) = self.as_map_mut() else { return };
        match m.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key.as_bytes())) {
            Some((_, v)) => *v = Value::Str(value.as_bytes().to_vec()),
            None => m.push((key.as_bytes().to_vec(), Value::Str(value.as_bytes().to_vec()))),
        }
    }
}

struct Reader<'a> { data: &'a [u8], pos: usize }

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, String> {
        let b = *self.data.get(self.pos).ok_or("fin de fichier inattendue")?;
        self.pos += 1;
        Ok(b)
    }
    fn cstr(&mut self) -> Result<Vec<u8>, String> {
        let rest = &self.data[self.pos..];
        let end = rest.iter().position(|&b| b == 0).ok_or("texte non terminé")?;
        self.pos += end + 1;
        Ok(rest[..end].to_vec())
    }
    fn wstr(&mut self) -> Result<Vec<u8>, String> {
        let start = self.pos;
        loop {
            let a = self.byte()?;
            let b = self.byte()?;
            if a == 0 && b == 0 { return Ok(self.data[start..self.pos - 2].to_vec()); }
        }
    }
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let s = self.data.get(self.pos..self.pos + N).ok_or("fin de fichier inattendue")?;
        self.pos += N;
        Ok(s.try_into().unwrap())
    }
    /// Lit les entrées d'une liste jusqu'à l'octet 8 (ou la fin du fichier pour la racine).
    fn map(&mut self, root: bool) -> Result<Vec<(Vec<u8>, Value)>, String> {
        let mut out = Vec::new();
        loop {
            if root && self.pos >= self.data.len() { return Ok(out); }
            let t = self.byte()?;
            if t == 8 || t == 11 { return Ok(out); }
            let key = self.cstr()?;
            let v = match t {
                0 => Value::Map(self.map(false)?),
                1 => Value::Str(self.cstr()?),
                2 => Value::Int(i32::from_le_bytes(self.bytes()?)),
                3 => Value::Float(self.bytes()?),
                4 => Value::Ptr(i32::from_le_bytes(self.bytes()?)),
                5 => Value::WStr(self.wstr()?),
                6 => Value::Color(i32::from_le_bytes(self.bytes()?)),
                7 => Value::U64(u64::from_le_bytes(self.bytes()?)),
                10 => Value::I64(i64::from_le_bytes(self.bytes()?)),
                _ => return Err(format!("type d'entrée inconnu ({t})")),
            };
            out.push((key, v));
        }
    }
}

/// Lit un fichier VDF binaire. La racine est rendue comme une liste.
pub fn parse(data: &[u8]) -> Result<Value, String> {
    let mut r = Reader { data, pos: 0 };
    let m = r.map(true)?;
    Ok(Value::Map(m))
}

fn write_map(out: &mut Vec<u8>, m: &[(Vec<u8>, Value)]) {
    for (k, v) in m {
        let t: u8 = match v { Value::Map(_) => 0, Value::Str(_) => 1, Value::Int(_) => 2, Value::Float(_) => 3, Value::Ptr(_) => 4, Value::WStr(_) => 5, Value::Color(_) => 6, Value::U64(_) => 7, Value::I64(_) => 10 };
        out.push(t);
        out.extend_from_slice(k);
        out.push(0);
        match v {
            Value::Map(m) => { write_map(out, m); out.push(8); }
            Value::Str(s) => { out.extend_from_slice(s); out.push(0); }
            Value::Int(i) | Value::Ptr(i) | Value::Color(i) => out.extend_from_slice(&i.to_le_bytes()),
            Value::Float(f) => out.extend_from_slice(f),
            Value::WStr(s) => { out.extend_from_slice(s); out.extend_from_slice(&[0, 0]); }
            Value::U64(u) => out.extend_from_slice(&u.to_le_bytes()),
            Value::I64(i) => out.extend_from_slice(&i.to_le_bytes()),
        }
    }
}

/// Réécrit la racine. Steam termine le fichier par un octet 8 après la liste
/// « shortcuts » ; `root_end` dit s'il faut l'ajouter (comme dans l'original).
pub fn write(root: &Value, root_end: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if let Value::Map(m) = root { write_map(&mut out, m); }
    if root_end { out.push(8); }
    out
}

/// Lit le fichier et dit s'il se termine par l'octet 8 de fin de racine.
pub fn parse_file(data: &[u8]) -> Result<(Value, bool), String> {
    // On essaie d'abord la forme de Steam (racine terminée par 8), sinon sans.
    let mut r = Reader { data, pos: 0 };
    if let Ok(m) = r.map(false) {
        if r.pos == data.len() { return Ok((Value::Map(m), true)); }
    }
    Ok((parse(data)?, false))
}

/// CRC-32 (IEEE), utilisé par Steam pour l'identifiant des anciens raccourcis.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 { crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 }; }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut d = vec![0];
        d.extend_from_slice(b"shortcuts\0");
        d.push(0); d.extend_from_slice(b"0\0");
        d.push(2); d.extend_from_slice(b"appid\0"); d.extend_from_slice(&(-1234567i32).to_le_bytes());
        d.push(1); d.extend_from_slice(b"AppName\0Jeu \xc3\xa9t\xc3\xa9\0");
        d.push(1); d.extend_from_slice(b"icon\0\0");
        d.push(0); d.extend_from_slice(b"tags\0"); d.push(1); d.extend_from_slice(b"0\0favorite\0"); d.push(8);
        d.push(8); // fin de « 0 »
        d.push(8); // fin de « shortcuts »
        d.push(8); // fin de la racine
        d
    }

    #[test]
    fn roundtrip() {
        let d = sample();
        let (v, end) = parse_file(&d).unwrap();
        assert!(end);
        assert_eq!(write(&v, end), d);
        let g = v.get("shortcuts").unwrap().get("0").unwrap();
        assert_eq!(g.str("appname").as_deref(), Some("Jeu été"));
        assert_eq!(g.int("AppID"), Some(-1234567));
    }

    #[test]
    fn set_icon() {
        let (mut v, end) = parse_file(&sample()).unwrap();
        let g = v.as_map_mut().unwrap()[0].1.as_map_mut().unwrap()[0].1.clone();
        let mut g2 = g.clone();
        g2.set_str("icon", "/x/y.png");
        assert_eq!(g2.str("icon").as_deref(), Some("/x/y.png"));
        v.as_map_mut().unwrap()[0].1.as_map_mut().unwrap()[0].1 = g2;
        let again = parse_file(&write(&v, end)).unwrap().0;
        assert_eq!(again.get("shortcuts").unwrap().get("0").unwrap().str("icon").as_deref(), Some("/x/y.png"));
    }

    #[test]
    fn crc() { assert_eq!(crc32(b"123456789"), 0xCBF4_3926); }

    /// Relit le vrai fichier de l'utilisateur (s'il y en a un) et vérifie qu'on le réécrit à l'identique.
    #[test]
    fn real_files_roundtrip() {
        let Some(home) = std::env::var_os("HOME") else { return };
        let base = std::path::PathBuf::from(home).join(".local/share/Steam/userdata");
        let Ok(dirs) = std::fs::read_dir(base) else { return };
        for d in dirs.flatten() {
            let Ok(data) = std::fs::read(d.path().join("config/shortcuts.vdf")) else { continue };
            let (v, end) = parse_file(&data).unwrap();
            assert_eq!(write(&v, end), data, "{:?}", d.path());
        }
    }
}
