//! Servir un pack web en HTTP local (sans API Tauri).
//!
//! Un dossier de site est servi sur place. Un zip est lu par son catalogue,
//! sans extraction. Un fichier `.sqlite` de corpus est lu en place. Un WACZ
//! est extrait dans un dossier temporaire, supprimé à l’arrêt du serveur.

use std::collections::HashSet;
use std::fs::{self, File};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use tiny_http::{Header, Response, Server, StatusCode};

use crate::droit::DroitPack;
use crate::progress::{self, Reporter};
use crate::zip_pack::ZipPack;

const ENTRY_NAMES: &[&str] = &["OUVRIR.html", "ouvrir.html", "index.html", "index.htm"];

pub struct PackServer {
    pub port: u16,
    pub entry: String,
    pub title: String,
    pub root: PathBuf,
    pub markdown: bool,
    /// Rempli par le thread du serveur avant d’accepter les requêtes, si le pack est Markdown.
    pub index: Arc<OnceLock<crate::md::MdIndex>>,
    /// Présent quand le pack est un zip servi sans extraction. `root` est alors le fichier zip.
    pub zip: Option<Arc<ZipPack>>,
    /// Présent quand le pack est un corpus sqlite. `root` est alors le fichier.
    pub droit: Option<Arc<DroitPack>>,
    stop: Arc<AtomicBool>,
}

impl PackServer {
    #[cfg(test)]
    pub fn open(pack_path: &Path) -> Result<Self, String> {
        Self::open_with(pack_path, &Reporter::silent())
    }

    pub fn open_with(pack_path: &Path, progress: &Reporter) -> Result<Self, String> {
        let pack_path = resolve_input(pack_path)?;
        let title = pack_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "pack".into());

        let Prepared {
            root,
            entry,
            temp,
            markdown,
            zip,
            droit,
            index: built,
        } = prepare_with(&pack_path, progress)?;

        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let server = Server::from_listener(listener, None).map_err(|e| format!("http: {e}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let index = Arc::new(OnceLock::new());
        if let Some(built) = built {
            let _ = index.set(built);
        }
        let index_t = index.clone();
        let root_t = root.clone();
        let zip_t = zip.clone();
        let droit_t = droit.clone();
        thread::Builder::new()
            .name(format!("taurus-{port}"))
            .spawn(move || {
                serve(server, root_t, stop_t, markdown, index_t, zip_t, droit_t);
                drop(temp);
            })
            .map_err(|e| format!("thread: {e}"))?;

        Ok(Self {
            port,
            entry,
            title,
            root,
            markdown,
            index,
            zip,
            droit,
            stop,
        })
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/{}", self.port, self.entry)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

pub(crate) fn is_web_archive(p: &Path) -> bool {
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.ends_with(".wacz") || name.ends_with(".warc") || name.ends_with(".warc.gz")
}

pub(crate) fn is_zip(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

pub(crate) fn is_sqlite(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("sqlite"))
}

/// Dossier, archive, ou zip que l’accueil peut ouvrir.
pub(crate) fn is_openable(path: &Path) -> bool {
    if path.is_dir() {
        is_site_dir(path)
            || find_archive_in_dir(path).is_some()
            || crate::md::contains_markdown(path)
    } else {
        is_web_archive(path) || is_zip(path) || is_sqlite(path)
    }
}

pub(crate) fn is_site_dir(path: &Path) -> bool {
    path.is_dir() && site_root(path).is_ok()
}

/// Fichier tel quel, dossier de site, ou archive trouvée dans un dossier de collection.
///
/// Un dossier qui a déjà une page d’entrée est servi lui-même, même s’il
/// contient aussi un `.wacz` ou un `.zip`.
pub(crate) fn resolve_input(path: &Path) -> Result<PathBuf, String> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    if path.is_dir() {
        if is_site_dir(path) {
            return Ok(path.to_path_buf());
        }
        if let Some(archive) = find_archive_in_dir(path) {
            return Ok(archive);
        }
        if crate::md::contains_markdown(path) {
            return Ok(path.to_path_buf());
        }
        return Err(
            "Pas de OUVRIR.html, index.html, Markdown, .wacz, .warc ni .zip dans le dossier."
                .into(),
        );
    }
    Err(format!("{} introuvable.", path.display()))
}

struct Prepared {
    root: PathBuf,
    entry: String,
    temp: Option<tempfile::TempDir>,
    markdown: bool,
    zip: Option<Arc<ZipPack>>,
    droit: Option<Arc<DroitPack>>,
    /// Index Markdown déjà construit, pour ne pas le refaire au premier GET.
    index: Option<crate::md::MdIndex>,
}

/// Prépare la racine servie. `TempDir` n’est présent que pour une archive extraite.
fn prepare_with(path: &Path, progress: &Reporter) -> Result<Prepared, String> {
    if path.is_dir() {
        if let Ok((root, entry)) = site_root(path) {
            return Ok(Prepared {
                root,
                entry,
                temp: None,
                markdown: false,
                zip: None,
                droit: None,
                index: None,
            });
        }
        if crate::md::contains_markdown(path) {
            let root = crate::md::markdown_root(path);
            let mut notes = 0u64;
            let built = crate::md::MdIndex::build_with(&root, &mut |count| {
                notes = count;
                progress.tick("Index des notes", count, None, progress::notes(count));
            });
            progress.force("Index des notes", None, progress::notes(notes));
            return Ok(Prepared {
                root,
                entry: "index.html".into(),
                temp: None,
                markdown: true,
                zip: None,
                droit: None,
                index: Some(built),
            });
        }
        return Err(
            "Pas de OUVRIR.html, index.html, Markdown, .wacz, .warc ni .zip dans le dossier."
                .into(),
        );
    }
    if is_web_archive(path) {
        progress.force("Extraction…", None, String::new());
        let dir = tempfile::tempdir().map_err(|e| format!("temp: {e}"))?;
        let site = crate::wacz::materialize(path, dir.path())?;
        let (root, entry) = site_root(&site)?;
        return Ok(Prepared {
            root,
            entry,
            temp: Some(dir),
            markdown: false,
            zip: None,
            droit: None,
            index: None,
        });
    }
    if is_zip(path) {
        let pack = ZipPack::open_with(path, progress)?;
        let entry = pack.entry.clone();
        let markdown = pack.markdown;
        let index = if markdown {
            Some(pack.md_index_with(progress))
        } else {
            None
        };
        return Ok(Prepared {
            root: path.to_path_buf(),
            entry,
            temp: None,
            markdown,
            zip: Some(Arc::new(pack)),
            droit: None,
            index,
        });
    }
    if is_sqlite(path) {
        let pack = DroitPack::open(path, progress)?;
        return Ok(Prepared {
            root: path.to_path_buf(),
            entry: String::new(),
            temp: None,
            markdown: false,
            zip: None,
            droit: Some(Arc::new(pack)),
            index: None,
        });
    }
    Err("Dossier de site, .zip, .wacz, .warc ou .sqlite de corpus attendu.".into())
}

pub(crate) fn find_archive_in_dir(dir: &Path) -> Option<PathBuf> {
    if let Some(name) = dir.file_name() {
        let named = dir.join(format!("{}.wacz", name.to_string_lossy()));
        if named.is_file() {
            return Some(named);
        }
    }

    let mut wacz = Vec::new();
    let mut warc = Vec::new();
    let mut zip = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for ent in rd.flatten() {
            let p = ent.path();
            if !p.is_file() {
                continue;
            }
            let n = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if n.ends_with(".wacz") {
                wacz.push(p);
            } else if n.ends_with(".warc.gz") || n.ends_with(".warc") {
                warc.push(p);
            } else if n.ends_with(".zip") {
                zip.push(p);
            }
        }
    }
    wacz.sort();
    if let Some(p) = wacz.into_iter().next() {
        return Some(p);
    }

    let archive = dir.join("archive");
    if archive.is_dir() {
        let mut recs = Vec::new();
        if let Ok(rd) = fs::read_dir(&archive) {
            for ent in rd.flatten() {
                let p = ent.path();
                let n = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if n.ends_with(".warc.gz") || n.ends_with(".warc") {
                    recs.push(p);
                }
            }
        }
        recs.sort();
        if let Some(p) = recs.into_iter().next() {
            return Some(p);
        }
    }

    warc.sort();
    if let Some(p) = warc.into_iter().next() {
        return Some(p);
    }
    zip.sort();
    zip.into_iter().next()
}

/// Chemin relatif du fichier qu’une URL HTML serait servie, ou `None` s’il
/// sort de la racine (lien symbolique compris) ou n’existe pas.
pub(crate) fn html_source(root: &Path, url_path: &str) -> Option<String> {
    let url_path = url_path.split(['?', '#']).next().unwrap_or(url_path);
    let rel = crate::md::percent_decode(url_path.trim_start_matches('/').trim_end_matches('/'));
    if !rel.is_empty()
        && rel
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return None;
    }
    let joined = resolve_served_path(root, &rel)?;
    let canon = joined.canonicalize().ok()?;
    let root_c = root.canonicalize().ok()?;
    if !canon.starts_with(&root_c) || !canon.is_file() {
        return None;
    }
    let rel_path = canon.strip_prefix(&root_c).ok()?;
    let text = rel_path.to_string_lossy().replace('\\', "/");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn resolve_served_path(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = rel.trim_start_matches('/').trim_end_matches('/');
    let joined = if rel.is_empty() {
        root.join("index.html")
    } else {
        root.join(rel)
    };
    if joined.is_file() {
        return Some(joined);
    }
    let index = joined.join("index.html");
    if index.is_file() {
        return Some(index);
    }
    None
}

pub(crate) fn unzip(zip_path: &Path, dest: &Path) -> Result<(), String> {
    let file = File::open(zip_path).map_err(|e| format!("zip introuvable: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("zip illisible: {e}"))?;
    archive
        .extract(dest)
        .map_err(|e| format!("extraction: {e}"))?;
    Ok(())
}

fn skip_name(name: &str) -> bool {
    name == "__MACOSX" || name == ".DS_Store" || name.starts_with('.')
}

fn site_root(extracted: &Path) -> Result<(PathBuf, String), String> {
    let root = extracted.canonicalize().map_err(|e| e.to_string())?;
    site_root_at(&root, &root, &mut HashSet::new())
}

fn site_root_at(
    root: &Path,
    dir: &Path,
    seen: &mut HashSet<PathBuf>,
) -> Result<(PathBuf, String), String> {
    if !seen.insert(dir.to_path_buf()) {
        return Err("Pas de OUVRIR.html ni index.html.".into());
    }
    for name in ENTRY_NAMES {
        if dir.join(name).is_file() {
            return Ok((dir.to_path_buf(), (*name).to_string()));
        }
    }

    // Un fichier quelconque (hors entrée) exclut le dossier enveloppe.
    // On s’arrête au premier, pour ne pas charger des centaines de milliers
    // de noms quand le dossier est une bibliothèque Markdown.
    // Un symlink n’est une enveloppe que s’il reste sous la racine et n’a pas
    // déjà été visité : `ln -s . loop` ne doit pas boucler.
    let mut only_dir: Option<PathBuf> = None;
    let mut dir_count = 0usize;
    for ent in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let ent = ent.map_err(|e| e.to_string())?;
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if skip_name(&name) {
            continue;
        }
        let ft = ent.file_type().map_err(|e| e.to_string())?;
        if ft.is_symlink() {
            let Ok(canon) = ent.path().canonicalize() else {
                continue;
            };
            if canon.is_dir() {
                if canon.starts_with(root) && !seen.contains(&canon) {
                    dir_count += 1;
                    if dir_count == 1 {
                        only_dir = Some(canon);
                    } else {
                        only_dir = None;
                    }
                }
                continue;
            }
            return Err("Pas de OUVRIR.html ni index.html.".into());
        }
        if ft.is_dir() {
            dir_count += 1;
            if dir_count == 1 {
                only_dir = Some(dir.join(ent.file_name()));
            } else {
                only_dir = None;
            }
            continue;
        }
        return Err("Pas de OUVRIR.html ni index.html.".into());
    }

    if dir_count == 1 {
        if let Some(sub) = only_dir {
            return site_root_at(root, &sub, seen);
        }
    }

    Err("Pas de OUVRIR.html ni index.html.".into())
}

pub(crate) fn content_type(path: &Path) -> String {
    let guessed = mime_guess::from_path(path).first_or_octet_stream();
    let essence = guessed.essence_str();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let textual = essence.starts_with("text/")
        || essence == "application/javascript"
        || essence == "application/json"
        || essence == "application/xml"
        || essence.ends_with("+xml")
        || matches!(
            ext.as_str(),
            "c" | "h" | "cc" | "hh" | "cpp" | "hpp" | "cxx" | "hxx" | "md" | "csv" | "svg" | "mjs"
        );
    if textual {
        format!("{essence}; charset=utf-8")
    } else {
        essence.to_string()
    }
}

fn serve(
    server: Server,
    root: PathBuf,
    stop: Arc<AtomicBool>,
    markdown: bool,
    index: Arc<OnceLock<crate::md::MdIndex>>,
    zip: Option<Arc<ZipPack>>,
    droit: Option<Arc<DroitPack>>,
) {
    if markdown && index.get().is_none() && droit.is_none() {
        let built = if let Some(zip) = &zip {
            zip.md_index()
        } else {
            crate::md::MdIndex::build(&root)
        };
        let _ = index.set(built);
    }
    let md_index = index.get();
    while !stop.load(Ordering::SeqCst) {
        let req = match server.recv_timeout(Duration::from_millis(250)) {
            Ok(Some(r)) => r,
            Ok(None) | Err(_) => continue,
        };

        if let Some(droit) = &droit {
            droit.respond(req);
            continue;
        }

        if let Some(zip) = &zip {
            if let Some(index) = &md_index {
                match crate::md::dispatch_in(zip.as_ref(), index, req.url()) {
                    crate::md::Reply::Html { status, body } => respond_html(req, status, body),
                    crate::md::Reply::Zip(rel) => zip.respond_rel(req, &rel),
                    crate::md::Reply::File(_) => respond_status(req, 404, "introuvable"),
                }
            } else {
                zip.respond_url(req);
            }
            continue;
        }

        if let Some(index) = &md_index {
            match crate::md::dispatch(&root, index, req.url()) {
                crate::md::Reply::Html { status, body } => respond_html(req, status, body),
                crate::md::Reply::File(path) => send_file(req, &root, &path),
                crate::md::Reply::Zip(_) => respond_status(req, 404, "introuvable"),
            }
            continue;
        }

        // Le zip décode déjà l’URL. Un dossier doit le faire aussi : le lecteur
        // encode les espaces et les « & » des noms (`%20`, `%26`).
        let url_path = req.url().split(['?', '#']).next().unwrap_or("/");
        let decoded = crate::md::percent_decode(url_path);
        let rel = decoded.trim_start_matches('/').trim_end_matches('/');
        if !rel.is_empty()
            && (rel.contains('\0')
                || rel.contains('\\')
                || rel
                    .split('/')
                    .any(|seg| seg.is_empty() || seg == "." || seg == ".."))
        {
            let _ =
                req.respond(Response::from_string("interdit").with_status_code(StatusCode(403)));
            continue;
        }
        let Some(joined) = resolve_served_path(&root, rel) else {
            let _ =
                req.respond(Response::from_string("introuvable").with_status_code(StatusCode(404)));
            continue;
        };
        send_file(req, &root, &joined);
    }
}

fn respond_status(req: tiny_http::Request, status: u16, body: &str) {
    let _ = req.respond(Response::from_string(body).with_status_code(StatusCode(status)));
}

fn respond_html(req: tiny_http::Request, status: u16, body: String) {
    let mut response = Response::from_data(body.into_bytes()).with_status_code(StatusCode(status));
    if let Ok(h) = Header::from_bytes(b"Content-Type", b"text/html; charset=utf-8") {
        response = response.with_header(h);
    }
    if let Ok(h) = Header::from_bytes(b"X-Content-Type-Options", b"nosniff") {
        response = response.with_header(h);
    }
    if let Ok(h) = Header::from_bytes(b"Cache-Control", b"no-store") {
        response = response.with_header(h);
    }
    let _ = req.respond(response);
}

fn send_file(req: tiny_http::Request, root: &Path, joined: &Path) {
    let Ok(canon) = joined.canonicalize() else {
        let _ = req.respond(Response::from_string("introuvable").with_status_code(StatusCode(404)));
        return;
    };
    let Ok(root_c) = root.canonicalize() else {
        let _ = req.respond(Response::from_string("erreur").with_status_code(StatusCode(500)));
        return;
    };
    if !canon.starts_with(&root_c) || !canon.is_file() {
        let _ = req.respond(Response::from_string("interdit").with_status_code(StatusCode(403)));
        return;
    }

    let mime = content_type(&canon);
    match File::open(&canon) {
        Ok(file) => {
            let mut response = Response::from_file(file);
            if let Ok(h) = Header::from_bytes(b"Content-Type", mime.as_bytes()) {
                response = response.with_header(h);
            }
            if let Ok(h) = Header::from_bytes(b"X-Content-Type-Options", b"nosniff") {
                response = response.with_header(h);
            }
            if let Ok(h) = Header::from_bytes(b"Cache-Control", b"no-store") {
                response = response.with_header(h);
            }
            let _ = req.respond(response);
        }
        Err(_) => {
            let _ =
                req.respond(Response::from_string("introuvable").with_status_code(StatusCode(404)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_directory_index() {
        let dir = tempfile::tempdir().unwrap();
        let blog = dir.path().join("blog");
        std::fs::create_dir(&blog).unwrap();
        std::fs::write(dir.path().join("index.html"), b"home").unwrap();
        std::fs::write(blog.join("index.html"), b"blog").unwrap();

        let home = resolve_served_path(dir.path(), "").unwrap();
        assert_eq!(std::fs::read_to_string(home).unwrap(), "home");
        let slash = resolve_served_path(dir.path(), "blog/").unwrap();
        assert_eq!(std::fs::read_to_string(slash).unwrap(), "blog");
        let noslash = resolve_served_path(dir.path(), "blog").unwrap();
        assert_eq!(std::fs::read_to_string(noslash).unwrap(), "blog");
    }

    #[test]
    fn finds_wacz_in_collection_dir() {
        let dir = tempfile::tempdir().unwrap();
        let col = dir.path().join("example");
        std::fs::create_dir(&col).unwrap();
        let wacz = col.join("example.wacz");
        std::fs::write(&wacz, b"pk").unwrap();
        assert_eq!(find_archive_in_dir(&col).as_deref(), Some(wacz.as_path()));
        assert_eq!(resolve_input(&col).unwrap(), wacz);
    }

    #[test]
    fn site_directory_wins_over_archive_inside() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"live").unwrap();
        std::fs::write(dir.path().join("example.wacz"), b"pk").unwrap();
        assert_eq!(resolve_input(dir.path()).unwrap(), dir.path());
    }

    #[test]
    fn directory_without_entry_or_archive_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"hi").unwrap();
        let err = resolve_input(dir.path()).unwrap_err();
        assert!(err.contains("index.html"), "{err}");
    }

    #[test]
    fn markdown_directory_opens_a_reader() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.md"), b"# Note\n\nBonjour").unwrap();
        let server = PackServer::open(dir.path()).unwrap();
        assert_eq!(server.entry, "index.html");
        let (status, body) = http_get(server.port, "/index.html");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("note.md"), "{body}");
        let (status, body) = http_get(server.port, "/note.md");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("<h1>Note</h1>"), "{body}");
        assert!(body.contains("Bonjour"), "{body}");
        server.stop();
    }

    #[test]
    fn archive_wins_over_markdown_readme() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("readme.md"), b"# hi").unwrap();
        let wacz = dir.path().join("col.wacz");
        std::fs::write(&wacz, b"pk").unwrap();
        assert_eq!(resolve_input(dir.path()).unwrap(), wacz);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_wrapper_loop_errors() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        symlink(".", dir.path().join("loop")).unwrap();
        let err = resolve_input(dir.path()).unwrap_err();
        assert!(err.contains("index.html"), "{err}");
    }

    #[test]
    fn markdown_wrapper_is_unwrapped() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("notes");
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(inner.join("page.md"), b"# Page\n").unwrap();
        let server = PackServer::open(dir.path()).unwrap();
        let (status, body) = http_get(server.port, "/page.md");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("<h1>Page</h1>"), "{body}");
        server.stop();
    }

    #[test]
    fn live_directory_serves_edits_and_keeps_files() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("index.html");
        std::fs::write(&index, b"v1").unwrap();
        std::fs::write(dir.path().join("topic.md"), b"# topic").unwrap();

        let server = PackServer::open(dir.path()).unwrap();
        assert_eq!(server.entry, "index.html");
        let (status, body) = http_get(server.port, "/index.html");
        assert_eq!(status, 200);
        assert_eq!(body, "v1");

        std::fs::write(&index, b"v2").unwrap();
        let (status, body) = http_get(server.port, "/");
        assert_eq!(status, 200);
        assert_eq!(body, "v2");

        let (status, body) = http_get(server.port, "/topic.md");
        assert_eq!(status, 200);
        assert_eq!(body, "# topic");

        let secret_name = format!("taurus-secret-{}", std::process::id());
        let secret = dir.path().parent().unwrap().join(&secret_name);
        std::fs::write(&secret, b"hidden").unwrap();
        let (status, body) = http_get(server.port, &format!("/../{secret_name}"));
        assert_eq!(status, 403, "{body}");
        assert!(!body.contains("hidden"));
        std::fs::remove_file(&secret).unwrap();

        server.stop();
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(std::fs::read_to_string(&index).unwrap(), "v2");
    }

    #[test]
    fn static_folder_decodes_percent_encoded_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"home").unwrap();
        std::fs::write(dir.path().join("A B.html"), b"space").unwrap();
        std::fs::write(dir.path().join("A&B.html"), b"amp").unwrap();
        std::fs::write(dir.path().join("A&Eacute;NIEN.html"), b"entity").unwrap();
        std::fs::write(dir.path().join("café.html"), b"accent").unwrap();
        let server = PackServer::open(dir.path()).unwrap();

        let (status, body) = http_get(server.port, "/");
        assert_eq!(status, 200, "{body}");
        assert_eq!(body, "home");

        let (status, body) = http_get(server.port, "/A%20B.html");
        assert_eq!(status, 200, "{body}");
        assert_eq!(body, "space");

        let (status, body) = http_get(server.port, "/A%26B.html");
        assert_eq!(status, 200, "{body}");
        assert_eq!(body, "amp");

        let (status, body) = http_get(server.port, "/A%26Eacute%3BNIEN.html");
        assert_eq!(status, 200, "{body}");
        assert_eq!(body, "entity");

        let (status, body) = http_get(server.port, "/caf%C3%A9.html");
        assert_eq!(status, 200, "{body}");
        assert_eq!(body, "accent");

        let (status, body) = http_get(server.port, "/%2e%2e%2fsecret");
        assert_eq!(status, 403, "{body}");
        assert_eq!(body, "interdit");

        server.stop();
    }

    #[test]
    fn live_directory_unwraps_single_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("site");
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(inner.join("index.html"), b"inner").unwrap();
        let server = PackServer::open(dir.path()).unwrap();
        let (status, body) = http_get(server.port, "/");
        assert_eq!(status, 200);
        assert_eq!(body, "inner");
        server.stop();
    }

    #[test]
    fn zip_serves_without_extracting() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("cours.zip");
        let file = File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts = SimpleFileOptions::default();
        writer.start_file("index.html", opts).unwrap();
        writer
            .write_all(b"from-zip<script>nope()</script>")
            .unwrap();
        writer.start_file("native/a.css", opts).unwrap();
        writer.write_all(b"keep").unwrap();
        writer.start_file("site/OUVRIR.html", opts).unwrap();
        writer.write_all(b"wrapped").unwrap();
        writer.start_file("site/a.css", opts).unwrap();
        writer.write_all(b"css").unwrap();
        writer.start_file("__MACOSX/._OUVRIR.html", opts).unwrap();
        writer.write_all(b"junk").unwrap();
        writer.finish().unwrap();

        // index.html à la racine gagne : le dossier site/ n’est pas la racine.
        let server = PackServer::open(&zip_path).unwrap();
        assert!(server.zip.is_some());
        assert!(!server.markdown);
        let (status, body) = http_get(server.port, "/");
        assert_eq!(status, 200);
        assert_eq!(body, "from-zip<script>nope()</script>");
        let (status, body) = http_get(server.port, "/site/OUVRIR.html");
        assert_eq!(status, 200);
        assert_eq!(body, "wrapped");
        let (status, _) = http_get(server.port, "/../cours.zip");
        assert_eq!(status, 403);
        server.stop();

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|ent| ent.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("cours.zip")]);
    }

    #[test]
    fn zip_unwraps_a_single_directory() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("native.zip");
        let file = File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts = SimpleFileOptions::default();
        writer.start_file("__MACOSX/._index.html", opts).unwrap();
        writer.write_all(b"junk").unwrap();
        writer.start_file("site/OUVRIR.html", opts).unwrap();
        writer.write_all(b"wrapped").unwrap();
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        writer.start_file("site/a.css", stored).unwrap();
        writer.write_all(b"css").unwrap();
        writer.finish().unwrap();

        let server = PackServer::open(&zip_path).unwrap();
        assert_eq!(server.entry, "OUVRIR.html");
        let (status, body) = http_get(server.port, "/OUVRIR.html");
        assert_eq!(status, 200);
        assert_eq!(body, "wrapped");
        let (status, body) = http_get(server.port, "/a.css");
        assert_eq!(status, 200);
        assert_eq!(body, "css");
        server.stop();
    }

    #[test]
    fn markdown_zip_renders_without_extracting() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("notes.zip");
        let file = File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts = SimpleFileOptions::default();
        writer.start_file("notes/page.md", opts).unwrap();
        writer.write_all(b"# Page\n\nBonjour").unwrap();
        writer.finish().unwrap();

        let server = PackServer::open(&zip_path).unwrap();
        assert!(server.markdown);
        let (status, body) = http_get(server.port, "/");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("page.md"), "{body}");
        let (status, body) = http_get(server.port, "/page.md");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("<h1>Page</h1>"), "{body}");
        assert!(body.contains("Bonjour"), "{body}");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|ent| ent.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("notes.zip")]);
        server.stop();
    }

    #[test]
    fn zip_opens_an_entry_that_only_differs_by_case() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("site.zip");
        let file = File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts = SimpleFileOptions::default();
        writer.start_file("site/Ouvrir.HTML", opts).unwrap();
        writer.write_all(b"ouvrir").unwrap();
        writer.start_file("site/photo.JPG", opts).unwrap();
        writer.write_all(b"pixels").unwrap();
        writer.start_file("site/img/Photo.JPG", opts).unwrap();
        writer.write_all(b"nested").unwrap();
        writer.start_file("__MACOSX/._Ouvrir.HTML", opts).unwrap();
        writer.write_all(b"junk").unwrap();
        writer.finish().unwrap();

        let server = PackServer::open(&zip_path).unwrap();
        assert_eq!(server.entry, "Ouvrir.HTML");
        for path in ["/Ouvrir.HTML", "/ouvrir.html", "/OUVRIR.html"] {
            let (status, body) = http_get(server.port, path);
            assert_eq!(status, 200, "{path} {body}");
            assert_eq!(body, "ouvrir", "{path}");
        }
        for path in ["/photo.jpg", "/photo.JPG"] {
            let (status, body) = http_get(server.port, path);
            assert_eq!(status, 200, "{path} {body}");
            assert_eq!(body, "pixels", "{path}");
        }
        let (status, body) = http_get(server.port, "/img/photo.jpg");
        assert_eq!(status, 200, "{body}");
        assert_eq!(body, "nested");
        let zip = server.zip.as_ref().unwrap();
        assert_eq!(
            zip.locate("/photo.jpg", false, None).as_deref(),
            Some("photo.JPG")
        );
        server.stop();
    }

    #[test]
    fn zip_exact_name_wins_when_case_collides() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("both.zip");
        let file = File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts = SimpleFileOptions::default();
        writer.start_file("index.html", opts).unwrap();
        writer.write_all(b"lower").unwrap();
        writer.start_file("Index.html", opts).unwrap();
        writer.write_all(b"mixed").unwrap();
        writer.finish().unwrap();

        let server = PackServer::open(&zip_path).unwrap();
        assert_eq!(server.entry, "index.html");
        let (status, body) = http_get(server.port, "/index.html");
        assert_eq!((status, body.as_str()), (200, "lower"));
        let (status, body) = http_get(server.port, "/Index.html");
        assert_eq!((status, body.as_str()), (200, "mixed"));
        let (status, body) = http_get(server.port, "/INDEX.HTML");
        assert_eq!((status, body.as_str()), (200, "lower"));
        server.stop();

        let tie_path = dir.path().join("tie.zip");
        let file = File::create(&tie_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer.start_file("INDEX.HTML", opts).unwrap();
        writer.write_all(b"upper").unwrap();
        writer.start_file("Index.html", opts).unwrap();
        writer.write_all(b"mixed").unwrap();
        writer.finish().unwrap();

        let server = PackServer::open(&tie_path).unwrap();
        assert_eq!(server.entry, "INDEX.HTML");
        let (status, body) = http_get(server.port, "/index.html");
        assert_eq!((status, body.as_str()), (200, "upper"));
        let (status, body) = http_get(server.port, "/Index.html");
        assert_eq!((status, body.as_str()), (200, "mixed"));
        let (status, body) = http_get(server.port, "/INDEX.HTML");
        assert_eq!((status, body.as_str()), (200, "upper"));
        server.stop();
    }

    #[test]
    fn zip_serves_nfd_names_from_nfc_urls() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let nfd_file = "caf\u{0065}\u{0301}.png";
        let nfd_dir = "caf\u{0065}\u{0301}/a.png";
        assert_ne!(nfd_file, "caf\u{00e9}.png");

        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("accents.zip");
        let file = File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts = SimpleFileOptions::default();
        writer.start_file("index.html", opts).unwrap();
        writer.write_all(b"home").unwrap();
        writer.start_file(nfd_file, opts).unwrap();
        writer.write_all(b"png").unwrap();
        writer.start_file(nfd_dir, opts).unwrap();
        writer.write_all(b"dirpng").unwrap();
        writer.finish().unwrap();

        let server = PackServer::open(&zip_path).unwrap();
        for path in ["/caf%C3%A9.png", "/cafe%CC%81.png", "/Caf%C3%A9.png"] {
            let (status, body) = http_get(server.port, path);
            assert_eq!(status, 200, "{path} {body}");
            assert_eq!(body, "png", "{path}");
        }
        let (status, body) = http_get(server.port, "/caf%C3%A9/a.png");
        assert_eq!(status, 200, "{body}");
        assert_eq!(body, "dirpng");
        server.stop();
    }

    #[test]
    fn markdown_zip_lists_an_image_only_folder() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("notes");
        std::fs::create_dir_all(folder.join("notes")).unwrap();
        std::fs::create_dir_all(folder.join("figures")).unwrap();
        std::fs::create_dir_all(folder.join("__MACOSX")).unwrap();
        std::fs::write(folder.join("notes/page.md"), b"# Page\n\nBonjour").unwrap();
        std::fs::write(folder.join("figures/a.png"), b"png").unwrap();
        std::fs::write(folder.join("__MACOSX/._page.md"), b"junk").unwrap();

        let disk = PackServer::open(&folder).unwrap();
        let (status, disk_body) = http_get(disk.port, "/");
        assert_eq!(status, 200, "{disk_body}");
        assert!(disk_body.contains("href=\"/figures/\""), "{disk_body}");
        assert!(!disk_body.contains("__MACOSX"), "{disk_body}");
        disk.stop();

        let zip_path = dir.path().join("notes.zip");
        let file = File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts = SimpleFileOptions::default();
        writer.start_file("notes/page.md", opts).unwrap();
        writer.write_all(b"# Page\n\nBonjour").unwrap();
        writer.start_file("figures/a.png", opts).unwrap();
        writer.write_all(b"png").unwrap();
        writer.start_file("__MACOSX/notes/._page.md", opts).unwrap();
        writer.write_all(b"junk").unwrap();
        writer.start_file(".hidden/secret.md", opts).unwrap();
        writer.write_all(b"# Secret").unwrap();
        writer.finish().unwrap();

        let server = PackServer::open(&zip_path).unwrap();
        assert!(server.markdown);
        let (status, body) = http_get(server.port, "/");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("href=\"/figures/\""), "{body}");
        assert!(body.contains("href=\"/notes/\""), "{body}");
        assert!(body.contains("page.md"), "{body}");
        assert!(!body.contains("__MACOSX"), "{body}");
        assert!(!body.contains("secret"), "{body}");
        let (status, body) = http_get(server.port, "/Notes/Page.md");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("<h1>Page</h1>"), "{body}");
        let (status, body) = http_get(server.port, "/Figures/");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("ne contient pas"), "{body}");
        let zip = server.zip.as_ref().unwrap();
        let index = server.index.get().unwrap();
        assert_eq!(
            zip.locate("/Notes/Page.md", true, Some(index)).as_deref(),
            Some("Notes/Page.md")
        );
        assert!(zip.load_text("Notes/Page.md").unwrap().contains("# Page"));
        server.stop();
    }

    #[test]
    fn opening_reports_zip_catalog_and_note_index() {
        use std::io::Write;
        use std::sync::Mutex;
        use zip::write::SimpleFileOptions;

        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("notes.zip");
        let file = File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts = SimpleFileOptions::default();
        writer.start_file("notes/page.md", opts).unwrap();
        writer.write_all(b"# Page").unwrap();
        writer.finish().unwrap();

        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let keep = std::sync::Arc::clone(&seen);
        let progress = crate::progress::Reporter::new(3, move |event| {
            keep.lock()
                .unwrap()
                .push((event.label, event.ratio, event.detail));
        });
        let server = PackServer::open_with(&zip_path, &progress).unwrap();
        assert!(server.markdown);
        assert!(server.index.get().is_some());
        let events = seen.lock().unwrap();
        assert!(events
            .iter()
            .any(|(label, ratio, _)| { label == "Lecture du catalogue" && *ratio == Some(1.0) }));
        assert!(events
            .iter()
            .any(|(label, ratio, _)| label == "Index des fichiers" && *ratio == Some(1.0)));
        assert!(events.iter().any(|(label, ratio, detail)| {
            label == "Index des notes" && *ratio == Some(1.0) && detail.contains("1 note")
        }));
        server.stop();
    }

    #[test]
    fn opening_a_markdown_folder_reports_an_indeterminate_count() {
        use std::sync::Mutex;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), b"# A").unwrap();
        std::fs::write(dir.path().join("b.md"), b"# B").unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let keep = std::sync::Arc::clone(&seen);
        let progress = crate::progress::Reporter::new(4, move |event| {
            keep.lock()
                .unwrap()
                .push((event.label, event.ratio, event.detail));
        });
        let server = PackServer::open_with(dir.path(), &progress).unwrap();
        assert!(server.markdown);
        let events = seen.lock().unwrap();
        assert!(events.iter().any(|(label, ratio, detail)| {
            label == "Index des notes" && ratio.is_none() && detail.contains("2 notes")
        }));
        server.stop();
    }

    fn http_get(port: u16, path: &str) -> (u16, String) {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf);
        let (head, body) = text
            .split_once("\r\n\r\n")
            .or_else(|| text.split_once("\n\n"))
            .unwrap_or(("", text.as_ref()));
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        (status, body.to_string())
    }
}
