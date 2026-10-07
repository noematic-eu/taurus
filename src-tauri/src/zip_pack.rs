//! Lecture d’un zip par son catalogue central.
//!
//! Le catalogue est à la fin du fichier. L’ouvrir ne lit pas les octets
//! comprimés : une archive de plusieurs dizaines de giga-octets s’ouvre sans
//! extraction, et chaque requête ne décompresse que le fichier demandé.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

use tiny_http::{Header, Request, Response, StatusCode};
use unicode_normalization::UnicodeNormalization;

use crate::md::{MdIndex, Reply, Store};
use crate::progress::{self, Reporter};

const ENTRY_NAMES: &[&str] = &["OUVRIR.html", "ouvrir.html", "index.html", "index.htm"];
/// Plafond d’une lecture en mémoire (Markdown, Interroger, rapport de liens). Le HTTP, lui, streame.
pub(crate) const TEXT_LIMIT: u64 = 16 * 1024 * 1024;

trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

enum Kind {
    Site(String),
    Markdown,
}

#[derive(Default)]
struct Kids {
    dirs: BTreeSet<String>,
    files: BTreeSet<String>,
}

pub(crate) struct ZipPack {
    archive: Mutex<zip::ZipArchive<Box<dyn ReadSeek>>>,
    files: HashMap<String, usize>,
    dirs: HashSet<String>,
    /// Nom plié (NFC, casse ASCII) → nom stocké, seulement quand ils diffèrent.
    file_alias: HashMap<String, String>,
    dir_alias: HashMap<String, String>,
    kids: HashMap<String, Kids>,
    label: String,
    pub(crate) markdown: bool,
    pub(crate) entry: String,
}

impl ZipPack {
    #[cfg(test)]
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        Self::open_with(path, &Reporter::silent())
    }

    pub(crate) fn open_with(path: &Path, progress: &Reporter) -> Result<Self, String> {
        let mut file = File::open(path).map_err(|e| format!("zip introuvable: {e}"))?;
        let label = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "pack".into());
        let reader: Box<dyn ReadSeek> = match catalog_span(&mut file) {
            Some(span) if span.size > 0 => {
                let progress = progress.clone();
                Box::new(CatalogReader::new(file, span, move |filled, size| {
                    progress.tick(
                        "Lecture du catalogue",
                        filled,
                        Some(size),
                        progress::byte_span(filled, size),
                    );
                }))
            }
            _ => Box::new(file),
        };
        Self::from_reader(reader, label, progress)
    }

    fn from_reader(
        reader: Box<dyn ReadSeek>,
        label: String,
        progress: &Reporter,
    ) -> Result<Self, String> {
        let archive = zip::ZipArchive::new(reader).map_err(|e| format!("zip illisible: {e}"))?;
        let mut files = HashMap::new();
        let mut dirs = HashSet::new();
        let mut kids = HashMap::new();
        let total = archive.len() as u64;
        for index in 0..archive.len() {
            let done = index as u64 + 1;
            if done == total || done % 1024 == 0 {
                progress.tick(
                    "Index des fichiers",
                    done,
                    Some(total),
                    progress::count_span(done, total),
                );
            }
            let Some(name) = archive.name_for_index(index) else {
                continue;
            };
            let Some((rel, is_dir)) = normalize(name) else {
                continue;
            };
            if is_dir {
                add_dir(&mut dirs, &mut kids, &rel);
            } else {
                add_file(&mut files, &mut dirs, &mut kids, &rel, index);
            }
        }
        let file_alias = fold_index(files.keys().map(String::as_str));
        let (prefix, kind) = classify(&files, &dirs, &file_alias)?;
        let (files, dirs, kids) = rebase(&files, &dirs, &prefix);
        let file_alias = fold_index(files.keys().map(String::as_str));
        let dir_alias = fold_index(dirs.iter().map(String::as_str));
        let (markdown, entry) = match kind {
            Kind::Site(entry) => (false, entry),
            Kind::Markdown => (true, "index.html".to_string()),
        };
        Ok(Self {
            archive: Mutex::new(archive),
            files,
            dirs,
            file_alias,
            dir_alias,
            kids,
            label,
            markdown,
            entry,
        })
    }

    pub(crate) fn md_index(&self) -> MdIndex {
        self.with_top_dirs(MdIndex::from_rels(self.files.keys()))
    }

    pub(crate) fn md_index_with(&self, progress: &Reporter) -> MdIndex {
        let total = self.files.len() as u64;
        let index = MdIndex::from_rels_with(self.files.keys(), Some(total), &mut |seen, found| {
            progress.tick("Index des notes", seen, Some(total), progress::notes(found));
        });
        self.with_top_dirs(index)
    }

    /// Un dossier d’images n’a pas de `.md`, mais il reste visible comme sur disque.
    fn with_top_dirs(&self, mut index: MdIndex) -> MdIndex {
        index.include_top_dirs(
            self.dirs
                .iter()
                .filter(|name| !name.contains('/') && !skip(name)),
        );
        index
    }

    pub(crate) fn locate(
        &self,
        url_path: &str,
        markdown: bool,
        index: Option<&MdIndex>,
    ) -> Option<String> {
        if markdown {
            if let Some(index) = index {
                return crate::md::markdown_source_in(self, index, url_path);
            }
            let rel = sanitize(url_path).ok()?;
            let name = rel.rsplit('/').next().unwrap_or("");
            if is_md(name) {
                return self.stored_file(&rel);
            }
            return None;
        }
        let rel = sanitize(url_path).ok()?;
        self.resolve_rel(&rel)
    }

    pub(crate) fn load_text(&self, rel: &str) -> Result<String, String> {
        let bytes = self.read_limited(rel)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub(crate) fn respond_url(&self, req: Request) {
        match sanitize(req.url()) {
            Ok(rel) => match self.resolve_rel(&rel) {
                Some(found) => self.respond_rel(req, &found),
                None => respond_status(req, 404, "introuvable"),
            },
            Err(()) => respond_status(req, 403, "interdit"),
        }
    }

    pub(crate) fn respond_rel(&self, req: Request, rel: &str) {
        let Some(rel) = self.stored_file(rel) else {
            respond_status(req, 404, "introuvable");
            return;
        };
        let Some(&index) = self.files.get(&rel) else {
            respond_status(req, 404, "introuvable");
            return;
        };
        let mut archive = self.archive.lock().unwrap_or_else(|err| err.into_inner());
        let mut file = match archive.by_index(index) {
            Ok(file) => file,
            Err(_) => {
                respond_status(req, 500, "illisible");
                return;
            }
        };
        let mime = crate::pack::content_type(Path::new(&rel));
        let len = usize::try_from(file.size()).ok();
        let mut headers = Vec::new();
        if let Ok(header) = Header::from_bytes(b"Content-Type", mime.as_bytes()) {
            headers.push(header);
        }
        if let Ok(header) = Header::from_bytes(b"X-Content-Type-Options", b"nosniff") {
            headers.push(header);
        }
        if let Ok(header) = Header::from_bytes(b"Cache-Control", b"no-store") {
            headers.push(header);
        }
        let response = Response::new(StatusCode(200), headers, &mut file, len, None);
        let _ = req.respond(response);
    }

    fn resolve_rel(&self, rel: &str) -> Option<String> {
        if !rel.is_empty() {
            if let Some(found) = self.stored_file(rel) {
                return Some(found);
            }
        }
        let index_rel = if rel.is_empty() {
            "index.html".to_string()
        } else {
            format!("{rel}/index.html")
        };
        self.stored_file(&index_rel)
    }

    /// Nom stocké dans le catalogue. La casse ASCII et le NFC ne comptent pas :
    /// un zip fait sur macOS mélange souvent les deux.
    fn stored_file(&self, rel: &str) -> Option<String> {
        lookup_folded(rel, |name| self.files.contains_key(name), &self.file_alias)
    }

    fn stored_dir(&self, rel: &str) -> Option<String> {
        if rel.is_empty() {
            return Some(String::new());
        }
        lookup_folded(rel, |name| self.dirs.contains(name), &self.dir_alias)
    }

    pub(crate) fn lookup_file(&self, rel: &str) -> Option<String> {
        self.stored_file(rel)
    }

    pub(crate) fn lookup_dir(&self, rel: &str) -> Option<String> {
        self.stored_dir(rel)
    }

    pub(crate) fn file_count(&self) -> u64 {
        self.files.len() as u64
    }

    fn read_limited(&self, rel: &str) -> Result<Vec<u8>, String> {
        Ok(self.read_bytes(rel)?.0)
    }

    /// Lit au plus `TEXT_LIMIT`. `truncated` est vrai quand le fichier décompressé
    /// dépasse ce plafond : le parcours de liens ne doit pas conclure sur une page coupée.
    pub(crate) fn read_capped(&self, rel: &str) -> Result<(String, bool), String> {
        let (buf, truncated) = self.read_bytes(rel)?;
        Ok((String::from_utf8_lossy(&buf).into_owned(), truncated))
    }

    fn read_bytes(&self, rel: &str) -> Result<(Vec<u8>, bool), String> {
        let rel = self
            .stored_file(rel)
            .ok_or_else(|| format!("Impossible de lire {rel}."))?;
        let index = self
            .files
            .get(&rel)
            .copied()
            .ok_or_else(|| format!("Impossible de lire {rel}."))?;
        let mut archive = self.archive.lock().unwrap_or_else(|err| err.into_inner());
        let mut file = archive
            .by_index(index)
            .map_err(|_| format!("Impossible de lire {rel}."))?;
        let truncated = file.size() > TEXT_LIMIT;
        let cap = usize::try_from(file.size().min(TEXT_LIMIT)).unwrap_or(0);
        let mut buf = Vec::with_capacity(cap);
        (&mut file)
            .take(TEXT_LIMIT)
            .read_to_end(&mut buf)
            .map_err(|_| format!("Impossible de lire {rel}."))?;
        Ok((buf, truncated))
    }
}

impl Store for ZipPack {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn is_file(&self, rel: &str) -> bool {
        self.stored_file(rel).is_some()
    }

    fn is_dir(&self, rel: &str) -> bool {
        self.stored_dir(rel).is_some()
    }

    fn read_text(&self, rel: &str) -> String {
        self.load_text(rel).unwrap_or_default()
    }

    fn list(&self, rel: &str) -> (Vec<String>, Vec<String>) {
        let Some(rel) = self.stored_dir(rel) else {
            return (Vec::new(), Vec::new());
        };
        let Some(kids) = self.kids.get(&rel) else {
            return (Vec::new(), Vec::new());
        };
        let dirs = kids
            .dirs
            .iter()
            .filter(|name| !skip(name))
            .cloned()
            .collect();
        let files = kids
            .files
            .iter()
            .filter(|name| !skip(name) && is_md(name))
            .cloned()
            .collect();
        (dirs, files)
    }

    fn open_file(&self, rel: &str) -> Reply {
        Reply::Zip(rel.to_string())
    }
}

fn respond_status(req: Request, status: u16, body: &str) {
    let _ = req.respond(Response::from_string(body).with_status_code(StatusCode(status)));
}

fn sanitize(url_path: &str) -> Result<String, ()> {
    let url_path = url_path.split(['?', '#']).next().unwrap_or(url_path);
    let path = crate::md::percent_decode(url_path);
    let rel = path.trim_start_matches('/').trim_end_matches('/');
    if rel.is_empty() {
        return Ok(String::new());
    }
    if rel.contains('\\')
        || rel
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return Err(());
    }
    Ok(rel.to_string())
}

/// `None` si le nom sort de l’archive (`..`, absolu mal formé, octet nul).
fn normalize(name: &str) -> Option<(String, bool)> {
    if name.contains('\0') {
        return None;
    }
    let name = name.replace('\\', "/");
    let is_dir = name.ends_with('/');
    let name = name.trim_matches('/');
    if name.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for seg in name.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return None;
        }
        parts.push(seg);
    }
    Some((parts.join("/"), is_dir))
}

fn rest_under<'a>(prefix: &str, rel: &'a str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(rel);
    }
    let rest = rel.strip_prefix(prefix)?.strip_prefix('/')?;
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

fn immediate<'a>(prefix: &str, rel: &'a str) -> Option<(&'a str, bool)> {
    let rest = rest_under(prefix, rel)?;
    match rest.split_once('/') {
        Some((first, _)) => Some((first, true)),
        None => Some((rest, false)),
    }
}

/// NFC puis minuscules ASCII. Deux noms qui ne diffèrent que par là désignent
/// le même fichier, comme après extraction sur APFS.
pub(crate) fn fold_key(name: &str) -> String {
    name.nfc().collect::<String>().to_ascii_lowercase()
}

/// Ne garde un alias que lorsque le nom stocké n’est pas déjà sa forme pliée.
/// En cas de collision, la forme pliée exacte gagne, sinon le nom le plus petit.
pub(crate) fn fold_index<'a>(names: impl Iterator<Item = &'a str>) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    for name in names {
        let key = fold_key(name);
        if let Some(cur) = map.get(&key) {
            let cur_exact = cur == &key;
            let cand_exact = name == key;
            let better = if cand_exact != cur_exact {
                cand_exact
            } else {
                name < cur.as_str()
            };
            if !better {
                continue;
            }
        }
        map.insert(key, name.to_string());
    }
    map.retain(|key, chosen| chosen != key);
    map
}

pub(crate) fn lookup_folded(
    rel: &str,
    exact: impl Fn(&str) -> bool,
    alias: &HashMap<String, String>,
) -> Option<String> {
    if exact(rel) {
        return Some(rel.to_string());
    }
    let key = fold_key(rel);
    if let Some(found) = alias.get(&key) {
        return Some(found.clone());
    }
    if key != rel && exact(&key) {
        return Some(key);
    }
    None
}

fn entry_at(
    files: &HashMap<String, usize>,
    folded: &HashMap<String, String>,
    prefix: &str,
) -> Option<String> {
    for name in ENTRY_NAMES {
        let rel = if prefix.is_empty() {
            (*name).to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let Some(found) = lookup_folded(&rel, |cand| files.contains_key(cand), folded) else {
            continue;
        };
        let rest = if prefix.is_empty() {
            found
        } else {
            let Some(rest) = rest_under(prefix, &found) else {
                continue;
            };
            rest.to_string()
        };
        if !rest.is_empty() && !rest.contains('/') {
            return Some(rest);
        }
    }
    None
}

fn has_md(files: &HashMap<String, usize>, prefix: &str) -> bool {
    files.keys().any(|rel| {
        let Some(rest) = rest_under(prefix, rel) else {
            return false;
        };
        if rest.is_empty() || rest.split('/').any(skip) {
            return false;
        }
        rest.rsplit('/').next().is_some_and(is_md)
    })
}

fn classify(
    files: &HashMap<String, usize>,
    dirs: &HashSet<String>,
    folded: &HashMap<String, String>,
) -> Result<(String, Kind), String> {
    let mut prefix = String::new();
    for _ in 0..64 {
        if let Some(entry) = entry_at(files, folded, &prefix) {
            return Ok((prefix, Kind::Site(entry)));
        }
        let mut dir_names = HashSet::new();
        let mut saw_file = false;
        let mut saw_md = false;
        for rel in files.keys() {
            let Some((first, nested)) = immediate(&prefix, rel) else {
                continue;
            };
            if skip(first) {
                continue;
            }
            if nested {
                dir_names.insert(first.to_string());
            } else {
                saw_file = true;
                if is_md(first) {
                    saw_md = true;
                }
            }
        }
        for rel in dirs {
            let Some((first, _)) = immediate(&prefix, rel) else {
                continue;
            };
            if skip(first) {
                continue;
            }
            dir_names.insert(first.to_string());
        }
        if saw_file {
            if saw_md || has_md(files, &prefix) {
                return Ok((prefix, Kind::Markdown));
            }
            return Err("Pas de OUVRIR.html ni index.html.".into());
        }
        if dir_names.len() == 1 {
            let Some(only) = dir_names.into_iter().next() else {
                return Err("Pas de OUVRIR.html ni index.html.".into());
            };
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(&only);
            continue;
        }
        if has_md(files, &prefix) {
            return Ok((prefix, Kind::Markdown));
        }
        return Err("Pas de OUVRIR.html ni index.html.".into());
    }
    Err("Pas de OUVRIR.html ni index.html.".into())
}

fn rebase(
    files: &HashMap<String, usize>,
    dirs: &HashSet<String>,
    prefix: &str,
) -> (
    HashMap<String, usize>,
    HashSet<String>,
    HashMap<String, Kids>,
) {
    let mut out_files = HashMap::new();
    let mut out_dirs = HashSet::new();
    let mut kids = HashMap::new();
    for (rel, index) in files {
        if let Some(rest) = rest_under(prefix, rel) {
            add_file(&mut out_files, &mut out_dirs, &mut kids, rest, *index);
        }
    }
    for rel in dirs {
        if let Some(rest) = rest_under(prefix, rel) {
            add_dir(&mut out_dirs, &mut kids, rest);
        }
    }
    (out_files, out_dirs, kids)
}

fn add_file(
    files: &mut HashMap<String, usize>,
    dirs: &mut HashSet<String>,
    kids: &mut HashMap<String, Kids>,
    rel: &str,
    index: usize,
) {
    files.insert(rel.to_string(), index);
    let parts: Vec<&str> = rel.split('/').collect();
    let mut parent = String::new();
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        if last {
            kids.entry(parent.clone())
                .or_default()
                .files
                .insert((*part).to_string());
        } else {
            kids.entry(parent.clone())
                .or_default()
                .dirs
                .insert((*part).to_string());
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(part);
            dirs.insert(parent.clone());
        }
    }
}

fn add_dir(dirs: &mut HashSet<String>, kids: &mut HashMap<String, Kids>, rel: &str) {
    let mut parent = String::new();
    for part in rel.split('/') {
        if part.is_empty() {
            continue;
        }
        kids.entry(parent.clone())
            .or_default()
            .dirs
            .insert(part.to_string());
        if !parent.is_empty() {
            parent.push('/');
        }
        parent.push_str(part);
        dirs.insert(parent.clone());
    }
}

fn skip(name: &str) -> bool {
    name == "__MACOSX" || name == ".DS_Store" || name.starts_with('.')
}

fn is_md(name: &str) -> bool {
    name.len() > 3 && name.as_bytes()[name.len() - 3..].eq_ignore_ascii_case(b".md")
}

struct CatalogSpan {
    start: u64,
    size: u64,
}

/// Début et taille du catalogue central, lus dans l’EOCD (ZIP64 compris).
///
/// Sert seulement à la barre. L’archive, elle, est relue par `ZipArchive`.
fn catalog_span(file: &mut File) -> Option<CatalogSpan> {
    let len = file.seek(SeekFrom::End(0)).ok()?;
    if len < 22 {
        return None;
    }
    let tail = len.min(22 + 65_535 + 20 + 1024);
    file.seek(SeekFrom::Start(len - tail)).ok()?;
    let mut buf = vec![0u8; tail as usize];
    file.read_exact(&mut buf).ok()?;
    let base = len - tail;
    if buf.len() < 22 {
        return None;
    }
    let mut i = buf.len() - 22;
    loop {
        if buf[i..i + 4] == [0x50, 0x4b, 0x05, 0x06] {
            let comment = u16::from_le_bytes([buf[i + 20], buf[i + 21]]) as u64;
            let eocd = base + i as u64;
            if eocd + 22 + comment <= len {
                let entries = u16::from_le_bytes([buf[i + 10], buf[i + 11]]);
                let size = u32::from_le_bytes(buf[i + 12..i + 16].try_into().ok()?) as u64;
                let start = u32::from_le_bytes(buf[i + 16..i + 20].try_into().ok()?) as u64;
                let zip64 =
                    entries == u16::MAX || size == u32::MAX as u64 || start == u32::MAX as u64;
                if zip64 {
                    if let Some(span) = zip64_span(file, eocd) {
                        if span.size == 0 || cd_signature(file, span.start) {
                            return Some(span);
                        }
                    }
                } else if size == 0
                    || (start < eocd
                        && start.saturating_add(size) <= eocd
                        && cd_signature(file, start))
                {
                    return Some(CatalogSpan { start, size });
                }
            }
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    None
}

fn zip64_span(file: &mut File, eocd: u64) -> Option<CatalogSpan> {
    if eocd < 20 {
        return None;
    }
    file.seek(SeekFrom::Start(eocd - 20)).ok()?;
    let mut locator = [0u8; 20];
    file.read_exact(&mut locator).ok()?;
    if locator[0..4] != [0x50, 0x4b, 0x06, 0x07] {
        return None;
    }
    let recorded = u64::from_le_bytes(locator[8..16].try_into().ok()?);
    file.seek(SeekFrom::Start(recorded)).ok()?;
    let mut header = [0u8; 56];
    file.read_exact(&mut header).ok()?;
    if header[0..4] != [0x50, 0x4b, 0x06, 0x06] {
        return None;
    }
    let size = u64::from_le_bytes(header[40..48].try_into().ok()?);
    let start = u64::from_le_bytes(header[48..56].try_into().ok()?);
    Some(CatalogSpan { start, size })
}

fn cd_signature(file: &mut File, start: u64) -> bool {
    let mut sig = [0u8; 4];
    file.seek(SeekFrom::Start(start)).is_ok()
        && file.read_exact(&mut sig).is_ok()
        && sig == [0x50, 0x4b, 0x01, 0x02]
}

/// Compte les octets lus pendant le parcours séquentiel du catalogue.
struct CatalogReader<R> {
    inner: R,
    pos: u64,
    start: u64,
    size: u64,
    filled: u64,
    armed: bool,
    finished: bool,
    on_fill: Box<dyn FnMut(u64, u64) + Send>,
}

impl<R> CatalogReader<R> {
    fn new(inner: R, span: CatalogSpan, on_fill: impl FnMut(u64, u64) + Send + 'static) -> Self {
        Self {
            inner,
            pos: 0,
            start: span.start,
            size: span.size,
            filled: 0,
            armed: false,
            finished: false,
            on_fill: Box::new(on_fill),
        }
    }
}

impl<R: Read> Read for CatalogReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if self.armed && n > 0 {
            let room = self.size.saturating_sub(self.filled);
            self.filled += (n as u64).min(room);
            (self.on_fill)(self.filled, self.size);
            if self.filled >= self.size {
                self.armed = false;
                self.finished = true;
            }
        }
        self.pos = self.pos.saturating_add(n as u64);
        Ok(n)
    }
}

impl<R: Seek> Seek for CatalogReader<R> {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let pos = self.inner.seek(from)?;
        if !self.finished && self.size > 0 && pos == self.start {
            self.armed = true;
            self.filled = 0;
        } else if pos != self.pos {
            self.armed = false;
        }
        self.pos = pos;
        Ok(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{SeekFrom, Write};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[test]
    fn opens_a_zip64_hole_without_reading_the_payload() {
        // 32 Mio stockés en trou, avec des en-têtes ZIP64. Le catalogue est
        // derrière ce trou : l’ouverture ne doit lire que lui et index.html.
        const HOLE: usize = 32 * 1024 * 1024;
        const READ_LIMIT: u64 = 2 * 1024 * 1024;

        let read = Arc::new(AtomicU64::new(0));
        let mut sparse = Sparse {
            bytes: std::collections::BTreeMap::new(),
            pos: 0,
            len: 0,
            read: read.clone(),
            armed: false,
        };
        {
            let mut writer = zip::ZipWriter::new(&mut sparse);
            let page = zip::write::SimpleFileOptions::default().large_file(true);
            writer.start_file("index.html", page).unwrap();
            writer.write_all(b"ok").unwrap();
            let hole = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .large_file(true);
            writer.start_file("big.bin", hole).unwrap();
            let zeros = vec![0u8; 1024 * 1024];
            for _ in 0..(HOLE / zeros.len()) {
                writer.write_all(&zeros).unwrap();
            }
            writer.finish().unwrap();
        }
        let logical = sparse.len;
        sparse.armed = true;
        sparse.read.store(0, Ordering::Relaxed);
        let pack =
            ZipPack::from_reader(Box::new(sparse), "gros".into(), &Reporter::silent()).unwrap();
        assert!(!pack.markdown);
        assert_eq!(pack.entry, "index.html");
        assert_eq!(pack.load_text("index.html").unwrap(), "ok");
        let n = read.load(Ordering::Relaxed);
        assert!(logical > HOLE as u64, "{logical}");
        assert!(n < READ_LIMIT, "lu {n} octets sur {logical}");
    }

    #[test]
    fn catalog_progress_reaches_the_central_directory() {
        use std::sync::Mutex;

        for large in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("notes.zip");
            let file = File::create(&path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default().large_file(large);
            writer.start_file("page.md", opts).unwrap();
            writer.write_all(b"# Page").unwrap();
            writer.start_file("other.md", opts).unwrap();
            writer.write_all(b"# Other").unwrap();
            writer.finish().unwrap();

            let mut probe = File::open(&path).unwrap();
            let span = catalog_span(&mut probe).expect("catalogue");
            assert!(span.size > 0, "large={large}");
            assert!(cd_signature(&mut probe, span.start), "large={large}");

            let seen = Arc::new(Mutex::new(Vec::new()));
            let keep = Arc::clone(&seen);
            let progress = Reporter::new(1, move |event| {
                keep.lock().unwrap().push((event.label, event.ratio));
            });
            ZipPack::open_with(&path, &progress).unwrap();
            let events = seen.lock().unwrap();
            assert!(
                events
                    .iter()
                    .any(|(label, ratio)| label == "Lecture du catalogue" && *ratio == Some(1.0)),
                "large={large} {events:?}"
            );
            assert!(
                events
                    .iter()
                    .any(|(label, ratio)| label == "Index des fichiers" && *ratio == Some(1.0)),
                "large={large} {events:?}"
            );
        }
    }

    struct Sparse {
        bytes: std::collections::BTreeMap<u64, u8>,
        pos: u64,
        len: u64,
        read: Arc<AtomicU64>,
        armed: bool,
    }

    impl Write for Sparse {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if buf.iter().all(|byte| *byte == 0) {
                let start = self.pos;
                let end = self.pos + buf.len() as u64;
                self.bytes.retain(|&at, _| at < start || at >= end);
            } else {
                for (offset, byte) in buf.iter().copied().enumerate() {
                    let at = self.pos + offset as u64;
                    if byte == 0 {
                        self.bytes.remove(&at);
                    } else {
                        self.bytes.insert(at, byte);
                    }
                }
            }
            self.pos += buf.len() as u64;
            if self.pos > self.len {
                self.len = self.pos;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seek for Sparse {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            let next = match pos {
                SeekFrom::Start(n) => n as i64,
                SeekFrom::Current(n) => self.pos as i64 + n,
                SeekFrom::End(n) => self.len as i64 + n,
            };
            if next < 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "seek",
                ));
            }
            self.pos = next as u64;
            if self.pos > self.len {
                self.len = self.pos;
            }
            Ok(self.pos)
        }
    }

    impl Read for Sparse {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.len || buf.is_empty() {
                return Ok(0);
            }
            let n = buf.len().min((self.len - self.pos) as usize);
            buf[..n].fill(0);
            let end = self.pos + n as u64;
            for (&at, &byte) in self.bytes.range(self.pos..end) {
                buf[(at - self.pos) as usize] = byte;
            }
            if self.armed {
                let seen = self.read.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
                if seen > 2 * 1024 * 1024 {
                    return Err(std::io::Error::other(format!("lecture de {seen} octets")));
                }
            }
            self.pos += n as u64;
            Ok(n)
        }
    }
}
