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
    pub fn open(zip_path: &Path) -> Result<Self, String> {
        let title = zip_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "pack".into());

        let dir = tempfile::tempdir().map_err(|e| format!("temp: {e}"))?;
        unzip(zip_path, dir.path())?;
        let (root, entry) = site_root(dir.path())?;

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

fn unzip(zip_path: &Path, dest: &Path) -> Result<(), String> {
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
        let rel = if rel.is_empty() {
            "index.html".to_string()
        } else {
            rel.to_string()
        };

        let joined = root.join(&rel);
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
