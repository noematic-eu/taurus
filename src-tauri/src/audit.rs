//! Rapport de parcours d’une archive zip, WACZ ou WARC.
//!
//! Le parcours suit les mêmes chemins que le lecteur : requête et fragment
//! retirés, décodage pourcent, repli sur `index.html`, casse ASCII et NFC.
//! Seuls le HTML et le CSS atteints sont lus, et jamais au-delà de
//! [`TEXT_LIMIT`](crate::zip_pack::TEXT_LIMIT). Le reste d’un zip n’est pas
//! décompressé. Un WACZ ou un WARC est d’abord matérialisé, comme à l’ouverture.
//! Une base qui sort de l’archive s’applique aux liens suivants. Un dossier
//! ouvert sans slash final garde cette URL : le lecteur n’envoie pas de
//! redirection, et le navigateur ne résout pas les liens comme sous `index.html`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::progress::{self, Reporter};
use crate::zip_pack::{self, ZipPack, TEXT_LIMIT};

const SOURCE_CAP: usize = 20;
const LIST_CAP: usize = 400;
const BROKEN_CAP: usize = 300;
const ENTRY_NAMES: &[&str] = &["OUVRIR.html", "ouvrir.html", "index.html", "index.htm"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Reason {
    Missing,
    Escape,
    Anchor,
    Unreadable,
}

#[derive(Clone, Debug)]
struct Source {
    page: String,
    href: String,
}

#[derive(Debug)]
struct BrokenGroup {
    path: String,
    reason: Reason,
    sources: Vec<Source>,
    source_count: usize,
}

#[derive(Debug)]
pub(crate) struct Report {
    pages: Vec<String>,
    stylesheets: Vec<String>,
    reached: usize,
    file_count: u64,
    external: usize,
    broken: Vec<BrokenGroup>,
    truncated: Vec<String>,
    broken_links: usize,
}

struct Loaded {
    text: String,
    truncated: bool,
}

trait Site {
    fn file(&self, rel: &str) -> Option<String>;
    fn is_dir(&self, rel: &str) -> bool;
    fn read(&self, rel: &str) -> Result<Loaded, String>;
    fn file_count(&self) -> u64;
    fn entry(&self) -> &str;
}

struct ZipSite<'a> {
    pack: &'a ZipPack,
}

impl Site for ZipSite<'_> {
    fn file(&self, rel: &str) -> Option<String> {
        self.pack.lookup_file(rel)
    }

    fn is_dir(&self, rel: &str) -> bool {
        self.pack.lookup_dir(rel).is_some()
    }

    fn read(&self, rel: &str) -> Result<Loaded, String> {
        let (text, truncated) = self.pack.read_capped(rel)?;
        Ok(Loaded { text, truncated })
    }

    fn file_count(&self) -> u64 {
        self.pack.file_count()
    }

    fn entry(&self) -> &str {
        &self.pack.entry
    }
}

struct DirSite {
    files: HashMap<String, PathBuf>,
    file_alias: HashMap<String, String>,
    dirs: HashSet<String>,
    dir_alias: HashMap<String, String>,
    entry: String,
}

impl DirSite {
    fn open(root: &Path) -> Result<Self, String> {
        let mut files = HashMap::new();
        let mut dirs = HashSet::new();
        walk_dir(root, root, &mut files, &mut dirs)?;
        let file_alias = zip_pack::fold_index(files.keys().map(String::as_str));
        let dir_alias = zip_pack::fold_index(dirs.iter().map(String::as_str));
        let site = Self {
            files,
            file_alias,
            dirs,
            dir_alias,
            entry: String::new(),
        };
        let entry = ENTRY_NAMES
            .iter()
            .find_map(|name| site.file(name))
            .ok_or_else(|| "Pas de page d’entrée dans l’archive.".to_string())?;
        Ok(Self { entry, ..site })
    }
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    files: &mut HashMap<String, PathBuf>,
    dirs: &mut HashSet<String>,
) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|err| format!("Lecture impossible : {err}"))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("Lecture impossible : {err}"))?;
        let path = entry.path();
        let meta =
            fs::symlink_metadata(&path).map_err(|err| format!("Lecture impossible : {err}"))?;
        if meta.file_type().is_symlink() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() {
            continue;
        }
        if meta.is_dir() {
            add_parents(dirs, &rel);
            dirs.insert(rel);
            walk_dir(root, &path, files, dirs)?;
        } else if meta.is_file() {
            add_parents(dirs, &rel);
            files.insert(rel, path);
        }
    }
    Ok(())
}

fn add_parents(dirs: &mut HashSet<String>, rel: &str) {
    let mut rest = rel;
    while let Some((parent, _)) = rest.rsplit_once('/') {
        if parent.is_empty() {
            break;
        }
        dirs.insert(parent.to_string());
        rest = parent;
    }
}

impl Site for DirSite {
    fn file(&self, rel: &str) -> Option<String> {
        zip_pack::lookup_folded(rel, |name| self.files.contains_key(name), &self.file_alias)
    }

    fn is_dir(&self, rel: &str) -> bool {
        if rel.is_empty() {
            return true;
        }
        zip_pack::lookup_folded(rel, |name| self.dirs.contains(name), &self.dir_alias).is_some()
    }

    fn read(&self, rel: &str) -> Result<Loaded, String> {
        let stored = self
            .file(rel)
            .ok_or_else(|| format!("Impossible de lire {rel}."))?;
        let path = self
            .files
            .get(&stored)
            .ok_or_else(|| format!("Impossible de lire {rel}."))?;
        let meta = fs::symlink_metadata(path).map_err(|_| format!("Impossible de lire {rel}."))?;
        if !meta.is_file() {
            return Err(format!("Impossible de lire {rel}."));
        }
        let truncated = meta.len() > TEXT_LIMIT;
        let mut file = File::open(path).map_err(|_| format!("Impossible de lire {rel}."))?;
        let cap = usize::try_from(meta.len().min(TEXT_LIMIT)).unwrap_or(0);
        let mut buf = Vec::with_capacity(cap);
        (&mut file)
            .take(TEXT_LIMIT)
            .read_to_end(&mut buf)
            .map_err(|_| format!("Impossible de lire {rel}."))?;
        Ok(Loaded {
            text: String::from_utf8_lossy(&buf).into_owned(),
            truncated,
        })
    }

    fn file_count(&self) -> u64 {
        self.files.len() as u64
    }

    fn entry(&self) -> &str {
        &self.entry
    }
}

#[derive(Clone)]
struct BaseUrl {
    origin: Option<String>,
    segments: Vec<String>,
    is_dir: bool,
    /// Nombre de `..` déjà au-dessus de la racine de l’archive. Une origine
    /// distante reste à zéro : le navigateur rabat ces `..`.
    above: u32,
}

struct Joined {
    segments: Vec<String>,
    is_dir: bool,
}

#[derive(Debug)]
struct Escaped {
    above: u32,
    segments: Vec<String>,
    is_dir: bool,
}

#[derive(Debug)]
enum Resolved {
    Skip,
    External(String),
    Escape(Escaped),
    Local {
        path: String,
        is_dir: bool,
        fragment: Option<String>,
    },
}

struct Job {
    file: String,
    url: String,
}

enum Located {
    File(String),
    Missing(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Html,
    Css,
}

struct AnchorCheck {
    target: String,
    fragment: String,
    page: String,
    href: String,
}

/// Vérifie une archive et renvoie le rapport. Le dossier temporaire d’un WACZ
/// est supprimé avant le retour.
pub(crate) fn check_archive(path: &Path, progress: &Reporter) -> Result<Report, String> {
    if !path.is_file() {
        return Err("Choisissez une archive .zip, .wacz ou .warc.".into());
    }
    if crate::pack::is_zip(path) {
        let pack = ZipPack::open_with(path, progress)?;
        if pack.markdown {
            return Err("Cette archive est un dossier de notes, pas un site HTML.".into());
        }
        progress.force("Parcours des liens", None, String::new());
        return Ok(crawl(&ZipSite { pack: &pack }, progress));
    }
    if crate::pack::is_web_archive(path) {
        progress.force("Extraction…", None, String::new());
        let tmp = tempfile::tempdir().map_err(|err| format!("temp: {err}"))?;
        let site = crate::wacz::materialize(path, tmp.path())?;
        let dir = DirSite::open(&site)?;
        progress.force("Parcours des liens", None, String::new());
        return Ok(crawl(&dir, progress));
    }
    Err("Choisissez une archive .zip, .wacz ou .warc.".into())
}

fn crawl<S: Site>(site: &S, progress: &Reporter) -> Report {
    let entry = site.entry().to_string();
    let mut queue = VecDeque::new();
    let mut queued = HashSet::new();
    queue.push_back(Job {
        file: entry.clone(),
        url: entry.clone(),
    });
    queued.insert(entry.clone());

    let mut reached = HashSet::new();
    if site.file(&entry).is_some() {
        reached.insert(entry.clone());
    }

    let mut pages = Vec::new();
    let mut stylesheets = Vec::new();
    let mut truncated = Vec::new();
    let mut external = HashSet::new();
    let mut broken = Vec::new();
    let mut broken_index: HashMap<(String, Reason, String), usize> = HashMap::new();
    let mut referrers: HashMap<String, Vec<Source>> = HashMap::new();
    let mut failed = HashSet::new();
    let mut anchors = Vec::new();
    let mut html_ids: HashMap<String, Option<HashSet<String>>> = HashMap::new();
    let mut listed = HashSet::new();
    let mut seen = 0u64;

    while let Some(job) = queue.pop_front() {
        seen += 1;
        progress.tick("Parcours des liens", seen, None, files_read(seen));
        let path = job.file;
        if failed.contains(&path) {
            continue;
        }
        let kind = kind_of(&path).unwrap_or(Kind::Html);
        let loaded = match site.read(&path) {
            Ok(loaded) => loaded,
            Err(_) => {
                failed.insert(path.clone());
                let sources = referrers.remove(&path).unwrap_or_default();
                let sources = if sources.is_empty() {
                    vec![Source {
                        page: "entrée".to_string(),
                        href: path.clone(),
                    }]
                } else {
                    sources
                };
                push_broken(
                    &mut broken,
                    &mut broken_index,
                    path.clone(),
                    Reason::Unreadable,
                    String::new(),
                    sources,
                );
                if kind == Kind::Html {
                    html_ids.insert(path, None);
                }
                continue;
            }
        };
        let first = listed.insert(path.clone());
        if loaded.truncated && first {
            truncated.push(path.clone());
        }
        let base_doc = base_from_document(&job.url);
        if kind == Kind::Css {
            if first {
                stylesheets.push(path.clone());
            }
            for href in css_urls(&loaded.text) {
                consider(
                    site,
                    &job.url,
                    &href,
                    &base_doc,
                    &mut queue,
                    &mut queued,
                    &mut reached,
                    &mut external,
                    &mut broken,
                    &mut broken_index,
                    &mut referrers,
                    &failed,
                    &mut anchors,
                );
            }
            continue;
        }

        let scan = scan_html(&loaded.text);
        if first {
            pages.push(path.clone());
            if loaded.truncated {
                html_ids.insert(path.clone(), None);
            } else {
                html_ids.insert(path.clone(), Some(scan.ids));
            }
        }
        let base = match scan.base {
            Some(href) => {
                let next = with_base(&base_doc, &href);
                if next.above > 0 {
                    consider(
                        site,
                        &job.url,
                        &href,
                        &base_doc,
                        &mut queue,
                        &mut queued,
                        &mut reached,
                        &mut external,
                        &mut broken,
                        &mut broken_index,
                        &mut referrers,
                        &failed,
                        &mut anchors,
                    );
                }
                next
            }
            None => base_doc,
        };
        for href in scan.links {
            consider(
                site,
                &job.url,
                &href,
                &base,
                &mut queue,
                &mut queued,
                &mut reached,
                &mut external,
                &mut broken,
                &mut broken_index,
                &mut referrers,
                &failed,
                &mut anchors,
            );
        }
    }

    for check in anchors {
        match html_ids.get(&check.target) {
            Some(Some(ids)) if !ids.contains(&check.fragment) => push_broken(
                &mut broken,
                &mut broken_index,
                check.target,
                Reason::Anchor,
                check.fragment,
                vec![Source {
                    page: check.page,
                    href: check.href,
                }],
            ),
            _ => {}
        }
    }

    let broken_links = broken.iter().map(|group| group.source_count).sum();
    Report {
        pages,
        stylesheets,
        reached: reached.len(),
        file_count: site.file_count(),
        external: external.len(),
        broken,
        truncated,
        broken_links,
    }
}

fn consider<S: Site>(
    site: &S,
    page: &str,
    href: &str,
    base: &BaseUrl,
    queue: &mut VecDeque<Job>,
    queued: &mut HashSet<String>,
    reached: &mut HashSet<String>,
    external: &mut HashSet<String>,
    broken: &mut Vec<BrokenGroup>,
    broken_index: &mut HashMap<(String, Reason, String), usize>,
    referrers: &mut HashMap<String, Vec<Source>>,
    failed: &HashSet<String>,
    anchors: &mut Vec<AnchorCheck>,
) {
    let source = Source {
        page: page.to_string(),
        href: href.to_string(),
    };
    match resolve_ref(base, href) {
        Resolved::Skip => {}
        Resolved::External(url) => {
            external.insert(url);
        }
        Resolved::Escape(escaped) => {
            push_broken(
                broken,
                broken_index,
                escape_display(&escaped),
                Reason::Escape,
                String::new(),
                vec![source],
            );
        }
        Resolved::Local {
            path,
            fragment,
            is_dir,
        } => match locate(site, &path) {
            Located::File(found) => {
                reached.insert(found.clone());
                if failed.contains(&found) {
                    push_broken(
                        broken,
                        broken_index,
                        found,
                        Reason::Unreadable,
                        String::new(),
                        vec![source],
                    );
                    return;
                }
                if kind_of(&found).is_some() {
                    referrers
                        .entry(found.clone())
                        .or_default()
                        .push(source.clone());
                    let url = visit_url(&path, is_dir, &found);
                    if queued.insert(url.clone()) {
                        queue.push_back(Job {
                            file: found.clone(),
                            url,
                        });
                    }
                }
                if let Some(fragment) = fragment {
                    if kind_of(&found) == Some(Kind::Html) {
                        anchors.push(AnchorCheck {
                            target: found,
                            fragment,
                            page: source.page,
                            href: source.href,
                        });
                    }
                }
            }
            Located::Missing(missing) => {
                push_broken(
                    broken,
                    broken_index,
                    missing,
                    Reason::Missing,
                    String::new(),
                    vec![source],
                );
            }
        },
    }
}

fn locate<S: Site>(site: &S, path: &str) -> Located {
    if !path.is_empty() {
        if let Some(found) = site.file(path) {
            return Located::File(found);
        }
    }
    let index = if path.is_empty() {
        "index.html".to_string()
    } else {
        format!("{path}/index.html")
    };
    if let Some(found) = site.file(&index) {
        return Located::File(found);
    }
    if path.is_empty() || site.is_dir(path) {
        Located::Missing(index)
    } else {
        Located::Missing(path.to_string())
    }
}

fn push_broken(
    groups: &mut Vec<BrokenGroup>,
    index: &mut HashMap<(String, Reason, String), usize>,
    path: String,
    reason: Reason,
    fragment: String,
    sources: Vec<Source>,
) {
    let key_path = if reason == Reason::Escape {
        path.clone()
    } else {
        zip_pack::fold_key(&path)
    };
    let display = if reason == Reason::Anchor {
        format!("{path}#{fragment}")
    } else {
        path
    };
    let key = (key_path, reason, fragment);
    let Some(&slot) = index.get(&key) else {
        let source_count = sources.len();
        let mut sources = sources;
        sources.truncate(SOURCE_CAP);
        index.insert(key, groups.len());
        groups.push(BrokenGroup {
            path: display,
            reason,
            sources,
            source_count,
        });
        return;
    };
    let group = &mut groups[slot];
    for source in sources {
        group.source_count += 1;
        if group.sources.len() < SOURCE_CAP {
            group.sources.push(source);
        }
    }
}

fn kind_of(path: &str) -> Option<Kind> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "html" | "htm" | "xhtml" | "shtml" => Some(Kind::Html),
        "css" => Some(Kind::Css),
        _ => None,
    }
}

fn files_read(n: u64) -> String {
    if n == 1 {
        "1 fichier".to_string()
    } else {
        format!("{} fichiers", progress::grouped(n))
    }
}

#[cfg(test)]
fn base_from_page(page: &str) -> BaseUrl {
    base_from_document(page)
}

/// URL montrée par le navigateur. Un dossier demandé sans slash final n’est
/// pas une URL de répertoire : `page.html` à côté de `docs` part à la racine.
fn base_from_document(url: &str) -> BaseUrl {
    let is_dir = url.ends_with('/');
    BaseUrl {
        origin: None,
        segments: split_path(url.trim_end_matches('/')),
        is_dir,
        above: 0,
    }
}

fn document_url(path: &str, is_dir: bool) -> String {
    if is_dir {
        if path.is_empty() {
            "/".to_string()
        } else {
            format!("{path}/")
        }
    } else {
        path.to_string()
    }
}

/// `/docs/` et `/docs/index.html` résolvent les liens de la même façon.
/// `/docs` sans slash, non : le lecteur sert l’index sans rediriger.
fn document_matches_file(request: &str, is_dir: bool, stored: &str) -> bool {
    if !is_dir {
        return zip_pack::fold_key(request) == zip_pack::fold_key(stored);
    }
    let (parent, name) = stored.rsplit_once('/').unwrap_or(("", stored));
    name.eq_ignore_ascii_case("index.html")
        && zip_pack::fold_key(parent) == zip_pack::fold_key(request)
}

fn visit_url(request: &str, is_dir: bool, stored: &str) -> String {
    if document_matches_file(request, is_dir, stored) {
        stored.to_string()
    } else {
        document_url(request, is_dir)
    }
}

fn with_base(doc: &BaseUrl, href: &str) -> BaseUrl {
    match resolve_ref(doc, href) {
        Resolved::External(url) => parse_external_base(&url),
        Resolved::Local { path, is_dir, .. } => BaseUrl {
            origin: None,
            segments: split_path(&path),
            is_dir,
            above: 0,
        },
        Resolved::Escape(escaped) => BaseUrl {
            origin: None,
            segments: escaped.segments,
            is_dir: escaped.is_dir,
            above: escaped.above,
        },
        Resolved::Skip => doc.clone(),
    }
}

fn split_path(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|seg| !seg.is_empty())
        .map(|seg| seg.to_string())
        .collect()
}

fn resolve_ref(base: &BaseUrl, raw: &str) -> Resolved {
    // Le pack est servi en http : le parseur WHATWG traite `\` comme `/`
    // avant de reconnaître le schéma. `%5C` reste un antislash décodé.
    let raw = raw.trim().replace('\\', "/");
    if raw.is_empty() {
        return Resolved::Skip;
    }
    if let Some((scheme, rest)) = split_scheme(&raw) {
        let scheme = scheme.to_ascii_lowercase();
        if scheme == "http" || scheme == "https" {
            if rest.starts_with("//") {
                return Resolved::External(strip_fragment(&raw));
            }
            // `http:foo` sur une page http est un chemin du même site.
            // `https:foo` sur cette page est une origine étrangère.
            if scheme == base_scheme(base) {
                return resolve_path(base, rest);
            }
            return match foreign_special(&scheme, rest) {
                Some(url) => Resolved::External(url),
                None => Resolved::Skip,
            };
        }
        return Resolved::Skip;
    }
    if raw.starts_with("//") {
        return Resolved::External(strip_fragment(&raw));
    }
    resolve_path(base, &raw)
}

fn resolve_path(base: &BaseUrl, raw: &str) -> Resolved {
    if raw.is_empty() {
        return Resolved::Skip;
    }
    let (before_hash, fragment) = match raw.split_once('#') {
        Some((path, fragment)) => {
            let fragment = crate::md::percent_decode(fragment);
            let fragment = fragment.trim().to_string();
            (
                path,
                if fragment.is_empty() {
                    None
                } else {
                    Some(fragment)
                },
            )
        }
        None => (raw, None),
    };
    let path_part = before_hash
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(before_hash);
    let decoded = crate::md::percent_decode(path_part);
    if decoded.contains('\0') {
        return Resolved::Local {
            path: decoded,
            is_dir: false,
            fragment,
        };
    }
    match join(base, &decoded) {
        Err(escaped) => Resolved::Escape(escaped),
        Ok(joined) => {
            let path = joined.segments.join("/");
            if let Some(origin) = &base.origin {
                return Resolved::External(external_url(origin, &joined.segments, joined.is_dir));
            }
            Resolved::Local {
                path,
                is_dir: joined.is_dir,
                fragment,
            }
        }
    }
}

fn base_scheme(base: &BaseUrl) -> &'static str {
    match base.origin.as_deref() {
        Some(origin) if origin.starts_with("https://") => "https",
        _ => "http",
    }
}

fn split_scheme(raw: &str) -> Option<(&str, &str)> {
    let bytes = raw.as_bytes();
    match bytes.first() {
        Some(byte) if byte.is_ascii_alphabetic() => {}
        _ => return None,
    }
    let mut i = 1;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'-' || byte == b'.' {
            i += 1;
            continue;
        }
        break;
    }
    if bytes.get(i) == Some(&b':') {
        Some((&raw[..i], &raw[i + 1..]))
    } else {
        None
    }
}

/// `https:foo/bar` sur une page http devient `https://foo/bar`. Un hôte vide
/// (`https:`) n’est pas une URL : on l’ignore.
fn foreign_special(scheme: &str, rest: &str) -> Option<String> {
    let rest = rest.trim_start_matches('/');
    let rest = rest.split_once('#').map(|(url, _)| url).unwrap_or(rest);
    if rest.is_empty() {
        return None;
    }
    let (pathish, query) = rest
        .split_once('?')
        .map(|(path, query)| (path, Some(query)))
        .unwrap_or((rest, None));
    if pathish.is_empty() {
        return None;
    }
    let (host, path) = pathish
        .split_once('/')
        .map(|(host, path)| (host, Some(path)))
        .unwrap_or((pathish, None));
    if host.is_empty() {
        return None;
    }
    let mut url = format!("{scheme}://{host}");
    match path {
        Some(path) => {
            url.push('/');
            url.push_str(path);
        }
        None => url.push('/'),
    }
    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }
    Some(url)
}

fn escape_display(escaped: &Escaped) -> String {
    let mut path = "../".repeat(escaped.above as usize);
    if escaped.segments.is_empty() {
        path.pop();
        return path;
    }
    path.push_str(&escaped.segments.join("/"));
    if escaped.is_dir {
        path.push('/');
    }
    path
}

fn strip_fragment(url: &str) -> String {
    url.split_once('#')
        .map(|(url, _)| url)
        .unwrap_or(url)
        .to_string()
}

fn external_url(origin: &str, segments: &[String], is_dir: bool) -> String {
    let mut url = String::from(origin);
    url.push('/');
    url.push_str(&segments.join("/"));
    if is_dir && !segments.is_empty() {
        url.push('/');
    }
    url
}

fn parse_external_base(url: &str) -> BaseUrl {
    let (prefix, rest) = if let Some(rest) = url.strip_prefix("https://") {
        ("https://", rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        ("http://", rest)
    } else if let Some(rest) = url.strip_prefix("//") {
        ("//", rest)
    } else {
        return BaseUrl {
            origin: Some(url.to_string()),
            segments: Vec::new(),
            is_dir: true,
            above: 0,
        };
    };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let path = path.split_once('?').map(|(path, _)| path).unwrap_or(path);
    let path = path.split_once('#').map(|(path, _)| path).unwrap_or(path);
    let is_dir = path.is_empty() || path.ends_with('/');
    BaseUrl {
        origin: Some(format!("{prefix}{host}")),
        segments: split_path(&crate::md::percent_decode(path)),
        is_dir,
        above: 0,
    }
}

/// Joint un chemin déjà décodé. Un `..` qui sort de l’archive est une erreur :
/// le navigateur le rabat sur l’origine, mais le lien ne désigne rien dans le pack.
/// Une base déjà dehors garde ce décalage pour les liens relatifs suivants.
fn join(base: &BaseUrl, decoded: &str) -> Result<Joined, Escaped> {
    if decoded.is_empty() {
        if base.above > 0 && base.origin.is_none() {
            return Err(Escaped {
                above: base.above,
                segments: base.segments.clone(),
                is_dir: base.is_dir,
            });
        }
        return Ok(Joined {
            segments: base.segments.clone(),
            is_dir: base.is_dir,
        });
    }
    let absolute = decoded.starts_with('/');
    let (mut above, mut segs, mut outside) = if absolute {
        (0, Vec::new(), Vec::new())
    } else if base.origin.is_some() {
        let mut segs = base.segments.clone();
        if !base.is_dir {
            segs.pop();
        }
        (0, segs, Vec::new())
    } else if base.above > 0 {
        let mut outside = base.segments.clone();
        if !base.is_dir {
            outside.pop();
        }
        (base.above, Vec::new(), outside)
    } else if base.is_dir {
        (0, base.segments.clone(), Vec::new())
    } else {
        let mut segs = base.segments.clone();
        segs.pop();
        (0, segs, Vec::new())
    };
    let last = decoded.rsplit('/').next().unwrap_or("");
    let is_dir = last.is_empty() || last == "." || last == "..";
    for seg in decoded.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            if above > 0 {
                if outside.pop().is_none() {
                    above += 1;
                }
                continue;
            }
            if segs.pop().is_none() && base.origin.is_none() {
                above += 1;
            }
            continue;
        }
        if above > 0 {
            outside.push(seg.to_string());
        } else {
            segs.push(seg.to_string());
        }
    }
    if above > 0 {
        return Err(Escaped {
            above,
            segments: outside,
            is_dir,
        });
    }
    Ok(Joined {
        segments: segs,
        is_dir,
    })
}

struct Scan {
    base: Option<String>,
    ids: HashSet<String>,
    links: Vec<String>,
}

struct Attr {
    name: String,
    value: String,
}

fn scan_html(html: &str) -> Scan {
    let bytes = html.as_bytes();
    let mut i = 0;
    let mut base = None;
    let mut ids = HashSet::new();
    let mut links = Vec::new();
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"<!--") {
            if let Some(end) = find_slice(&bytes[i + 4..], b"-->") {
                i = i + 4 + end + 3;
            } else {
                break;
            }
            continue;
        }
        let Some(next) = bytes.get(i + 1).copied() else {
            break;
        };
        if next == b'!' || next == b'?' || next == b'/' {
            if let Some(end) = find_byte(&bytes[i + 1..], b'>') {
                i = i + 1 + end + 1;
            } else {
                break;
            }
            continue;
        }
        let Some(tag_end) = find_tag_end(&bytes[i + 1..]) else {
            break;
        };
        let tag_src = &html[i + 1..i + 1 + tag_end];
        i = i + 1 + tag_end + 1;
        let (name, attrs) = parse_tag(tag_src);
        if name.is_empty() {
            continue;
        }
        if name == "script" {
            collect_tag(&name, &attrs, &mut base, &mut ids, &mut links);
            if let Some((body, after)) = find_close(&bytes[i..], b"script") {
                i += after;
                let _ = body;
            } else {
                break;
            }
            continue;
        }
        if name == "style" {
            collect_tag(&name, &attrs, &mut base, &mut ids, &mut links);
            if let Some((body_end, after)) = find_close(&bytes[i..], b"style") {
                links.extend(css_urls(&html[i..i + body_end]));
                i += after;
            } else {
                links.extend(css_urls(&html[i..]));
                break;
            }
            continue;
        }
        collect_tag(&name, &attrs, &mut base, &mut ids, &mut links);
    }
    Scan { base, ids, links }
}

fn collect_tag(
    tag: &str,
    attrs: &[Attr],
    base: &mut Option<String>,
    ids: &mut HashSet<String>,
    links: &mut Vec<String>,
) {
    for attr in attrs {
        if attr.name == "id" && !attr.value.is_empty() {
            ids.insert(attr.value.clone());
        }
        if tag == "a" && attr.name == "name" && !attr.value.is_empty() {
            ids.insert(attr.value.clone());
        }
        if attr.name == "style" {
            links.extend(css_urls(&attr.value));
        }
    }
    if tag == "base" {
        if base.is_none() {
            if let Some(href) = attr_value(attrs, "href") {
                let href = href.trim();
                if !href.is_empty() {
                    *base = Some(href.to_string());
                }
            }
        }
        return;
    }
    let keys: &[&str] = match tag {
        "a" | "area" | "link" => &["href"],
        "img" | "script" | "iframe" | "embed" | "source" | "audio" | "track" | "input" => {
            &["src", "srcset"]
        }
        "video" => &["src", "srcset", "poster"],
        "object" => &["data"],
        "form" => &["action"],
        _ => &[],
    };
    for key in keys {
        let Some(value) = attr_value(attrs, key) else {
            continue;
        };
        if *key == "srcset" {
            links.extend(srcset_urls(value));
        } else {
            let value = value.trim();
            if !value.is_empty() {
                links.push(value.to_string());
            }
        }
    }
}

fn attr_value<'a>(attrs: &'a [Attr], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|attr| attr.name == name)
        .map(|attr| attr.value.as_str())
}

fn srcset_urls(value: &str) -> Vec<String> {
    let mut rest = value;
    let mut out = Vec::new();
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let (url, next) = split_srcset_candidate(rest);
        if !url.is_empty() {
            out.push(url);
        }
        rest = next;
    }
    out
}

/// Une URL `data:` contient des virgules. Le descripteur (`1x`, `100w`) vient
/// après une espace, donc la coupure de candidat s’arrête sur cette espace.
fn split_srcset_candidate(input: &str) -> (String, &str) {
    let bytes = input.as_bytes();
    let mut i = 0;
    let data = input.len() >= 5 && input[..5].eq_ignore_ascii_case("data:");
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() || (!data && bytes[i] == b',') {
            break;
        }
        i += 1;
    }
    let url = input[..i].trim().to_string();
    while i < bytes.len() && bytes[i] != b',' {
        i += 1;
    }
    if i < bytes.len() {
        i += 1;
    }
    (url, &input[i..])
}

fn parse_tag(src: &str) -> (String, Vec<Attr>) {
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'/' {
        i += 1;
    }
    let name = src[..i].to_ascii_lowercase();
    (name, parse_attrs(&src[i..]))
}

fn parse_attrs(src: &str) -> Vec<Attr> {
    let bytes = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] == b'/' {
            break;
        }
        let start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'/'
        {
            i += 1;
        }
        if i == start {
            break;
        }
        let name = src[start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            out.push(Attr {
                name,
                value: String::new(),
            });
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let value = if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
            let quote = bytes[i];
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            let value = html_decode(&src[start..i]);
            if i < bytes.len() {
                i += 1;
            }
            value
        } else {
            let start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            html_decode(&src[start..i])
        };
        out.push(Attr { name, value });
    }
    out
}

fn find_tag_end(bytes: &[u8]) -> Option<usize> {
    let mut i = 0;
    let mut quote = 0u8;
    while i < bytes.len() {
        let byte = bytes[i];
        if quote != 0 {
            if byte == quote {
                quote = 0;
            }
            i += 1;
            continue;
        }
        if byte == b'"' || byte == b'\'' {
            quote = byte;
            i += 1;
            continue;
        }
        if byte == b'>' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_close(bytes: &[u8], name: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 2 + name.len() <= bytes.len() {
        if bytes[i] == b'<' && bytes[i + 1] == b'/' {
            let rest = &bytes[i + 2..];
            if rest.len() >= name.len() && rest[..name.len()].eq_ignore_ascii_case(name) {
                let mut j = i + 2 + name.len();
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'>' {
                    return Some((i, j + 1));
                }
            }
        }
        i += 1;
    }
    None
}

fn find_byte(bytes: &[u8], needle: u8) -> Option<usize> {
    bytes.iter().position(|byte| *byte == needle)
}

fn find_slice(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes
        .windows(needle.len())
        .position(|window| window == needle)
}

fn html_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some((ch, len)) = decode_entity(&input[i..]) {
                out.push(ch);
                i += len;
                continue;
            }
        }
        let ch = input[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn decode_entity(input: &str) -> Option<(char, usize)> {
    let rest = input.strip_prefix('&')?;
    let end = rest.find(';')?;
    let body = &rest[..end];
    let ch = if let Some(num) = body.strip_prefix('#') {
        let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            num.parse().ok()?
        };
        char::from_u32(code)?
    } else {
        match body {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => return None,
        }
    };
    Some((ch, end + 2))
}

fn css_urls(css: &str) -> Vec<String> {
    let bytes = css.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            if let Some(end) = find_slice(&bytes[i + 2..], b"*/") {
                i = i + 2 + end + 2;
            } else {
                break;
            }
            continue;
        }
        if ident_at(bytes, i, b"url(") {
            if let Some((url, next)) = parse_url(css, i + 4) {
                if !url.is_empty() {
                    out.push(url);
                }
                i = next;
                continue;
            }
        }
        if ident_at(bytes, i, b"@import") {
            let mut j = i + 7;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
                if let Some((url, next)) = parse_css_string(css, j) {
                    if !url.is_empty() {
                        out.push(url);
                    }
                    i = next;
                    continue;
                }
            }
        }
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            if let Some((_, next)) = parse_css_string(css, i) {
                i = next;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn ident_at(bytes: &[u8], i: usize, needle: &[u8]) -> bool {
    if i + needle.len() > bytes.len() || !bytes[i..i + needle.len()].eq_ignore_ascii_case(needle) {
        return false;
    }
    if i > 0 {
        let prev = bytes[i - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'-' {
            return false;
        }
    }
    true
}

fn parse_url(css: &str, start: usize) -> Option<(String, usize)> {
    let bytes = css.as_bytes();
    let mut j = start;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j >= bytes.len() {
        return None;
    }
    if bytes[j] == b'"' || bytes[j] == b'\'' {
        let (url, next) = parse_css_string(css, j)?;
        let mut k = next;
        while k < bytes.len() && bytes[k] != b')' {
            k += 1;
        }
        if k < bytes.len() {
            k += 1;
        }
        return Some((url, k));
    }
    let from = j;
    while j < bytes.len() && bytes[j] != b')' {
        j += 1;
    }
    let url = css[from..j].trim().to_string();
    if j < bytes.len() {
        j += 1;
    }
    Some((url, j))
}

fn parse_css_string(css: &str, start: usize) -> Option<(String, usize)> {
    let bytes = css.as_bytes();
    let quote = *bytes.get(start)?;
    let mut j = start + 1;
    let mut out = String::new();
    while j < bytes.len() {
        if bytes[j] == b'\\' && j + 1 < bytes.len() {
            let ch = css[j + 1..].chars().next()?;
            out.push(ch);
            j += 1 + ch.len_utf8();
            continue;
        }
        if bytes[j] == quote {
            return Some((out, j + 1));
        }
        let ch = css[j..].chars().next()?;
        out.push(ch);
        j += ch.len_utf8();
    }
    None
}

pub(crate) fn render(report: &Report, archive: &str) -> String {
    let mut html = String::new();
    html.push_str("<p class=\"meta\">");
    html.push_str(&escape(archive));
    html.push_str("</p>");
    if report.broken_links == 0 && report.truncated.is_empty() {
        html.push_str("<p class=\"verdict ok\">Aucun lien cassé</p>");
    } else if report.broken_links == 0 {
        html.push_str("<p class=\"verdict ko\">Parcours incomplet</p>");
    } else if report.broken_links == 1 {
        html.push_str("<p class=\"verdict ko\">1 lien cassé</p>");
    } else {
        html.push_str("<p class=\"verdict ko\">");
        html.push_str(&escape(&format!(
            "{} liens cassés",
            progress::grouped(report.broken_links as u64)
        )));
        html.push_str("</p>");
    }
    html.push_str("<p class=\"meta\">");
    html.push_str(&escape(&format!(
        "{} · {} · {} sur {} · {}",
        count_phrase(report.pages.len(), "page lue", "pages lues"),
        count_phrase(
            report.stylesheets.len(),
            "feuille de style",
            "feuilles de style"
        ),
        count_phrase(report.reached, "fichier atteint", "fichiers atteints"),
        progress::grouped(report.file_count),
        count_phrase(report.external, "lien externe", "liens externes"),
    )));
    html.push_str("</p>");

    if !report.truncated.is_empty() {
        html.push_str("<h2>Fichiers tronqués</h2><ul>");
        for path in report.truncated.iter().take(LIST_CAP) {
            let detail = if kind_of(path) == Some(Kind::Css) {
                "lecture limitée à 16 Mio. Les adresses au-delà ne sont pas vérifiées."
            } else {
                "lecture limitée à 16 Mio. Les ancres de cette page ne sont pas vérifiées."
            };
            html.push_str("<li><code>");
            html.push_str(&escape(path));
            html.push_str("</code> — ");
            html.push_str(detail);
            html.push_str("</li>");
        }
        html.push_str("</ul>");
        if report.truncated.len() > LIST_CAP {
            html.push_str(&note(&format!(
                "{LIST_CAP} fichiers tronqués affichés sur {}.",
                progress::grouped(report.truncated.len() as u64)
            )));
        }
    }

    if !report.broken.is_empty() {
        html.push_str("<h2>Liens cassés</h2>");
        for group in report.broken.iter().take(BROKEN_CAP) {
            html.push_str("<section><h3><code>");
            html.push_str(&escape(&group.path));
            html.push_str("</code> · ");
            html.push_str(reason_label(group.reason));
            html.push_str("</h3><ul>");
            for source in &group.sources {
                html.push_str("<li><code>");
                html.push_str(&escape(&source.page));
                html.push_str("</code> <span class=\"href\">");
                html.push_str(&escape(&source.href));
                html.push_str("</span></li>");
            }
            html.push_str("</ul>");
            if group.source_count > group.sources.len() {
                html.push_str(&note(&format!(
                    "{} sources affichées sur {}.",
                    group.sources.len(),
                    progress::grouped(group.source_count as u64)
                )));
            }
            html.push_str("</section>");
        }
        if report.broken.len() > BROKEN_CAP {
            html.push_str(&note(&format!(
                "{BROKEN_CAP} cibles affichées sur {}.",
                progress::grouped(report.broken.len() as u64)
            )));
        }
    }

    html.push_str("<h2>Pages</h2>");
    push_paths(&mut html, &report.pages, "pages affichées");
    if !report.stylesheets.is_empty() {
        html.push_str("<h2>Feuilles de style</h2>");
        push_paths(&mut html, &report.stylesheets, "feuilles affichées");
    }
    html.push_str(
        "<p class=\"meta\">Les adresses écrites seulement dans un script JavaScript ne sont pas des liens.</p>",
    );
    html
}

fn push_paths(html: &mut String, paths: &[String], more: &str) {
    html.push_str("<ol>");
    for path in paths.iter().take(LIST_CAP) {
        html.push_str("<li><code>");
        html.push_str(&escape(path));
        html.push_str("</code></li>");
    }
    html.push_str("</ol>");
    if paths.len() > LIST_CAP {
        html.push_str(&note(&format!(
            "{LIST_CAP} {more} sur {}.",
            progress::grouped(paths.len() as u64)
        )));
    }
}

fn note(text: &str) -> String {
    format!("<p class=\"meta\">{}</p>", escape(text))
}

fn count_phrase(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{} {many}", progress::grouped(n as u64))
    }
}

fn reason_label(reason: Reason) -> &'static str {
    match reason {
        Reason::Missing => "introuvable",
        Reason::Escape => "hors de l’archive",
        Reason::Anchor => "ancre absente",
        Reason::Unreadable => "illisible",
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

#[derive(Default)]
struct ReportSlot {
    html: Option<String>,
    latest: u64,
}

#[derive(Default)]
pub(crate) struct ReportStore {
    slot: Mutex<ReportSlot>,
}

impl ReportStore {
    /// Réserve le prochain rapport. Un parcours plus ancien ne publie plus.
    pub(crate) fn begin(&self) -> u64 {
        let mut slot = self.slot.lock().unwrap_or_else(|err| err.into_inner());
        slot.latest += 1;
        slot.latest
    }

    pub(crate) fn is_current(&self, gen: u64) -> bool {
        self.slot
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .latest
            == gen
    }

    /// Enregistre le HTML si aucun parcours plus récent n’a commencé.
    pub(crate) fn publish(&self, gen: u64, html: String) -> bool {
        let mut slot = self.slot.lock().unwrap_or_else(|err| err.into_inner());
        if slot.latest != gen {
            return false;
        }
        slot.html = Some(html);
        true
    }

    pub(crate) fn get(&self) -> Result<String, String> {
        self.slot
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .html
            .clone()
            .ok_or_else(|| "Aucun rapport.".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::fs::{self, File};
    use std::io::Write;

    use crate::progress::Reporter;
    use crate::zip_pack;

    struct MemSite {
        files: HashMap<String, String>,
        dirs: HashSet<String>,
        file_alias: HashMap<String, String>,
        dir_alias: HashMap<String, String>,
        entry: String,
        bad: HashSet<String>,
        cut: HashSet<String>,
    }

    impl MemSite {
        fn new(files: &[(&str, &str)]) -> Self {
            let mut map = HashMap::new();
            let mut dirs = HashSet::new();
            for (path, body) in files {
                map.insert((*path).to_string(), (*body).to_string());
                add_parents(&mut dirs, path);
            }
            let file_alias = zip_pack::fold_index(map.keys().map(String::as_str));
            let dir_alias = zip_pack::fold_index(dirs.iter().map(String::as_str));
            Self {
                files: map,
                dirs,
                file_alias,
                dir_alias,
                entry: "index.html".to_string(),
                bad: HashSet::new(),
                cut: HashSet::new(),
            }
        }

        fn crawl(&self) -> Report {
            crawl(self, &Reporter::silent())
        }
    }

    impl Site for MemSite {
        fn file(&self, rel: &str) -> Option<String> {
            zip_pack::lookup_folded(rel, |name| self.files.contains_key(name), &self.file_alias)
        }

        fn is_dir(&self, rel: &str) -> bool {
            if rel.is_empty() {
                return true;
            }
            zip_pack::lookup_folded(rel, |name| self.dirs.contains(name), &self.dir_alias).is_some()
        }

        fn read(&self, rel: &str) -> Result<Loaded, String> {
            let stored = self
                .file(rel)
                .ok_or_else(|| format!("Impossible de lire {rel}."))?;
            if self.bad.contains(&stored) {
                return Err(format!("Impossible de lire {rel}."));
            }
            let text = self
                .files
                .get(&stored)
                .cloned()
                .ok_or_else(|| format!("Impossible de lire {rel}."))?;
            Ok(Loaded {
                text,
                truncated: self.cut.contains(&stored),
            })
        }

        fn file_count(&self) -> u64 {
            self.files.len() as u64
        }

        fn entry(&self) -> &str {
            &self.entry
        }
    }

    fn group<'a>(report: &'a Report, path: &str, reason: Reason) -> Option<&'a BrokenGroup> {
        report
            .broken
            .iter()
            .find(|item| item.path == path && item.reason == reason)
    }

    #[test]
    fn reports_missing_page_image_and_anchor() {
        let site = MemSite::new(&[(
            "index.html",
            r##"<a href="absent.html">x</a><img src="gone.png"><a href="#missing">y</a><h1 id="ok">ok</h1><a href="#ok">z</a>"##,
        )]);
        let report = site.crawl();
        assert!(
            group(&report, "absent.html", Reason::Missing).is_some(),
            "{report:?}"
        );
        assert!(
            group(&report, "gone.png", Reason::Missing).is_some(),
            "{report:?}"
        );
        assert!(
            group(&report, "index.html#missing", Reason::Anchor).is_some(),
            "{report:?}"
        );
        assert!(group(&report, "index.html#ok", Reason::Anchor).is_none());
        assert_eq!(report.broken_links, 3);
    }

    #[test]
    fn external_and_mailto_are_not_broken() {
        let site = MemSite::new(&[
            (
                "index.html",
                r#"<a href="https://ex.com/a">a</a><a href="mailto:a@b.c">m</a><a href="//cdn.example/x.js">c</a><img src="data:image/png;base64,aaaa"><script>var u = "https://evil.example/hidden";</script><script src="app.js"></script>"#,
            ),
            ("app.js", "var z = 'ghost.html';"),
        ]);
        let report = site.crawl();
        assert!(report.broken.is_empty(), "{:?}", report.broken);
        assert_eq!(report.external, 2);
        assert!(report.pages == ["index.html"]);
    }

    #[test]
    fn dot_dot_that_leaves_the_archive_is_broken() {
        let site = MemSite::new(&[("index.html", r#"<a href="../secret.html">x</a>"#)]);
        let report = site.crawl();
        assert!(
            group(&report, "../secret.html", Reason::Escape).is_some(),
            "{report:?}"
        );
        let html = render(&report, "site.zip");
        assert!(html.contains("hors de l’archive"), "{html}");
    }

    #[test]
    fn directory_and_parent_links_resolve_inside() {
        let site = MemSite::new(&[
            (
                "index.html",
                r##"<a href="docs/">d</a><a href="?q=1">q</a><a href="#">ici</a>"##,
            ),
            ("docs/index.html", r#"<a href="page.html">p</a>"#),
            (
                "docs/page.html",
                r#"<a href="../index.html">up</a><a href="/index.html">root</a>"#,
            ),
        ]);
        let report = site.crawl();
        assert!(report.broken.is_empty(), "{:?}", report.broken);
        assert_eq!(
            report.pages,
            ["index.html", "docs/index.html", "docs/page.html"]
        );
        let html = render(&report, "site.zip");
        assert!(html.contains("Aucun lien cassé"), "{html}");
        assert!(html.contains("site.zip"), "{html}");
    }

    #[test]
    fn srcset_style_and_css_urls_are_checked() {
        let site = MemSite::new(&[
            (
                "index.html",
                r#"<img src="ok.png" srcset="ok.png 1x, gone.png 2x"><div style="background: url('miss-inline.png')"></div><link rel="stylesheet" href="site.css">"#,
            ),
            ("ok.png", "png"),
            ("bg.png", "png"),
            (
                "site.css",
                r#"@import "https://cdn.example/a.css"; @import "css/extra.css"; body { background: url("bg.png"); } /* url("nope.png") */"#,
            ),
            ("css/extra.css", r#".a { background: url("miss.png"); }"#),
        ]);
        let report = site.crawl();
        assert!(
            group(&report, "gone.png", Reason::Missing).is_some(),
            "{report:?}"
        );
        assert!(
            group(&report, "css/miss.png", Reason::Missing).is_some(),
            "{report:?}"
        );
        assert!(
            group(&report, "miss-inline.png", Reason::Missing).is_some(),
            "{report:?}"
        );
        assert!(
            group(&report, "nope.png", Reason::Missing).is_none(),
            "{report:?}"
        );
        assert!(group(&report, "bg.png", Reason::Missing).is_none());
        assert_eq!(report.stylesheets, ["site.css", "css/extra.css"]);
        assert_eq!(report.external, 1);
    }

    #[test]
    fn ascii_case_and_nfc_do_not_break_a_stored_file() {
        let site = MemSite::new(&[
            (
                "index.html",
                "<img src=\"img/a.png\"><img src=\"cafe\u{0301}.png\">",
            ),
            ("Img/A.PNG", "x"),
            ("caf\u{00e9}.png", "x"),
        ]);
        let report = site.crawl();
        assert!(report.broken.is_empty(), "{:?}", report.broken);
        assert_eq!(report.reached, 3);
        assert_eq!(report.file_count, 3);
    }

    #[test]
    fn duplicate_missing_image_is_one_group() {
        let site = MemSite::new(&[
            (
                "index.html",
                r#"<a href="other.html">o</a><img src="missing.png"><img src="Missing.PNG">"#,
            ),
            ("other.html", r#"<img src="missing.png">"#),
        ]);
        let report = site.crawl();
        assert_eq!(report.broken.len(), 1, "{report:?}");
        let group = group(&report, "missing.png", Reason::Missing).unwrap();
        assert_eq!(group.source_count, 3);
        assert_eq!(group.sources.len(), 3);
        assert_eq!(report.broken_links, 3);
        assert_eq!(report.pages, ["index.html", "other.html"]);
    }

    #[test]
    fn truncated_page_does_not_fail_a_missing_anchor() {
        let mut site = MemSite::new(&[("index.html", r##"<a href="#bottom">bas</a>"##)]);
        site.cut.insert("index.html".to_string());
        let report = site.crawl();
        assert!(report.broken.is_empty(), "{:?}", report.broken);
        assert_eq!(report.truncated, ["index.html"]);
        let html = render(&report, "gros.zip");
        assert!(html.contains("Parcours incomplet"), "{html}");
        assert!(html.contains("Fichiers tronqués"), "{html}");
        assert!(
            html.contains("Les ancres de cette page ne sont pas vérifiées."),
            "{html}"
        );
        assert!(!html.contains("ancre absente"), "{html}");
        assert!(!html.contains("Aucun lien cassé"), "{html}");
    }

    #[test]
    fn remote_base_resolves_like_a_browser() {
        let base = with_base(&base_from_page("index.html"), "https://ex.com/sub/");
        match resolve_ref(&base, "a.html") {
            Resolved::External(url) => assert_eq!(url, "https://ex.com/sub/a.html"),
            other => panic!("relatif: {other:?}"),
        }
        match resolve_ref(&base, "/b.html") {
            Resolved::External(url) => assert_eq!(url, "https://ex.com/b.html"),
            other => panic!("racine: {other:?}"),
        }
        match resolve_ref(&base, "../x.html") {
            Resolved::External(url) => assert_eq!(url, "https://ex.com/x.html"),
            other => panic!("parent: {other:?}"),
        }
    }

    #[test]
    fn remote_base_makes_relative_links_external() {
        let site = MemSite::new(&[(
            "index.html",
            r#"<base href="https://ex.com/sub/"><a href="a.html">a</a><a href="/b.html">b</a><img src="c.png"><a href="mailto:a@b.c">m</a>"#,
        )]);
        let report = site.crawl();
        assert!(report.broken.is_empty(), "{:?}", report.broken);
        assert_eq!(report.external, 3, "{:?}", report.external);
    }

    #[test]
    fn local_base_resolves_under_that_directory() {
        let site = MemSite::new(&[
            (
                "index.html",
                r#"<base href="files/"><a href="a.html">a</a>"#,
            ),
            ("files/a.html", "<p id=\"here\">ok</p>"),
        ]);
        let report = site.crawl();
        assert!(report.broken.is_empty(), "{:?}", report.broken);
        assert_eq!(report.pages, ["index.html", "files/a.html"]);
    }

    #[test]
    fn zip_archive_reports_the_missing_page_and_visits_the_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("site.zip");
        let file = File::create(&path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        writer.start_file("index.html", opts).unwrap();
        writer
            .write_all(
                br#"<a href="ok.html">ok</a><a href="absent.html">no</a><img src="pic.png">"#,
            )
            .unwrap();
        writer.start_file("ok.html", opts).unwrap();
        writer.write_all(b"<p>ok</p>").unwrap();
        writer.start_file("pic.png", opts).unwrap();
        writer.write_all(b"PNG").unwrap();
        writer.finish().unwrap();

        let report = check_archive(&path, &Reporter::silent()).unwrap();
        assert!(
            group(&report, "absent.html", Reason::Missing).is_some(),
            "{report:?}"
        );
        assert_eq!(report.pages, ["index.html", "ok.html"]);
        assert_eq!(report.reached, 3);
        assert!(group(&report, "pic.png", Reason::Missing).is_none());
        let html = render(&report, "site.zip");
        assert!(html.contains("1 lien cassé"), "{html}");
        assert!(html.contains("ok.html"), "{html}");
    }

    #[test]
    fn markdown_zip_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.zip");
        let file = File::create(&path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        writer.start_file("page.md", opts).unwrap();
        writer.write_all(b"# Page").unwrap();
        writer.finish().unwrap();
        let err = check_archive(&path, &Reporter::silent()).unwrap_err();
        assert_eq!(
            err,
            "Cette archive est un dossier de notes, pas un site HTML."
        );
    }

    #[test]
    fn warc_uses_the_rewritten_site_and_the_generated_index() {
        let html = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><a href=\"absent.html\">x</a><img src=\"https://cdn.test/Figures/a.png\"></html>";
        let img = b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\n\r\nPNG";
        let rec = |uri: &str, body: &[u8]| {
            format!(
                "WARC/1.0\r\nWARC-Type: response\r\nWARC-Target-URI: {uri}\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .into_bytes()
            .into_iter()
            .chain(body.iter().copied())
            .chain(b"\r\n\r\n".iter().copied())
            .collect::<Vec<u8>>()
        };
        let warc = [
            rec("http://127.0.0.1:9/1.html", html),
            rec("https://cdn.test/Figures/a.png", img),
        ]
        .concat();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("livre.warc");
        fs::write(&path, warc).unwrap();

        let report = check_archive(&path, &Reporter::silent()).unwrap();
        assert_eq!(report.pages[0], "index.html");
        assert!(
            report.pages.iter().any(|page| page == "1.html"),
            "{report:?}"
        );
        assert!(
            group(&report, "absent.html", Reason::Missing).is_some(),
            "{report:?}"
        );
        assert!(
            report
                .broken
                .iter()
                .all(|group| !group.path.contains("figures/")),
            "{report:?}"
        );
        assert_eq!(report.external, 0);
        assert!(report.reached >= 3, "reached {}", report.reached);
    }

    #[test]
    fn other_schemes_are_not_archive_files() {
        let site = MemSite::new(&[
            (
                "sub/index.html",
                r#"<a href="http:foo.html">f</a><a href="HTTP:foo.html">F</a><iframe src="about:blank"></iframe><a href="file:///etc/passwd">p</a><a href="ftp://files.example/a">t</a><a href="https:foo">e</a><a href="foo:bar.html">n</a>"#,
            ),
            ("sub/foo.html", "<p>ok</p>"),
        ]);
        let mut site = site;
        site.entry = "sub/index.html".to_string();
        let report = site.crawl();
        assert!(report.broken.is_empty(), "{:?}", report.broken);
        assert_eq!(report.external, 1, "{:?}", report.external);
        assert_eq!(report.pages, ["sub/index.html", "sub/foo.html"]);
    }

    #[test]
    fn same_scheme_http_is_a_pack_path_and_https_is_external() {
        let base = base_from_page("sub/index.html");
        match resolve_ref(&base, "http:foo.html") {
            Resolved::Local { path, .. } => assert_eq!(path, "sub/foo.html"),
            other => panic!("{other:?}"),
        }
        match resolve_ref(&base, "http:/foo.html") {
            Resolved::Local { path, .. } => assert_eq!(path, "foo.html"),
            other => panic!("{other:?}"),
        }
        match resolve_ref(&base, "https:foo") {
            Resolved::External(url) => assert_eq!(url, "https://foo/"),
            other => panic!("{other:?}"),
        }
        match resolve_ref(&base, "https:foo/bar") {
            Resolved::External(url) => assert_eq!(url, "https://foo/bar"),
            other => panic!("{other:?}"),
        }
        match resolve_ref(&base, "about:blank") {
            Resolved::Skip => {}
            other => panic!("{other:?}"),
        }
        match resolve_ref(&base, r"img\bar.png") {
            Resolved::Local { path, .. } => assert_eq!(path, "sub/img/bar.png"),
            other => panic!("{other:?}"),
        }
        match resolve_ref(&base, "img%5Cbar.png") {
            Resolved::Local { path, .. } => assert_eq!(path, "sub/img\\bar.png"),
            other => panic!("{other:?}"),
        }
        match resolve_ref(&base, "http:\\\\ex.com\\a") {
            Resolved::External(url) => assert_eq!(url, "http://ex.com/a"),
            other => panic!("{other:?}"),
        }
        let remote = with_base(&base_from_page("index.html"), "https://ex.com/sub/");
        match resolve_ref(&remote, "https:a.html") {
            Resolved::External(url) => assert_eq!(url, "https://ex.com/sub/a.html"),
            other => panic!("{other:?}"),
        }
        match resolve_ref(&remote, "http:foo.html") {
            Resolved::External(url) => assert_eq!(url, "http://foo.html/"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn escaping_base_applies_to_the_links_that_follow() {
        let site = MemSite::new(&[
            ("index.html", r#"<a href="sub/">s</a>"#),
            (
                "sub/index.html",
                r#"<base href="../../out/"><img src="a.png"><a href="/index.html">home</a>"#,
            ),
            ("sub/a.png", "png"),
        ]);
        let report = site.crawl();
        assert!(
            group(&report, "../out/", Reason::Escape).is_some(),
            "{report:?}"
        );
        assert!(
            group(&report, "../out/a.png", Reason::Escape).is_some(),
            "{report:?}"
        );
        assert!(group(&report, "sub/a.png", Reason::Missing).is_none());
        assert!(group(&report, "index.html", Reason::Missing).is_none());
        assert_eq!(report.broken_links, 2);
    }

    #[test]
    fn srcset_does_not_split_a_data_url() {
        let site = MemSite::new(&[(
            "index.html",
            r#"<img srcset="data:image/png;base64,iVBORw0KGgo 1x, gone.png 2x"><img src="data:image/png;base64,aaaa">"#,
        )]);
        let report = site.crawl();
        assert!(
            group(&report, "gone.png", Reason::Missing).is_some(),
            "{report:?}"
        );
        assert_eq!(report.broken.len(), 1, "{report:?}");
        assert!(
            report
                .broken
                .iter()
                .all(|group| !group.path.contains("iVBOR")),
            "{report:?}"
        );
    }

    #[test]
    fn backslash_matches_the_slashed_file() {
        let site = MemSite::new(&[
            (
                "index.html",
                "<img src=\"img\\bar.png\"><link rel=\"stylesheet\" href=\"site.css\">",
            ),
            ("img/bar.png", "png"),
            ("site.css", r"body { background: url(img\plain.png); }"),
            ("img/plain.png", "png"),
        ]);
        let report = site.crawl();
        assert!(report.broken.is_empty(), "{:?}", report.broken);
        assert_eq!(report.reached, 4);
    }

    #[test]
    fn slashless_directory_url_resolves_like_the_browser() {
        let site = MemSite::new(&[
            ("index.html", r#"<a href="docs">d</a>"#),
            ("docs/index.html", r#"<a href="page.html">p</a>"#),
            ("docs/page.html", "<p>ok</p>"),
        ]);
        let report = site.crawl();
        let missing = group(&report, "page.html", Reason::Missing).unwrap();
        assert_eq!(missing.sources.len(), 1);
        assert_eq!(missing.sources[0].page, "docs");
        assert_eq!(missing.sources[0].href, "page.html");
        assert!(!report.pages.iter().any(|page| page == "docs/page.html"));
        assert!(report.pages.iter().any(|page| page == "docs/index.html"));
    }

    #[test]
    fn truncated_stylesheet_mentions_unchecked_addresses() {
        let mut site = MemSite::new(&[
            ("index.html", r#"<link rel="stylesheet" href="big.css">"#),
            ("big.css", r#"body { background: url("past.png"); }"#),
        ]);
        site.cut.insert("big.css".to_string());
        let report = site.crawl();
        assert_eq!(report.truncated, ["big.css"]);
        let html = render(&report, "gros.zip");
        assert!(html.contains("Fichiers tronqués"), "{html}");
        assert!(
            html.contains("Les adresses au-delà ne sont pas vérifiées."),
            "{html}"
        );
        assert!(
            !html.contains("Les ancres de cette page ne sont pas vérifiées."),
            "{html}"
        );
    }

    #[test]
    fn an_older_report_does_not_replace_a_newer_one() {
        let store = ReportStore::default();
        let first = store.begin();
        let second = store.begin();
        assert!(!store.publish(first, "ancien".into()));
        assert!(store.is_current(second));
        assert!(!store.is_current(first));
        assert!(store.publish(second, "récent".into()));
        assert_eq!(store.get().unwrap(), "récent");
    }
}
