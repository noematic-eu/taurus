//! Extraire un .wacz / .warc.gz vers un site statique local (HTML + figures).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;

/// Plafond par enregistrement WARC (évite un OOM sur un Content-Length hostile).
const MAX_WARC_RECORD: usize = 512 * 1024 * 1024;

/// Déplie un WACZ (zip) ou un WARC brut dans `dest` et renvoie la racine du site.
pub fn materialize(archive: &Path, dest: &Path) -> Result<PathBuf, String> {
    let name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let site = dest.join("site");
    fs::create_dir_all(&site).map_err(|e| e.to_string())?;

    let mut origins = HashSet::new();
    if name.ends_with(".wacz") {
        let unpacked = dest.join("wacz");
        super::pack::unzip(archive, &unpacked)?;
        let mut warcs = Vec::new();
        collect_warcs(&unpacked, &mut warcs)?;
        if warcs.is_empty() {
            return Err("Pas de .warc dans le WACZ.".into());
        }
        for w in &warcs {
            extract_warc(w, &site, &mut origins)?;
        }
    } else if name.ends_with(".warc.gz") || name.ends_with(".warc") {
        extract_warc(archive, &site, &mut origins)?;
    } else {
        return Err("Fichier .wacz ou .warc attendu.".into());
    }

    rewrite_site(&site, &origins)?;
    ensure_index(&site)?;
    Ok(site)
}

fn collect_warcs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for ent in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let p = ent.map_err(|e| e.to_string())?.path();
        if p.is_dir() {
            collect_warcs(&p, out)?;
            continue;
        }
        let n = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let low = n.to_ascii_lowercase();
        if low.ends_with(".warc.gz") || low.ends_with(".warc") {
            out.push(p);
        }
    }
    Ok(())
}

fn extract_warc(path: &Path, site: &Path, origins: &mut HashSet<String>) -> Result<(), String> {
    let file = File::open(path).map_err(|e| format!("warc: {e}"))?;
    let gzip = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().ends_with(".gz"));
    if gzip {
        let decoder = MultiGzDecoder::new(file);
        read_records(BufReader::new(decoder), site, origins)
    } else {
        read_records(BufReader::new(file), site, origins)
    }
}

fn read_records<R: BufRead>(
    mut reader: R,
    site: &Path,
    origins: &mut HashSet<String>,
) -> Result<(), String> {
    loop {
        let mut version = String::new();
        let n = reader.read_line(&mut version).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if version.trim().is_empty() {
            continue;
        }
        if !version.to_ascii_uppercase().starts_with("WARC/") {
            return Err(format!("en-tête WARC inattendu: {}", version.trim()));
        }

        let mut headers = HashMap::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).map_err(|e| e.to_string())?;
            let t = line.trim_end_matches(['\r', '\n']);
            if t.is_empty() {
                break;
            }
            if let Some((k, v)) = t.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }

        let len: usize = headers
            .get("content-length")
            .ok_or_else(|| "WARC sans Content-Length".to_string())?
            .parse()
            .map_err(|_| "Content-Length WARC invalide".to_string())?;
        if len > MAX_WARC_RECORD {
            return Err(format!(
                "enregistrement WARC trop gros ({len} octets, max {MAX_WARC_RECORD})."
            ));
        }

        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).map_err(|e| format!("corps WARC: {e}"))?;
        // Skip the CRLF(s) that terminate the WARC record.
        loop {
            let avail = reader.fill_buf().map_err(|e| e.to_string())?;
            if avail.is_empty() {
                break;
            }
            if avail[0] == b'\r' || avail[0] == b'\n' {
                reader.consume(1);
            } else {
                break;
            }
        }

        let warc_type = headers.get("warc-type").map(|s| s.as_str()).unwrap_or("");
        if warc_type != "response" && warc_type != "resource" {
            continue;
        }
        let Some(uri) = headers.get("warc-target-uri") else {
            continue;
        };
        // Browsertrix pageinfo/screenshots: urn:pageinfo:https://… — pas des pages web.
        if !is_http_uri(uri) {
            continue;
        }

        let (payload, as_html) = if warc_type == "resource" {
            let ctype = headers.get("content-type").map(|s| s.as_str()).unwrap_or("");
            (body, is_html_type(ctype))
        } else {
            match split_http(&body) {
                Some((status, payload)) if (200..300).contains(&status) => {
                    (payload, is_html_type(&http_content_type(&body)))
                }
                _ => continue,
            }
        };

        let rel = url_to_rel_with(uri, as_html)?;
        write_payload(site, &rel, &payload)?;
        if as_html {
            if let Some(origin) = origin_of(uri) {
                origins.insert(origin);
            }
        }
    }
    Ok(())
}

fn write_payload(site: &Path, rel: &str, payload: &[u8]) -> Result<(), String> {
    let dest = site.join(rel);
    if dest.exists() && dest.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        if parent.exists() && parent.is_file() {
            return Ok(());
        }
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&dest, payload).map_err(|e| format!("écriture {rel}: {e}"))
}

fn is_html_type(ctype: &str) -> bool {
    let c = ctype.to_ascii_lowercase();
    c.contains("text/html") || c.contains("application/xhtml")
}

fn http_content_type(body: &[u8]) -> String {
    let Some(sep) = body.windows(4).position(|w| w == b"\r\n\r\n") else {
        return String::new();
    };
    let Ok(head) = std::str::from_utf8(&body[..sep]) else {
        return String::new();
    };
    for line in head.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k.eq_ignore_ascii_case("content-type") {
            return v.trim().to_string();
        }
    }
    String::new()
}

fn is_http_uri(uri: &str) -> bool {
    let u = uri.trim().trim_matches(['<', '>']);
    let Some((scheme, _)) = u.split_once("://") else {
        return false;
    };
    scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
}

fn origin_of(uri: &str) -> Option<String> {
    let u = uri.trim().trim_matches(['<', '>']);
    let (scheme, rest) = u.split_once("://")?;
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) {
        return None;
    }
    let host = rest.split('/').next().filter(|h| !h.is_empty())?;
    Some(format!("{}://{host}", scheme.to_ascii_lowercase()))
}

fn split_http(body: &[u8]) -> Option<(u16, Vec<u8>)> {
    let sep = body.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&body[..sep]).ok()?;
    let status = head.lines().next()?.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, body[sep + 4..].to_vec()))
}

/// Mappe une URL d’archive vers un chemin relatif sous la racine du site.
#[cfg(test)]
fn url_to_rel(uri: &str) -> Result<String, String> {
    url_to_rel_with(uri, false)
}

/// Comme `url_to_rel`, mais une page HTML sans extension devient `…/index.html`
/// (évite le conflit fichier `blog` vs dossier `blog/page/2` des crawls Browsertrix).
pub fn url_to_rel_with(uri: &str, as_html: bool) -> Result<String, String> {
    let uri = uri.trim().trim_matches(['<', '>']);
    let path = if let Some(rest) = uri.split_once("://") {
        rest.1.split_once('/').map(|(_, p)| p).unwrap_or("")
    } else {
        uri.trim_start_matches('/')
    };
    let path = path.split(['?', '#']).next().unwrap_or(path);
    if path.is_empty() {
        return Ok("index.html".into());
    }
    let trailing_slash = path.ends_with('/');
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.iter().any(|p| *p == "..") {
        return Err(format!("URL refusée: {uri}"));
    }
    // Figures Manning CloudFront → figures/<fichier>
    if parts.len() >= 2 && parts[parts.len() - 2].eq_ignore_ascii_case("figures") {
        let file = parts.last().copied().unwrap_or("figure.bin");
        return Ok(format!("figures/{file}"));
    }
    let last = parts.last().copied().unwrap_or("");
    let has_ext = last.contains('.');
    if trailing_slash || (as_html && !has_ext) {
        return Ok(format!("{}/index.html", parts.join("/")));
    }
    Ok(parts.join("/"))
}

fn rewrite_site(site: &Path, origins: &HashSet<String>) -> Result<(), String> {
    let mut files = Vec::new();
    collect_rewritable(site, site, &mut files)?;
    for path in files {
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let rewritten = rewrite_cloudfront_src(&rewrite_origins(&raw, origins));
        if rewritten != raw {
            fs::write(&path, rewritten).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn rewrite_origins(html: &str, origins: &HashSet<String>) -> String {
    let mut list: Vec<&String> = origins.iter().collect();
    list.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = html.to_string();
    for origin in list {
        out = out.replace(origin, "");
        if let Some((_, host)) = origin.split_once("://") {
            out = out.replace(&format!("//{host}"), "");
        }
    }
    out
}

/// Remplace les URL CloudFront Manning `…/Figures/NAME` par `figures/NAME`.
pub fn rewrite_cloudfront_src(html: &str) -> String {
    // https://host/…/Figures/file.ext  →  figures/file.ext
    let mut out = String::with_capacity(html.len());
    let needle = "/Figures/";
    let mut rest = html;
    while let Some(idx) = rest.find(needle) {
        let before = &rest[..idx];
        if let Some(scheme) = before.rfind("https://").or_else(|| before.rfind("http://")) {
            out.push_str(&before[..scheme]);
            let after = &rest[idx + needle.len()..];
            let end = after
                .find(|c: char| matches!(c, '"' | '\'' | ' ' | ')' | '<' | '>' | '?' | '#'))
                .unwrap_or(after.len());
            let file = &after[..end];
            out.push_str("figures/");
            out.push_str(file);
            rest = &after[end..];
        } else {
            out.push_str(&rest[..idx + needle.len()]);
            rest = &rest[idx + needle.len()..];
        }
    }
    out.push_str(rest);
    out
}

fn collect_rewritable(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for ent in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let p = ent.map_err(|e| e.to_string())?.path();
        if p.is_dir() {
            collect_rewritable(root, &p, out)?;
            continue;
        }
        if p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            matches!(e.to_ascii_lowercase().as_str(), "html" | "htm" | "css")
        }) {
            out.push(p);
        }
    }
    Ok(())
}

fn ensure_index(site: &Path) -> Result<(), String> {
    for name in ["OUVRIR.html", "ouvrir.html", "index.html", "index.htm"] {
        if site.join(name).is_file() {
            return Ok(());
        }
    }
    let mut pages: Vec<String> = fs::read_dir(site)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
        .filter(|n| n.to_ascii_lowercase().ends_with(".html"))
        .collect();
    pages.sort_by(|a, b| nat_cmp(a, b));
    if pages.is_empty() {
        return Err("Aucune page HTML extraite du WARC.".into());
    }
    let mut body = String::from(
        "<!doctype html><meta charset=utf-8><title>Archive</title>\
         <style>body{font:16px/1.4 system-ui;max-width:40rem;margin:2rem auto;padding:0 1rem}\
         a{display:block;padding:.35rem 0}</style><h1>Archive</h1>\n",
    );
    for p in &pages {
        body.push_str(&format!("<a href=\"{p}\">{p}</a>\n"));
    }
    fs::write(site.join("index.html"), body).map_err(|e| e.to_string())?;
    Ok(())
}

fn nat_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let na = a
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse::<u32>()
        .ok();
    let nb = b
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse::<u32>()
        .ok();
    match (na, nb) {
        (Some(x), Some(y)) if x != y => x.cmp(&y),
        _ => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::io::Read;

    #[test]
    fn maps_local_chapter() {
        assert_eq!(
            url_to_rel("http://127.0.0.1:53150/1.html").unwrap(),
            "1.html"
        );
        assert_eq!(
            url_to_rel("<http://127.0.0.1:53150/1.html>").unwrap(),
            "1.html"
        );
    }

    #[test]
    fn maps_browsertrix_directory_urls() {
        assert_eq!(url_to_rel("https://ex.com/").unwrap(), "index.html");
        assert_eq!(
            url_to_rel("https://ex.com/blog/").unwrap(),
            "blog/index.html"
        );
        assert_eq!(
            url_to_rel("https://ex.com/blog/page/2/").unwrap(),
            "blog/page/2/index.html"
        );
        assert_eq!(
            url_to_rel("https://ex.com/style.css?ver=4").unwrap(),
            "style.css"
        );
        assert_eq!(
            url_to_rel_with("https://ex.com/about", true).unwrap(),
            "about/index.html"
        );
        assert!(!is_http_uri(
            "urn:pageinfo:https://www.getcybersecurity.fr/blog/"
        ));
        assert!(is_http_uri("https://www.getcybersecurity.fr/blog/"));
    }

    #[test]
    fn maps_manning_figure() {
        assert_eq!(
            url_to_rel(
                "https://drek4537l1klr.cloudfront.net/kimothi/Figures/CH01_F01_Kimothi.png"
            )
            .unwrap(),
            "figures/CH01_F01_Kimothi.png"
        );
    }

    #[test]
    fn rejects_dotdot() {
        assert!(url_to_rel("http://x/../../etc/passwd").is_err());
    }

    #[test]
    fn rewrites_img_src() {
        let html = r#"<img src="https://drek4537l1klr.cloudfront.net/kimothi/Figures/CH01_F01_Kimothi.png" alt="x">"#;
        let out = rewrite_cloudfront_src(html);
        assert!(out.contains("figures/CH01_F01_Kimothi.png"));
        assert!(!out.contains("cloudfront"));
    }

    #[test]
    fn rejects_oversized_warc_record() {
        let warc = format!(
            "WARC/1.0\r\nWARC-Type: response\r\nWARC-Target-URI: http://x/\r\nContent-Length: {}\r\n\r\n",
            MAX_WARC_RECORD + 1
        );
        let dir = tempfile::tempdir().unwrap();
        let warc_path = dir.path().join("huge.warc");
        std::fs::write(&warc_path, warc).unwrap();
        let site = dir.path().join("site");
        std::fs::create_dir(&site).unwrap();
        let err = extract_warc(&warc_path, &site, &mut HashSet::new()).unwrap_err();
        assert!(err.contains("trop gros"), "{err}");
        assert!(err.contains(&(MAX_WARC_RECORD + 1).to_string()), "{err}");
    }

    #[test]
    fn split_ok() {
        let body = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\nhello";
        let (st, p) = split_http(body).unwrap();
        assert_eq!(st, 200);
        assert_eq!(p, b"hello");
    }

    #[test]
    fn extracts_uncompressed_warc() {
        let html = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><img src=\"https://cdn.test/Figures/a.png\"></html>";
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
            rec("http://127.0.0.1:8/1.html", html),
            rec("https://cdn.test/Figures/a.png", img),
        ]
        .concat();

        let dir = tempfile::tempdir().unwrap();
        let warc_path = dir.path().join("t.warc");
        std::fs::write(&warc_path, warc).unwrap();
        let site = dir.path().join("site");
        std::fs::create_dir(&site).unwrap();
        let mut origins = HashSet::new();
        extract_warc(&warc_path, &site, &mut origins).unwrap();
        rewrite_site(&site, &origins).unwrap();
        ensure_index(&site).unwrap();

        let page = std::fs::read_to_string(site.join("1.html")).unwrap();
        assert!(page.contains("figures/a.png"), "{page}");
        assert!(!page.contains("cdn.test"));
        assert_eq!(std::fs::read(site.join("figures/a.png")).unwrap(), b"PNG");
        assert!(site.join("index.html").is_file());
    }

    #[test]
    fn counts_wget_warc_records() {
        let home = std::env::var("HOME").unwrap_or_default();
        let p = PathBuf::from(home).join(
            "kb/html/manning/_archives/a-simple-guide-to-retrieval-augmented-generation/a-simple-guide-to-retrieval-augmented-generation.warc.gz",
        );
        if !p.is_file() {
            return;
        }
        let file = File::open(&p).unwrap();
        let decoder = MultiGzDecoder::new(file);
        let mut reader = BufReader::new(decoder);
        let mut n = 0u32;
        let mut types = std::collections::BTreeMap::<String, u32>::new();
        loop {
            let mut version = String::new();
            let k = reader.read_line(&mut version).unwrap();
            if k == 0 {
                break;
            }
            if version.trim().is_empty() {
                continue;
            }
            if !version.to_ascii_uppercase().starts_with("WARC/") {
                panic!("desync at record {n}: {:?}", version.chars().take(40).collect::<String>());
            }
            let mut headers = HashMap::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let t = line.trim_end_matches(['\r', '\n']);
                if t.is_empty() {
                    break;
                }
                if let Some((k, v)) = t.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                }
            }
            let len: usize = headers["content-length"].parse().unwrap();
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).unwrap();
            loop {
                let avail = reader.fill_buf().unwrap();
                if avail.is_empty() || (avail[0] != b'\r' && avail[0] != b'\n') {
                    break;
                }
                reader.consume(1);
            }
            n += 1;
            let t = headers.get("warc-type").cloned().unwrap_or_default();
            *types.entry(t).or_default() += 1;
        }
        eprintln!("records={n} types={types:?}");
        assert!(n > 10, "only {n} records");
    }

    #[test]
    fn materialize_manning_wacz_if_present() {
        let home = std::env::var("HOME").unwrap_or_default();
        let p = PathBuf::from(home).join(
            "kb/html/manning/_archives/a-simple-guide-to-retrieval-augmented-generation/a-simple-guide-to-retrieval-augmented-generation.wacz",
        );
        if !p.is_file() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let site = materialize(&p, dir.path()).unwrap();
        assert!(site.join("1.html").is_file());
        assert!(site.join("9.html").is_file());
        assert!(site.join("index.html").is_file());
        let fig_count = std::fs::read_dir(site.join("figures")).unwrap().count();
        assert!(fig_count > 50, "expected Manning figures, got {fig_count}");
        let html = std::fs::read_to_string(site.join("1.html")).unwrap();
        assert!(
            html.contains("figures/CH01_F01_Kimothi.png"),
            "CloudFront src should be rewritten"
        );
        assert!(!html.contains("drek4537l1klr.cloudfront.net"));
    }

    #[test]
    fn extracts_nested_html_without_file_dir_clash() {
        let rec = |uri: &str, ctype: &str, body: &[u8]| {
            let http = [
                format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\n\r\n").into_bytes(),
                body.to_vec(),
            ]
            .concat();
            format!(
                "WARC/1.0\r\nWARC-Type: response\r\nWARC-Target-URI: {uri}\r\nContent-Length: {}\r\n\r\n",
                http.len()
            )
            .into_bytes()
            .into_iter()
            .chain(http)
            .chain(b"\r\n\r\n".iter().copied())
            .collect::<Vec<u8>>()
        };
        let pageinfo = |uri: &str| {
            let body = b"{\"id\":\"x\"}";
            format!(
                "WARC/1.0\r\nWARC-Type: resource\r\nWARC-Target-URI: {uri}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .into_bytes()
            .into_iter()
            .chain(body.iter().copied())
            .chain(b"\r\n\r\n".iter().copied())
            .collect::<Vec<u8>>()
        };
        let warc = [
            rec(
                "https://ex.com/",
                "text/html",
                b"<html><link href=\"https://ex.com/s.css\"><a href=\"https://ex.com/blog/\">b</a></html>",
            ),
            rec("https://ex.com/s.css", "text/css", b"body{color:red}"),
            rec("https://ex.com/blog/", "text/html", b"<html>blog</html>"),
            rec(
                "https://ex.com/blog/page/2/",
                "text/html",
                b"<html>page2</html>",
            ),
            pageinfo("urn:pageinfo:https://ex.com/"),
            pageinfo("urn:pageinfo:https://ex.com/blog/"),
        ]
        .concat();

        let dir = tempfile::tempdir().unwrap();
        let warc_path = dir.path().join("t.warc");
        std::fs::write(&warc_path, warc).unwrap();
        let site = dir.path().join("site");
        std::fs::create_dir(&site).unwrap();
        let mut origins = HashSet::new();
        extract_warc(&warc_path, &site, &mut origins).unwrap();
        rewrite_site(&site, &origins).unwrap();
        ensure_index(&site).unwrap();

        let home = std::fs::read_to_string(site.join("index.html")).unwrap();
        assert!(home.contains("<html>"), "{home}");
        assert!(home.contains("href=\"/s.css\""), "{home}");
        assert!(home.contains("href=\"/blog/\""), "{home}");
        assert!(!home.contains("https://ex.com"), "{home}");
        assert_eq!(
            std::fs::read_to_string(site.join("blog/index.html")).unwrap(),
            "<html>blog</html>"
        );
        assert_eq!(
            std::fs::read_to_string(site.join("blog/page/2/index.html")).unwrap(),
            "<html>page2</html>"
        );
        assert_eq!(
            std::fs::read_to_string(site.join("s.css")).unwrap(),
            "body{color:red}"
        );
    }

    #[test]
    fn materialize_browsertrix_example_if_present() {
        let p = PathBuf::from(
            "/Users/baptisteboussemart/wacz/crawls/collections/example/example.wacz",
        );
        if !p.is_file() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let site = materialize(&p, dir.path()).unwrap();
        let html = std::fs::read_to_string(site.join("index.html")).unwrap();
        let low = html.to_ascii_lowercase();
        assert!(
            low.contains("<html") || low.contains("<!doctype"),
            "index.html should be HTML, got {} bytes starting {:?}",
            html.len(),
            html.chars().take(80).collect::<String>()
        );
        assert!(
            site.join("blog/index.html").is_file(),
            "expected blog/index.html"
        );
        assert!(
            site.join("wp-content/themes/astra/assets/css/minified/frontend.min.css")
                .is_file(),
            "expected crawled CSS"
        );
    }
}
