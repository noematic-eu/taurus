//! Extraire un zip de pack web et le servir en HTTP local (sans API Tauri).

use std::fs::{self, File};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tiny_http::{Header, Response, Server, StatusCode};

const ENTRY_NAMES: &[&str] = &["OUVRIR.html", "ouvrir.html", "index.html", "index.htm"];

pub struct PackServer {
    pub port: u16,
    pub entry: String,
    pub title: String,
    stop: Arc<AtomicBool>,
}

impl PackServer {
    pub fn open(pack_path: &Path) -> Result<Self, String> {
        let title = pack_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "pack".into());

        let dir = tempfile::tempdir().map_err(|e| format!("temp: {e}"))?;
        let pack_path = resolve_input(pack_path)?;
        let (root, entry) = if is_web_archive(&pack_path) {
            let site = crate::wacz::materialize(&pack_path, dir.path())?;
            site_root(&site)?
        } else {
            unzip(&pack_path, dir.path())?;
            site_root(dir.path())?
        };

        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let server = Server::from_listener(listener, None).map_err(|e| format!("http: {e}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        thread::Builder::new()
            .name(format!("taurus-{port}"))
            .spawn(move || {
                serve(server, root, stop_t);
                drop(dir);
            })
            .map_err(|e| format!("thread: {e}"))?;

        Ok(Self {
            port,
            entry,
            title,
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

/// Si `path` est un dossier de collection Browsertrix, pointe vers le `.wacz` (ou WARC) dedans.
pub(crate) fn resolve_input(path: &Path) -> Result<PathBuf, String> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    if path.is_dir() {
        return find_archive_in_dir(path)
            .ok_or_else(|| "Pas de .wacz / .warc / .zip dans le dossier.".into());
    }
    Err(format!("{} introuvable.", path.display()))
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
    for name in ENTRY_NAMES {
        if extracted.join(name).is_file() {
            return Ok((extracted.to_path_buf(), (*name).to_string()));
        }
    }

    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for ent in fs::read_dir(extracted).map_err(|e| e.to_string())? {
        let ent = ent.map_err(|e| e.to_string())?;
        let p = ent.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if skip_name(name) {
            continue;
        }
        if p.is_dir() {
            dirs.push(p);
        } else {
            files.push(p);
        }
    }

    if dirs.len() == 1 && files.is_empty() {
        return site_root(&dirs[0]);
    }

    Err("Pas de OUVRIR.html ni index.html dans le zip.".into())
}

fn content_type(path: &Path) -> String {
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

fn serve(server: Server, root: PathBuf, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        let req = match server.recv_timeout(Duration::from_millis(250)) {
            Ok(Some(r)) => r,
            Ok(None) | Err(_) => continue,
        };

        let url_path = req.url().split('?').next().unwrap_or("/");
        let rel = url_path.trim_start_matches('/');
        let Some(joined) = resolve_served_path(&root, rel) else {
            let _ =
                req.respond(Response::from_string("introuvable").with_status_code(StatusCode(404)));
            continue;
        };
        let Ok(canon) = joined.canonicalize() else {
            let _ =
                req.respond(Response::from_string("introuvable").with_status_code(StatusCode(404)));
            continue;
        };
        let Ok(root_c) = root.canonicalize() else {
            let _ = req.respond(Response::from_string("erreur").with_status_code(StatusCode(500)));
            continue;
        };
        if !canon.starts_with(&root_c) || !canon.is_file() {
            let _ =
                req.respond(Response::from_string("interdit").with_status_code(StatusCode(403)));
            continue;
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
                let _ = req.respond(
                    Response::from_string("introuvable").with_status_code(StatusCode(404)),
                );
            }
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
}
