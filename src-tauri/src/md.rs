//! Lecteur d’un dossier de fichiers Markdown.
//!
//! Pas de page d’entrée HTML : l’accueil est une recherche sur le nom des
//! fichiers, et chaque `.md` est rendu en HTML. Les liens `/page/Slug` des
//! exports Grokipedia ouvrent `Slug_grokipedia.md` (à défaut `_wikipedia.md`).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use pulldown_cmark::{html, CowStr, Event, Options, Parser, Tag};

const STYLE: &str = r#"
:root { color-scheme: light dark; }
body { font: 1.05rem/1.55 Georgia, "Iowan Old Style", serif; max-width: 42rem; margin: 2rem auto; padding: 0 1.2rem; }
nav, form, .meta, .dirs { font-family: ui-sans-serif, system-ui, sans-serif; font-size: 0.92rem; }
h1, h2, h3 { line-height: 1.2; }
label { display: block; margin: 1rem 0 0.4rem; }
input[type=search] { width: 100%; box-sizing: border-box; padding: 0.5rem 0.6rem; font: inherit; }
button { margin-top: 0.6rem; font: inherit; }
a { color: inherit; }
.meta { opacity: 0.75; }
article { overflow-wrap: anywhere; }
pre { background: rgba(127,127,127,0.12); padding: 0.8rem 1rem; overflow: auto; }
code { font-family: ui-monospace, SFMono-Regular, monospace; font-size: 0.9em; }
"#;

const SEARCH_LIMIT: usize = 40;
const LIST_LIMIT: usize = 400;

struct Doc {
    rel: String,
    name_lower: String,
}

pub(crate) struct MdIndex {
    docs: Vec<Doc>,
    /// slug normalisé → (rang, chemin relatif). Le rang préfère Grokipedia.
    /// À rang égal, le chemin le plus court gagne.
    pages: HashMap<String, (u8, String)>,
    subdirs: Vec<String>,
    /// Basenames en minuscules présents plus d’une fois.
    dup_names: HashSet<String>,
}

pub(crate) enum Reply {
    Html {
        status: u16,
        body: String,
    },
    File(PathBuf),
    /// Chemin relatif servi depuis un zip, sans extraction.
    Zip(String),
}

pub(crate) fn contains_markdown(dir: &Path) -> bool {
    let Ok(root) = dir.canonicalize() else {
        return false;
    };
    if !root.is_dir() {
        return false;
    }
    fn walk(root: &Path, dir: &Path, seen: &mut HashSet<PathBuf>) -> bool {
        if !seen.insert(dir.to_path_buf()) {
            return false;
        }
        let Ok(rd) = fs::read_dir(dir) else {
            return false;
        };
        for ent in rd.flatten() {
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if skip(&name) {
                continue;
            }
            match classify(root, dir, &ent, seen) {
                Child::Dir(child) => {
                    if walk(root, &child, seen) {
                        return true;
                    }
                }
                Child::File if is_md(&name) => return true,
                Child::File | Child::Skip => {}
            }
        }
        false
    }
    walk(&root, &root, &mut HashSet::new())
}

/// Descend un dossier enveloppe (un seul sous-dossier, aucun fichier).
pub(crate) fn markdown_root(dir: &Path) -> PathBuf {
    let Ok(root) = dir.canonicalize() else {
        return dir.to_path_buf();
    };
    fn descend(root: &Path, dir: &Path, seen: &mut HashSet<PathBuf>) -> PathBuf {
        if !seen.insert(dir.to_path_buf()) {
            return dir.to_path_buf();
        }
        let Ok(rd) = fs::read_dir(dir) else {
            return dir.to_path_buf();
        };
        let mut only = None;
        let mut dirs = 0usize;
        for ent in rd.flatten() {
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if skip(&name) {
                continue;
            }
            match classify(root, dir, &ent, seen) {
                Child::Dir(child) => {
                    dirs += 1;
                    if dirs == 1 {
                        only = Some(child);
                    } else {
                        only = None;
                    }
                }
                Child::File => return dir.to_path_buf(),
                Child::Skip => {}
            }
        }
        if dirs == 1 {
            if let Some(sub) = only {
                return descend(root, &sub, seen);
            }
        }
        dir.to_path_buf()
    }
    descend(&root, &root, &mut HashSet::new())
}

/// Un symlink n’est suivi que s’il reste sous `root` et n’est pas déjà visité.
/// Les fichiers ordinaires ne sont pas canonicalisés : un corpus de centaines
/// de milliers de notes ne doit pas payer un `realpath` par fichier.
fn classify(root: &Path, dir: &Path, ent: &fs::DirEntry, seen: &HashSet<PathBuf>) -> Child {
    let Ok(ft) = ent.file_type() else {
        return Child::Skip;
    };
    if ft.is_symlink() {
        let Ok(canon) = ent.path().canonicalize() else {
            return Child::Skip;
        };
        if !canon.starts_with(root) || seen.contains(&canon) {
            return Child::Skip;
        }
        if canon.is_dir() {
            Child::Dir(canon)
        } else {
            Child::File
        }
    } else if ft.is_dir() {
        let child = dir.join(ent.file_name());
        if seen.contains(&child) {
            Child::Skip
        } else {
            Child::Dir(child)
        }
    } else {
        Child::File
    }
}

enum Child {
    Dir(PathBuf),
    File,
    Skip,
}

impl MdIndex {
    pub(crate) fn build(root: &Path) -> Self {
        Self::build_with(root, &mut |_| {})
    }

    /// Comme [`build`](Self::build). `bump` reçoit le nombre de notes déjà vues.
    pub(crate) fn build_with(root: &Path, bump: &mut dyn FnMut(u64)) -> Self {
        let mut index = Self {
            docs: Vec::new(),
            pages: HashMap::new(),
            subdirs: Vec::new(),
            dup_names: HashSet::new(),
        };
        let mut notes = 0u64;
        if let Ok(root_canon) = root.canonicalize() {
            let mut seen = HashSet::new();
            seen.insert(root_canon.clone());
            index.walk(&root_canon, &root_canon, "", &mut seen, &mut notes, bump);
        }
        bump(notes);
        index.finish()
    }

    /// Index construit depuis des chemins déjà connus, sans lire les fichiers.
    pub(crate) fn from_rels(rels: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self::from_rels_with(rels, None, &mut |_, _| {})
    }

    /// `bump(vus, notes)` pendant le parcours. `total` est le nombre de chemins attendus.
    pub(crate) fn from_rels_with(
        rels: impl IntoIterator<Item = impl AsRef<str>>,
        total: Option<u64>,
        bump: &mut dyn FnMut(u64, u64),
    ) -> Self {
        let mut index = Self {
            docs: Vec::new(),
            pages: HashMap::new(),
            subdirs: Vec::new(),
            dup_names: HashSet::new(),
        };
        let mut subdirs = HashSet::new();
        let mut seen = 0u64;
        let mut notes = 0u64;
        for rel in rels {
            seen += 1;
            let rel = rel.as_ref();
            if !rel.split('/').any(skip) {
                if let Some(name) = Path::new(rel).file_name().and_then(|n| n.to_str()) {
                    if is_md(name) {
                        if let Some((dir, _)) = rel.split_once('/') {
                            if !dir.is_empty() {
                                subdirs.insert(dir.to_string());
                            }
                        }
                        index.add_md(name, rel.to_string());
                        notes += 1;
                    }
                }
            }
            if seen % 1024 == 0 || total.is_some_and(|total| seen >= total) {
                bump(seen, notes);
            }
        }
        bump(seen, notes);
        index.subdirs = subdirs.into_iter().collect();
        index.finish()
    }

    /// Ajoute des dossiers de premier niveau qui ne contiennent aucune note.
    pub(crate) fn include_top_dirs<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for name in names {
            let name = name.as_ref();
            if name.is_empty() || name.contains('/') || self.subdirs.iter().any(|have| have == name)
            {
                continue;
            }
            self.subdirs.push(name.to_string());
        }
        self.subdirs.sort();
    }

    fn finish(mut self) -> Self {
        self.docs.sort_by(|a, b| {
            a.name_lower
                .cmp(&b.name_lower)
                .then_with(|| a.rel.cmp(&b.rel))
        });
        self.subdirs.sort();
        self.dup_names = duplicate_basenames(&self.docs);
        self
    }

    fn walk(
        &mut self,
        root: &Path,
        dir: &Path,
        prefix: &str,
        seen: &mut HashSet<PathBuf>,
        notes: &mut u64,
        bump: &mut dyn FnMut(u64),
    ) {
        let Ok(rd) = fs::read_dir(dir) else {
            return;
        };
        for ent in rd.flatten() {
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if skip(&name) {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            match classify(root, dir, &ent, seen) {
                Child::Dir(child) => {
                    if !seen.insert(child.clone()) {
                        continue;
                    }
                    if prefix.is_empty() {
                        self.subdirs.push(name.to_string());
                    }
                    self.walk(root, &child, &rel, seen, notes, bump);
                }
                Child::File if is_md(&name) => {
                    self.add_md(&name, rel);
                    *notes += 1;
                    bump(*notes);
                }
                Child::File | Child::Skip => {}
            }
        }
    }

    fn add_md(&mut self, name: &str, rel: String) {
        if let Some(slug) = article_slug(name) {
            let rank = source_rank(name);
            let replace = match self.pages.get(&slug) {
                Some((old_rank, old_rel)) => {
                    *old_rank < rank || (*old_rank == rank && shorter_rel(&rel, old_rel))
                }
                None => true,
            };
            if replace {
                self.pages.insert(slug, (rank, rel.clone()));
            }
        }
        self.docs.push(Doc {
            name_lower: name.to_lowercase(),
            rel,
        });
    }

    fn page(&self, slug: &str) -> Option<&str> {
        self.pages
            .get(&normalize_slug(slug))
            .map(|(_, rel)| rel.as_str())
    }
}

/// Arborescence servie : un dossier sur disque, ou un zip lu sans extraction.
pub(crate) trait Store {
    fn label(&self) -> String;
    fn is_file(&self, rel: &str) -> bool;
    fn is_dir(&self, rel: &str) -> bool;
    fn read_text(&self, rel: &str) -> String;
    fn list(&self, rel: &str) -> (Vec<String>, Vec<String>);
    fn open_file(&self, rel: &str) -> Reply;
}

struct Disk<'a> {
    root: &'a Path,
}

impl Store for Disk<'_> {
    fn label(&self) -> String {
        self.root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Markdown".into())
    }

    fn is_file(&self, rel: &str) -> bool {
        under_root(self.root, &self.root.join(rel)).is_some_and(|path| path.is_file())
    }

    fn is_dir(&self, rel: &str) -> bool {
        directory_inside(self.root, &self.root.join(rel))
    }

    fn read_text(&self, rel: &str) -> String {
        let Some(path) = under_root(self.root, &self.root.join(rel)) else {
            return String::new();
        };
        fs::read_to_string(path).unwrap_or_default()
    }

    fn list(&self, rel: &str) -> (Vec<String>, Vec<String>) {
        let dir = self.root.join(rel);
        let root_canon = self.root.canonicalize().ok();
        let here = dir.canonicalize().ok();
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        let Ok(rd) = fs::read_dir(&dir) else {
            return (dirs, files);
        };
        for ent in rd.flatten() {
            let name = ent.file_name();
            let name = name.to_string_lossy().into_owned();
            if skip(&name) {
                continue;
            }
            let Ok(ft) = ent.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                let Some(root_canon) = root_canon.as_ref() else {
                    continue;
                };
                let Ok(canon) = ent.path().canonicalize() else {
                    continue;
                };
                if !canon.starts_with(root_canon) || here.as_ref().is_some_and(|h| &canon == h) {
                    continue;
                }
                if canon.is_dir() {
                    dirs.push(name);
                } else if is_md(&name) {
                    files.push(name);
                }
            } else if ft.is_dir() {
                dirs.push(name);
            } else if is_md(&name) {
                files.push(name);
            }
        }
        (dirs, files)
    }

    fn open_file(&self, rel: &str) -> Reply {
        match under_root(self.root, &self.root.join(rel)) {
            Some(path) => Reply::File(path),
            None => html(403, shell("Interdit", "<p>Chemin refusé.</p>")),
        }
    }
}

/// Fichier `.md` source d’une URL du lecteur. L’accueil, une recherche et un
/// dossier n’ont pas de fichier. `/page/Slug` renvoie le `.md` choisi par l’index.
pub(crate) fn markdown_source(root: &Path, index: &MdIndex, url_path: &str) -> Option<String> {
    markdown_source_in(&Disk { root }, index, url_path)
}

pub(crate) fn markdown_source_in(
    store: &dyn Store,
    index: &MdIndex,
    url_path: &str,
) -> Option<String> {
    let url_path = url_path.split(['?', '#']).next().unwrap_or(url_path);
    let path = percent_decode(url_path);
    let rel = path.trim_start_matches('/').trim_end_matches('/');
    if !rel.is_empty()
        && rel
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return None;
    }
    if rel.is_empty() || rel == "index.html" || rel == "index.htm" {
        return None;
    }
    if let Some(slug) = rel.strip_prefix("page/") {
        let file_rel = index.page(slug)?;
        return store.is_file(file_rel).then(|| file_rel.to_string());
    }
    let name = Path::new(rel)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if !is_md(name) {
        return None;
    }
    store.is_file(rel).then(|| rel.to_string())
}

pub(crate) fn dispatch(root: &Path, index: &MdIndex, url: &str) -> Reply {
    dispatch_in(&Disk { root }, index, url)
}

pub(crate) fn dispatch_in(store: &dyn Store, index: &MdIndex, url: &str) -> Reply {
    let (url_path, query) = match url.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (url, None),
    };
    let path = percent_decode(url_path);
    let rel = path.trim_start_matches('/').trim_end_matches('/');
    if !rel.is_empty()
        && rel
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return html(403, shell("Interdit", "<p>Chemin refusé.</p>"));
    }

    let q = query.and_then(|q| query_value(q, "q"));
    if rel.is_empty() || rel == "index.html" {
        let folder = store.label();
        return html(200, home_html(&folder, index, q.as_deref().unwrap_or("")));
    }

    if let Some(slug) = rel.strip_prefix("page/") {
        let slug = percent_decode(slug);
        if let Some(rel) = index.page(&slug) {
            return article(store, rel, index);
        }
        let key = normalize_slug(&slug);
        let search = if key.is_empty() { slug.as_str() } else { &key };
        return html(
            404,
            shell(
                "Introuvable",
                &format!(
                    "<p>Pas d’article pour <code>{}</code>.</p><p class=\"nav\"><a href=\"/?q={}\">Rechercher</a></p>",
                    escape(&slug),
                    encode_component(search)
                ),
            ),
        );
    }

    if store.is_dir(rel) {
        let (dirs, files) = store.list(rel);
        return html(200, dir_html(rel, dirs, files));
    }
    if store.is_file(rel) {
        let name = Path::new(rel)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if is_md(name) {
            return article(store, rel, index);
        }
        return store.open_file(rel);
    }
    html(
        404,
        shell(
            "Introuvable",
            "<p>Introuvable.</p><p class=\"nav\"><a href=\"/\">Accueil</a></p>",
        ),
    )
}

fn article(store: &dyn Store, rel: &str, index: &MdIndex) -> Reply {
    let md = store.read_text(rel);
    let fallback = Path::new(rel)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| rel.to_string());
    let title = title_of(&md, &fallback);
    let rendered = render_markdown(&md, index);
    let name = Path::new(rel)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| rel.to_string());
    html(
        200,
        shell(
            &title,
            &format!(
                "<p class=\"nav\"><a href=\"/\">Accueil</a></p><p class=\"meta\">{}</p><article>{}</article>",
                escape(&name),
                rendered
            ),
        ),
    )
}

fn home_html(folder: &str, index: &MdIndex, query: &str) -> String {
    let mut body = format!(
        "<h1>{}</h1>\
         <p class=\"meta\">{}. La recherche porte sur le nom du fichier.</p>\
         <form action=\"/\" method=\"get\">\
         <label for=\"q\">Rechercher</label>\
         <input id=\"q\" type=\"search\" name=\"q\" value=\"{}\" autofocus>\
         <button type=\"submit\">Rechercher</button>\
         </form>",
        escape(folder),
        articles_label(index.docs.len()),
        escape(query)
    );

    let q = query.trim();
    if q.is_empty() && index.docs.len() <= LIST_LIMIT {
        body.push_str("<ul>");
        for doc in &index.docs {
            body.push_str(&format!(
                "<li><a href=\"/{}\">{}</a></li>",
                encode_rel(&doc.rel),
                escape(&link_label(index, doc))
            ));
        }
        body.push_str("</ul>");
    } else if q.is_empty() || q.chars().count() < 2 {
        body.push_str("<p>Indiquez au moins deux lettres.</p>");
    } else {
        let needle = q.to_lowercase();
        let mut shown = Vec::new();
        let mut total = 0usize;
        for doc in &index.docs {
            if doc.name_lower.contains(&needle) {
                total += 1;
                if shown.len() < SEARCH_LIMIT {
                    shown.push(doc);
                }
            }
        }
        if total == 0 {
            body.push_str(&format!(
                "<p>Aucun article dont le nom contient « {} ».</p>",
                escape(q)
            ));
        } else {
            body.push_str("<h2>Résultats</h2>");
            if total > shown.len() {
                body.push_str(&format!(
                    "<p class=\"meta\">{} affichés sur {}.</p>",
                    shown.len(),
                    group_digits(total)
                ));
            } else {
                body.push_str(&format!("<p class=\"meta\">{}</p>", group_digits(total)));
            }
            body.push_str("<ul>");
            for doc in shown {
                body.push_str(&format!(
                    "<li><a href=\"/{}\">{}</a></li>",
                    encode_rel(&doc.rel),
                    escape(&link_label(index, doc))
                ));
            }
            body.push_str("</ul>");
        }
    }

    if !index.subdirs.is_empty() {
        body.push_str("<h2>Dossiers</h2><ul class=\"dirs\">");
        for name in &index.subdirs {
            body.push_str(&format!(
                "<li><a href=\"/{}/\">{}</a></li>",
                encode_component(name),
                escape(name)
            ));
        }
        body.push_str("</ul>");
    }
    shell(folder, &body)
}

fn dir_html(rel: &str, mut dirs: Vec<String>, mut files: Vec<String>) -> String {
    dirs.sort();
    files.sort();
    let extra = files.len().saturating_sub(LIST_LIMIT);
    files.truncate(LIST_LIMIT);

    let title = Path::new(rel)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| rel.to_string());
    let mut body = format!(
        "<p class=\"nav\"><a href=\"/\">Accueil</a></p><h1>{}</h1>",
        escape(&title)
    );
    if dirs.is_empty() && files.is_empty() {
        body.push_str("<p>Ce dossier ne contient pas d’article.</p>");
    }
    if !dirs.is_empty() || !files.is_empty() {
        body.push_str("<ul>");
        for name in dirs {
            let child = format!("{rel}/{name}");
            body.push_str(&format!(
                "<li><a href=\"/{}/\">{}/</a></li>",
                encode_rel(&child),
                escape(&name)
            ));
        }
        for name in files {
            let child = format!("{rel}/{name}");
            body.push_str(&format!(
                "<li><a href=\"/{}\">{}</a></li>",
                encode_rel(&child),
                escape(&name)
            ));
        }
        body.push_str("</ul>");
    }
    if extra > 0 {
        body.push_str(&format!(
            "<p class=\"meta\">{} articles de plus. Passez par la recherche de l’accueil.</p>",
            group_digits(extra)
        ));
    }
    shell(&title, &body)
}

fn render_markdown(md: &str, index: &MdIndex) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, options);
    let events = parser.map(|event| rewrite_event(event, index));
    let mut out = String::new();
    html::push_html(&mut out, events);
    out
}

fn rewrite_event<'a>(event: Event<'a>, index: &MdIndex) -> Event<'a> {
    match event {
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: mapped_dest(dest_url, index),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: mapped_dest(dest_url, index),
            title,
            id,
        }),
        Event::Html(html) => Event::Text(html),
        Event::InlineHtml(html) => Event::Text(html),
        other => other,
    }
}

fn mapped_dest<'a>(dest: CowStr<'a>, index: &MdIndex) -> CowStr<'a> {
    if let Some(rewritten) = rewrite_page(&dest, index) {
        return rewritten;
    }
    if dest_allowed(&dest) {
        dest
    } else {
        CowStr::from("#")
    }
}

/// Réécrit `/page/Slug`. `None` si la cible n’est pas une page : l’appelant
/// décide alors si l’URL d’origine peut rester.
fn rewrite_page(dest: &str, index: &MdIndex) -> Option<CowStr<'static>> {
    let (path, frag) = match dest.split_once('#') {
        Some((path, frag)) => (path, Some(frag)),
        None => (dest, None),
    };
    let slug = path.strip_prefix("/page/")?;
    if slug.is_empty() {
        return None;
    }
    let slug = percent_decode(slug);
    if let Some(rel) = index.page(&slug) {
        let mut url = format!("/{}", encode_rel(rel));
        if let Some(frag) = frag {
            url.push('#');
            url.push_str(frag);
        }
        return Some(CowStr::from(url));
    }
    let key = normalize_slug(&slug);
    if key.is_empty() {
        return Some(CowStr::from("#"));
    }
    Some(CowStr::from(format!("/?q={}", encode_component(&key))))
}

fn dest_allowed(dest: &str) -> bool {
    let dest = dest.trim();
    if dest.is_empty() || dest.starts_with('#') {
        return true;
    }
    if dest.starts_with("//") {
        return false;
    }
    match scheme_of(dest) {
        Some(scheme) => scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"),
        None => true,
    }
}

fn scheme_of(dest: &str) -> Option<&str> {
    let bytes = dest.as_bytes();
    if !bytes.first()?.is_ascii_alphabetic() {
        return None;
    }
    let mut i = 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.' {
            i += 1;
            continue;
        }
        if b == b':' {
            return Some(&dest[..i]);
        }
        return None;
    }
    None
}

fn title_of(md: &str, fallback: &str) -> String {
    for line in md.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if let Some(rest) = line.strip_prefix("# ") {
            let rest = rest.trim();
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
        if !line.is_empty() {
            break;
        }
    }
    fallback.to_string()
}

fn article_slug(file_name: &str) -> Option<String> {
    let lower = file_name.to_lowercase();
    let stem = lower.strip_suffix(".md")?;
    let stem = stem
        .strip_suffix("_grokipedia")
        .or_else(|| stem.strip_suffix("_wikipedia"))
        .unwrap_or(stem);
    Some(normalize_slug(stem))
}

fn source_rank(file_name: &str) -> u8 {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with("_grokipedia.md") {
        2
    } else if lower.ends_with("_wikipedia.md") {
        1
    } else {
        0
    }
}

/// Minuscules, puis chaque suite de caractères hors `[a-z0-9]` devient un
/// seul `-`, bornes comprises. Idempotent. Appliquer après avoir retiré
/// `_grokipedia` / `_wikipedia`, sinon le suffixe se fond dans le slug.
fn normalize_slug(slug: &str) -> String {
    let slug = slug.trim().trim_matches('/');
    let mut out = String::with_capacity(slug.len());
    let mut hyphen = false;
    for c in slug.chars() {
        for c in c.to_lowercase() {
            if c.is_ascii_alphanumeric() {
                out.push(c);
                hyphen = false;
            } else if !hyphen && !out.is_empty() {
                out.push('-');
                hyphen = true;
            }
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    out
}

fn shorter_rel(candidate: &str, current: &str) -> bool {
    let depth = |s: &str| s.bytes().filter(|b| *b == b'/').count();
    match depth(candidate).cmp(&depth(current)) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => candidate < current,
    }
}

fn duplicate_basenames(docs: &[Doc]) -> HashSet<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for doc in docs {
        *counts.entry(doc.name_lower.as_str()).or_default() += 1;
    }
    counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(name, _)| name.to_string())
        .collect()
}

fn link_label(index: &MdIndex, doc: &Doc) -> String {
    if index.dup_names.contains(&doc.name_lower) {
        doc.rel.clone()
    } else {
        Path::new(&doc.rel)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| doc.rel.clone())
    }
}

fn directory_inside(root: &Path, candidate: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(candidate) else {
        return false;
    };
    let ft = meta.file_type();
    if !ft.is_symlink() {
        return ft.is_dir();
    }
    let (Ok(root), Ok(canon)) = (root.canonicalize(), candidate.canonicalize()) else {
        return false;
    };
    canon.is_dir() && canon.starts_with(&root)
}

pub(crate) fn under_root(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let path = candidate.canonicalize().ok()?;
    if path.starts_with(&root) {
        Some(path)
    } else {
        None
    }
}

fn html(status: u16, body: String) -> Reply {
    Reply::Html { status, body }
}

fn shell(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html><html lang=\"fr\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src 'self' http: https:; form-action 'self'; base-uri 'none'\">\
         <title>{}</title><style>{}</style></head><body>{}</body></html>",
        escape(title),
        STYLE,
        body
    )
}

fn skip(name: &str) -> bool {
    name == "__MACOSX" || name == ".DS_Store" || name.starts_with('.')
}

fn is_md(name: &str) -> bool {
    name.len() > 3 && name.as_bytes()[name.len() - 3..].eq_ignore_ascii_case(b".md")
}

fn articles_label(n: usize) -> String {
    if n == 1 {
        "1 article".into()
    } else {
        format!("{} articles", group_digits(n))
    }
}

fn group_digits(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('\u{00a0}');
        }
        out.push(c);
    }
    out
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn encode_rel(rel: &str) -> String {
    rel.split('/')
        .map(encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub(crate) fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_value(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(k) == key {
            return Some(percent_decode(&v.replace('+', " ")));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (tempfile::TempDir, MdIndex) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.md"), b"# Alpha\n\nHello").unwrap();
        fs::write(dir.path().join("beta.md"), b"# Beta\n").unwrap();
        fs::write(
            dir.path().join("Freemasonry_grokipedia.md"),
            b"# Lodge\n\nSee [magic](/page/Ceremonial_magic) and [missing](/page/Nope_nope).\n\n<script>alert(1)</script>",
        )
        .unwrap();
        fs::write(
            dir.path().join("Ceremonial-magic_wikipedia.md"),
            b"# Wiki\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("Ceremonial-magic_grokipedia.md"),
            b"# Grok magic\n",
        )
        .unwrap();
        let nested = dir.path().join("Science");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("Zinc_grokipedia.md"), b"# Zinc\n").unwrap();
        let index = MdIndex::build(dir.path());
        (dir, index)
    }

    #[test]
    fn build_reports_the_note_count() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.md"), b"# A").unwrap();
        fs::write(dir.path().join("b.txt"), b"pas une note").unwrap();
        fs::write(dir.path().join("c.md"), b"# C").unwrap();
        let mut counts = Vec::new();
        let index = MdIndex::build_with(dir.path(), &mut |n| counts.push(n));
        assert_eq!(index.docs.len(), 2);
        assert_eq!(counts.last().copied(), Some(2));
    }

    #[test]
    fn search_lists_matching_names_only() {
        let (dir, index) = sample();
        let Reply::Html { status, body } = dispatch(dir.path(), &index, "/?q=alp") else {
            panic!("html");
        };
        assert_eq!(status, 200);
        assert!(body.contains("alpha.md"), "{body}");
        assert!(!body.contains("beta.md"), "{body}");
    }

    #[test]
    fn short_query_does_not_list() {
        let (dir, index) = sample();
        let Reply::Html { body, .. } = dispatch(dir.path(), &index, "/?q=a") else {
            panic!("html");
        };
        assert!(body.contains("deux lettres"), "{body}");
        assert!(!body.contains("alpha.md"), "{body}");
    }

    #[test]
    fn page_slug_prefers_grokipedia_and_renders() {
        let (dir, index) = sample();
        let Reply::Html { status, body } = dispatch(dir.path(), &index, "/page/Ceremonial_magic")
        else {
            panic!("html");
        };
        assert_eq!(status, 200);
        assert!(body.contains("Grok magic"), "{body}");
        assert!(!body.contains("<h1>Wiki</h1>"), "{body}");
        assert!(body.contains("<h1>Grok magic</h1>"), "{body}");
    }

    #[test]
    fn markdown_links_rewrite_and_html_is_text() {
        let (dir, index) = sample();
        let Reply::Html { body, .. } = dispatch(dir.path(), &index, "/Freemasonry_grokipedia.md")
        else {
            panic!("html");
        };
        assert!(body.contains("/Ceremonial-magic_grokipedia.md"), "{body}");
        assert!(body.contains("href=\"/?q=nope-nope\""), "{body}");
        assert!(!body.contains("<script>"), "{body}");
        assert!(body.contains("&lt;script&gt;"), "{body}");
        assert!(body.contains("Content-Security-Policy"), "{body}");
    }

    #[test]
    fn slug_folds_punctuation_like_the_filename() {
        assert_eq!(normalize_slug("Austin,_Texas"), "austin-texas");
        assert_eq!(normalize_slug("Bachelor's_degree"), "bachelor-s-degree");
        assert_eq!(
            normalize_slug("Call_of_Duty:_Black_Ops"),
            "call-of-duty-black-ops"
        );
        assert_eq!(normalize_slug("austin-texas"), "austin-texas");
        assert_eq!(normalize_slug("___"), "");
        assert_eq!(
            normalize_slug(&normalize_slug("Austin,_Texas")),
            "austin-texas"
        );
        assert_eq!(
            article_slug("Austin-Texas_grokipedia.md").as_deref(),
            Some("austin-texas")
        );
        assert_eq!(
            article_slug("Bachelor-s-degree_wikipedia.md").as_deref(),
            Some("bachelor-s-degree")
        );
        assert_eq!(article_slug("Note_grokipedia.md").as_deref(), Some("note"));
    }

    #[test]
    fn page_slug_with_punctuation_opens_the_hyphenated_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Austin-Texas_grokipedia.md"), b"# Austin\n").unwrap();
        fs::write(
            dir.path().join("Bachelor-s-degree_grokipedia.md"),
            b"# Degree\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("Call-of-Duty-Black-Ops_grokipedia.md"),
            b"# Duty\n",
        )
        .unwrap();
        let index = MdIndex::build(dir.path());
        for (url, needle) in [
            ("/page/Austin,_Texas", "Austin"),
            ("/page/Bachelor's_degree", "Degree"),
            ("/page/Call_of_Duty:_Black_Ops", "Duty"),
        ] {
            let Reply::Html { status, body } = dispatch(dir.path(), &index, url) else {
                panic!("html");
            };
            assert_eq!(status, 200, "{url} {body}");
            assert!(body.contains(needle), "{url} {body}");
        }
        let Reply::Html { status, body } = dispatch(dir.path(), &index, "/page/Nope,_Nope") else {
            panic!("html");
        };
        assert_eq!(status, 404);
        assert!(body.contains("href=\"/?q=nope-nope\""), "{body}");
        assert!(body.contains("Nope,_Nope"), "{body}");
    }

    #[test]
    fn same_rank_prefers_shorter_path_and_lists_the_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("E_grokipedia.md"), b"# Root E\n").unwrap();
        let science = dir.path().join("Science");
        fs::create_dir(&science).unwrap();
        fs::write(science.join("E_grokipedia.md"), b"# Nested E\n").unwrap();
        fs::write(dir.path().join("Rank_wikipedia.md"), b"# Wiki rank\n").unwrap();
        fs::write(science.join("Rank_grokipedia.md"), b"# Grok rank\n").unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        fs::write(a.join("Z_grokipedia.md"), b"# A Z\n").unwrap();
        fs::write(b.join("Z_grokipedia.md"), b"# B Z\n").unwrap();
        let index = MdIndex::build(dir.path());

        let Reply::Html { body, .. } = dispatch(dir.path(), &index, "/page/E") else {
            panic!("html");
        };
        assert!(body.contains("Root E"), "{body}");
        assert!(!body.contains("Nested E"), "{body}");

        let Reply::Html { body, .. } = dispatch(dir.path(), &index, "/page/Rank") else {
            panic!("html");
        };
        assert!(body.contains("Grok rank"), "{body}");
        assert!(!body.contains("Wiki rank"), "{body}");

        let Reply::Html { body, .. } = dispatch(dir.path(), &index, "/page/Z") else {
            panic!("html");
        };
        assert!(body.contains("A Z"), "{body}");
        assert!(!body.contains("B Z"), "{body}");

        let Reply::Html { body, .. } = dispatch(dir.path(), &index, "/?q=e_grok") else {
            panic!("html");
        };
        assert!(body.contains("Science/E_grokipedia.md"), "{body}");
        let Reply::Html { body, .. } = dispatch(dir.path(), &index, "/") else {
            panic!("html");
        };
        assert!(body.contains("Science/E_grokipedia.md"), "{body}");
    }

    #[test]
    fn unsafe_urls_are_neutralized() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("note.md"),
            b"# N\n\n[x](javascript:alert(1)) [y](data:text/html,hi) [z](//evil.example/a) [ok](https://example.com/a) [rel](other.md)\n\n![i](javascript:alert(2))\n",
        )
        .unwrap();
        let index = MdIndex::build(dir.path());
        let Reply::Html { body, .. } = dispatch(dir.path(), &index, "/note.md") else {
            panic!("html");
        };
        let lower = body.to_lowercase();
        assert!(!lower.contains("javascript:"), "{body}");
        assert!(!body.contains("data:"), "{body}");
        assert!(!body.contains("//evil"), "{body}");
        assert!(body.contains("href=\"#\""), "{body}");
        assert!(body.contains("src=\"#\""), "{body}");
        assert!(body.contains("https://example.com/a"), "{body}");
        assert!(body.contains("other.md"), "{body}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cycle_and_outside_link_are_not_indexed() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("note.md"), b"# Note\n").unwrap();
        symlink(".", dir.path().join("loop")).unwrap();
        let nested = dir.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("inner.md"), b"# Inner\n").unwrap();
        symlink("..", nested.join("up")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.md"), b"Secret-outside").unwrap();
        symlink(outside.path(), dir.path().join("escape")).unwrap();

        assert!(contains_markdown(dir.path()));
        let index = MdIndex::build(dir.path());
        assert!(index.docs.iter().any(|d| d.rel == "note.md"));
        assert!(index.docs.iter().any(|d| d.rel == "nested/inner.md"));
        assert!(!index.docs.iter().any(|d| d.rel.contains("secret")));
        assert!(!index.subdirs.iter().any(|s| s == "escape" || s == "loop"));

        let Reply::Html { status, body } = dispatch(dir.path(), &index, "/escape/") else {
            panic!("html");
        };
        assert_ne!(status, 200, "{body}");
        assert!(!body.contains("Secret-outside"), "{body}");

        let Reply::Html { status, body } = dispatch(dir.path(), &index, "/escape/secret.md") else {
            panic!("html");
        };
        assert_ne!(status, 200, "{body}");
        assert!(!body.contains("Secret-outside"), "{body}");

        let Reply::Html { body, .. } = dispatch(dir.path(), &index, "/nested/up/") else {
            panic!("html");
        };
        assert!(!body.contains("Secret-outside"), "{body}");
        assert!(
            !body.contains(">escape<") && !body.contains(">escape/<"),
            "{body}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_only_loop_is_not_markdown() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        symlink(".", dir.path().join("loop")).unwrap();
        assert!(!contains_markdown(dir.path()));
    }

    #[test]
    fn dotdot_is_refused() {
        let (dir, index) = sample();
        let Reply::Html { status, .. } = dispatch(dir.path(), &index, "/../secret") else {
            panic!("html");
        };
        assert_eq!(status, 403);
    }
}
