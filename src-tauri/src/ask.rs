//! Interrogation locale d’un pack par Ollama.
//!
//! Le lot et la page courante restent en mémoire le temps du pack. Rien n’est
//! envoyé avant la question, et uniquement vers l’hôte configuré.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;

pub(crate) const DOC_CHARS: usize = 12_000;
pub(crate) const BATCH_MAX: usize = 8;
const CHAT_TIMEOUT: Duration = Duration::from_secs(120);
const TAGS_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) const SYSTEM_PROMPT: &str = "\
Tu réponds uniquement à partir des documents fournis. Si ces documents ne permettent pas de répondre, dis-le. Pour chaque passage utilisé, cite le nom de fichier entre crochets, par exemple [France_grokipedia.md]. N’invente pas de source. Réponds en français, de façon concise.";

pub(crate) const DEFAULT_OLLAMA_HOST: &str = "http://127.0.0.1:11434";

#[derive(Serialize)]
pub(crate) struct AskView {
    pub current: Option<String>,
    pub batch: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct AskReply {
    pub answer: String,
    pub files: Vec<String>,
}

pub(crate) struct LivePack {
    pub root: PathBuf,
    pub markdown: bool,
    pub port: u16,
    pub index: Arc<OnceLock<crate::md::MdIndex>>,
    last_path: Mutex<Option<String>>,
    current: Mutex<Option<String>>,
    batch: Mutex<Vec<String>>,
}

impl LivePack {
    pub(crate) fn new(
        root: PathBuf,
        markdown: bool,
        port: u16,
        index: Arc<OnceLock<crate::md::MdIndex>>,
    ) -> Self {
        Self {
            root,
            markdown,
            port,
            index,
            last_path: Mutex::new(None),
            current: Mutex::new(None),
            batch: Mutex::new(Vec::new()),
        }
    }

    /// Mémorise une navigation. Une URL hors de ce pack est ignorée. Une URL
    /// du pack qui ne résout pas vers un fichier ne remplace pas la page courante.
    pub(crate) fn note(&self, url: &tauri::Url) {
        if !is_pack_origin(url, self.port) {
            return;
        }
        let path = url.path().to_string();
        if let Some(rel) = source_of(&self.root, self.markdown, self.index.get(), &path) {
            *lock(&self.last_path) = Some(path);
            *lock(&self.current) = Some(rel);
            return;
        }
        // L'index Markdown n'est pas encore prêt : on garde l'URL pour la
        // résoudre à la question, sans laisser l'accueil écraser un article.
        if self.markdown && self.index.get().is_none() && looks_like_document(&path) {
            *lock(&self.last_path) = Some(path);
        }
    }

    pub(crate) fn resolved_current(&self) -> Option<String> {
        let path = lock(&self.last_path).clone();
        if let Some(path) = path {
            if let Some(rel) = source_of(&self.root, self.markdown, self.index.get(), &path) {
                *lock(&self.current) = Some(rel.clone());
                return Some(rel);
            }
        }
        lock(&self.current).clone()
    }

    pub(crate) fn view(&self) -> AskView {
        AskView {
            current: self.resolved_current(),
            batch: lock(&self.batch).clone(),
        }
    }

    pub(crate) fn add_current(&self) -> Result<AskView, String> {
        let Some(rel) = self.resolved_current() else {
            return Err("Aucune page courante à ajouter.".into());
        };
        add_to_batch(&mut lock(&self.batch), &rel)?;
        Ok(self.view())
    }

    pub(crate) fn remove(&self, rel: &str) -> AskView {
        lock(&self.batch).retain(|p| p != rel);
        self.view()
    }

    pub(crate) fn question(
        &self,
        question: &str,
        host: &str,
        model: Option<&str>,
    ) -> Result<AskReply, String> {
        let current = self.resolved_current();
        let batch = lock(&self.batch).clone();
        let rels = pages_to_send(&batch, current.as_deref())?;
        let mut docs = Vec::with_capacity(rels.len());
        for rel in &rels {
            let text = read_pack_text(&self.root, rel, self.markdown)?;
            docs.push((rel.clone(), text));
        }
        let question = normalize_question(question);
        let user = user_message(&docs, &question);
        let model = match model.map(str::trim).filter(|m| !m.is_empty()) {
            Some(model) => model.to_string(),
            None => fetch_model(host)?,
        };
        let answer = post_chat(host, &model, &user)?;
        Ok(AskReply {
            answer,
            files: rels,
        })
    }
}

fn looks_like_document(path: &str) -> bool {
    let rel = path.trim_matches('/');
    !rel.is_empty() && rel != "index.html" && rel != "index.htm"
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

pub(crate) struct AskStore {
    packs: Mutex<std::collections::HashMap<u64, Arc<LivePack>>>,
}

impl Default for AskStore {
    fn default() -> Self {
        Self {
            packs: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl AskStore {
    pub(crate) fn insert(&self, id: u64, pack: Arc<LivePack>) {
        lock(&self.packs).insert(id, pack);
    }

    pub(crate) fn get(&self, id: u64) -> Result<Arc<LivePack>, String> {
        lock(&self.packs)
            .get(&id)
            .cloned()
            .ok_or_else(|| "Ce pack est fermé.".to_string())
    }

    pub(crate) fn remove(&self, id: u64) {
        lock(&self.packs).remove(&id);
    }
}

pub(crate) fn ollama_host() -> String {
    std::env::var("TAURUS_OLLAMA_HOST")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_HOST.to_string())
}

pub(crate) fn ollama_model() -> Option<String> {
    std::env::var("TAURUS_OLLAMA_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn is_pack_origin(url: &tauri::Url, port: u16) -> bool {
    url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.port_or_known_default() == Some(port)
}

pub(crate) fn source_of(
    root: &Path,
    markdown: bool,
    index: Option<&crate::md::MdIndex>,
    url_path: &str,
) -> Option<String> {
    if markdown {
        crate::md::markdown_source(root, index?, url_path)
    } else {
        crate::pack::html_source(root, url_path)
    }
}

/// Pages envoyées : le lot s’il n’est pas vide, sinon la page courante seule.
pub(crate) fn pages_to_send(
    batch: &[String],
    current: Option<&str>,
) -> Result<Vec<String>, String> {
    if !batch.is_empty() {
        return Ok(batch.to_vec());
    }
    match current.map(str::trim).filter(|rel| !rel.is_empty()) {
        Some(rel) => Ok(vec![rel.to_string()]),
        None => Err("Aucune page à interroger. Ouvrez une page du pack.".into()),
    }
}

pub(crate) fn add_to_batch(batch: &mut Vec<String>, rel: &str) -> Result<(), String> {
    if batch.iter().any(|p| p == rel) {
        return Ok(());
    }
    if batch.len() >= BATCH_MAX {
        return Err("Le lot est limité à 8 pages.".into());
    }
    batch.push(rel.to_string());
    Ok(())
}

pub(crate) fn normalize_question(question: &str) -> String {
    let question = question.trim();
    if question.is_empty() {
        "Résume.".to_string()
    } else {
        question.to_string()
    }
}

pub(crate) fn user_message(docs: &[(String, String)], question: &str) -> String {
    let mut out = String::from("Documents :\n");
    for (rel, text) in docs {
        let (text, cut) = truncate_chars(text, DOC_CHARS);
        out.push('\n');
        out.push_str("## fichier: ");
        out.push_str(rel);
        out.push_str("\n\n");
        out.push_str(&text);
        if cut {
            if !text.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("[Document tronqué à 12 000 caractères.]");
        }
        out.push('\n');
    }
    out.push_str("\nQuestion : ");
    out.push_str(question);
    out
}

fn truncate_chars(text: &str, max: usize) -> (String, bool) {
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i == max {
            return (out, true);
        }
        out.push(ch);
    }
    (out, false)
}

pub(crate) fn read_pack_text(root: &Path, rel: &str, markdown: bool) -> Result<String, String> {
    if rel.is_empty()
        || rel.contains('\\')
        || rel
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return Err("Chemin refusé.".into());
    }
    let path =
        crate::md::under_root(root, &root.join(rel)).ok_or_else(|| "Chemin refusé.".to_string())?;
    if !path.is_file() {
        return Err("Chemin refusé.".into());
    }
    let bytes = fs::read(&path).map_err(|_| format!("Impossible de lire {rel}."))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if markdown {
        Ok(text)
    } else {
        Ok(strip_script_and_style(&text))
    }
}

fn strip_script_and_style(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if let Some(end) =
            element_end(bytes, i, b"script").or_else(|| element_end(bytes, i, b"style"))
        {
            i = end;
            continue;
        }
        let ch = input[i..].chars().next().unwrap_or('\u{FFFD}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn element_end(lower: &[u8], i: usize, name: &[u8]) -> Option<usize> {
    if i >= lower.len() || lower[i] != b'<' {
        return None;
    }
    let name_at = i + 1;
    let after = name_at + name.len();
    if after > lower.len() || &lower[name_at..after] != name {
        return None;
    }
    let boundary = *lower.get(after).unwrap_or(&b'>');
    if !(boundary.is_ascii_whitespace() || boundary == b'>' || boundary == b'/') {
        return None;
    }
    let mut close = Vec::with_capacity(name.len() + 3);
    close.extend_from_slice(b"</");
    close.extend_from_slice(name);
    close.push(b'>');
    match lower[after..]
        .windows(close.len())
        .position(|w| w == close.as_slice())
    {
        Some(rel) => Some(after + rel + close.len()),
        None => Some(lower.len()),
    }
}

fn fetch_model(host: &str) -> Result<String, String> {
    let body = http_exchange(host, "GET", "/api/tags", None, TAGS_TIMEOUT)?;
    let value: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| "Réponse Ollama illisible.".to_string())?;
    value["models"]
        .as_array()
        .and_then(|models| models.first())
        .and_then(|model| model["name"].as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "Aucun modèle Ollama n’est installé.".to_string())
}

fn post_chat(host: &str, model: &str, user: &str) -> Result<String, String> {
    let payload = serde_json::json!({
        "model": model,
        "stream": false,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": user}
        ]
    });
    let bytes = serde_json::to_vec(&payload).map_err(|e| format!("json: {e}"))?;
    let body = http_exchange(host, "POST", "/api/chat", Some(&bytes), CHAT_TIMEOUT)?;
    let value: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| "Réponse Ollama illisible.".to_string())?;
    value["message"]["content"]
        .as_str()
        .map(str::to_string)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| "Ollama n’a pas renvoyé de réponse.".to_string())
}

fn http_exchange(
    host: &str,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let (addr, port) = parse_http_host(host)?;
    let sock = (addr.as_str(), port)
        .to_socket_addrs()
        .map_err(|_| format!("Ollama ne répond pas à {host}."))?
        .next()
        .ok_or_else(|| format!("Ollama ne répond pas à {host}."))?;
    let mut stream = TcpStream::connect_timeout(&sock, timeout)
        .map_err(|_| format!("Ollama ne répond pas à {host}."))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|_| format!("Ollama ne répond pas à {host}."))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|_| format!("Ollama ne répond pas à {host}."))?;

    let body = body.unwrap_or(b"");
    let header = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}:{port}\r\nConnection: close\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream
        .write_all(header.as_bytes())
        .and_then(|_| stream.write_all(body))
        .map_err(|_| format!("Ollama ne répond pas à {host}."))?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|_| format!("Ollama ne répond pas à {host}."))?;
    let sep = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "Réponse Ollama illisible.".to_string())?;
    let head =
        std::str::from_utf8(&buf[..sep]).map_err(|_| "Réponse Ollama illisible.".to_string())?;
    let status: u16 = head
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| "Réponse Ollama illisible.".to_string())?;
    if !(200..300).contains(&status) {
        return Err(format!("Ollama a répondu {status}."));
    }
    let rest = &buf[sep + 4..];
    if head.to_ascii_lowercase().lines().any(|line| {
        let line = line.trim();
        line.starts_with("transfer-encoding:") && line.contains("chunked")
    }) {
        decode_chunked(rest)
    } else {
        Ok(rest.to_vec())
    }
}

fn parse_http_host(host: &str) -> Result<(String, u16), String> {
    let host = host.trim().trim_end_matches('/');
    let rest = host
        .strip_prefix("http://")
        .ok_or_else(|| "TAURUS_OLLAMA_HOST doit être une URL http.".to_string())?;
    if rest.is_empty() || rest.contains('/') {
        return Err("Hôte Ollama illisible.".into());
    }
    if let Some((name, port)) = rest.rsplit_once(':') {
        if name.is_empty() {
            return Err("Hôte Ollama illisible.".into());
        }
        let port: u16 = port
            .parse()
            .map_err(|_| "Port Ollama illisible.".to_string())?;
        Ok((name.to_string(), port))
    } else {
        Ok((rest.to_string(), 80))
    }
}

fn decode_chunked(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < input.len() {
        let line_end = input[i..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| "Réponse Ollama illisible.".to_string())?;
        let size_txt = std::str::from_utf8(&input[i..i + line_end])
            .map_err(|_| "Réponse Ollama illisible.".to_string())?;
        let size_txt = size_txt.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_txt, 16)
            .map_err(|_| "Réponse Ollama illisible.".to_string())?;
        i += line_end + 2;
        if size == 0 {
            break;
        }
        let end = i + size;
        if end > input.len() {
            return Err("Réponse Ollama illisible.".into());
        }
        out.extend_from_slice(&input[i..end]);
        i = end + 2;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    fn md_pack() -> (tempfile::TempDir, crate::md::MdIndex) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Ceremonial-magic_grokipedia.md"),
            b"# Grok magic\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("Ceremonial-magic_wikipedia.md"),
            b"# Wiki\n",
        )
        .unwrap();
        fs::write(dir.path().join("note.md"), b"Texte de la note.\n").unwrap();
        let index = crate::md::MdIndex::build(dir.path());
        (dir, index)
    }

    #[test]
    fn markdown_page_url_resolves_to_the_source_file() {
        let (dir, index) = md_pack();
        let rel = source_of(dir.path(), true, Some(&index), "/page/Ceremonial_magic").unwrap();
        assert_eq!(rel, "Ceremonial-magic_grokipedia.md");
        let text = read_pack_text(dir.path(), &rel, true).unwrap();
        assert!(text.contains("# Grok magic"), "{text}");
        assert!(!text.contains("<h1>"), "{text}");
        assert!(source_of(dir.path(), true, Some(&index), "/index.html").is_none());
    }

    #[test]
    fn html_url_resolves_to_the_served_file_without_script_or_style() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("index.html"),
            b"<p>Bonjour</p><script>alert(1)</script><style>body{color:red}</style><p>Fin</p>",
        )
        .unwrap();
        let rel = source_of(dir.path(), false, None, "/index.html").unwrap();
        assert_eq!(rel, "index.html");
        let text = read_pack_text(dir.path(), &rel, false).unwrap();
        assert!(text.contains("Bonjour"), "{text}");
        assert!(text.contains("Fin"), "{text}");
        assert!(!text.to_lowercase().contains("alert"), "{text}");
        assert!(!text.contains("color:red"), "{text}");
    }

    #[test]
    fn path_outside_the_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("index.html"), b"home").unwrap();
        let err = read_pack_text(dir.path(), "../secret.md", false).unwrap_err();
        assert!(err.contains("refusé"), "{err}");
        assert!(source_of(dir.path(), false, None, "/../secret.md").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_outside_the_root_is_refused() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("index.html"), b"home").unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.md"), b"Secret-outside").unwrap();
        symlink(outside.path().join("secret.md"), dir.path().join("escape")).unwrap();

        assert!(source_of(dir.path(), false, None, "/escape").is_none());
        let err = read_pack_text(dir.path(), "escape", false).unwrap_err();
        assert!(err.contains("refusé"), "{err}");
    }

    #[test]
    fn empty_batch_uses_the_current_page_only() {
        let pages = pages_to_send(&[], Some("note.md")).unwrap();
        assert_eq!(pages, vec!["note.md".to_string()]);
    }

    #[test]
    fn nonempty_batch_ignores_the_current_page_when_absent() {
        let batch = vec!["a.md".to_string()];
        let pages = pages_to_send(&batch, Some("b.md")).unwrap();
        assert_eq!(pages, vec!["a.md".to_string()]);
        assert!(!pages.iter().any(|p| p == "b.md"));
    }

    #[test]
    fn ninth_page_is_refused_and_a_duplicate_is_ignored() {
        let mut batch = Vec::new();
        for i in 0..BATCH_MAX {
            add_to_batch(&mut batch, &format!("p{i}.md")).unwrap();
        }
        assert_eq!(batch.len(), 8);
        let err = add_to_batch(&mut batch, "p8.md").unwrap_err();
        assert!(err.contains("8"), "{err}");
        assert_eq!(batch.len(), 8);
        add_to_batch(&mut batch, "p0.md").unwrap();
        assert_eq!(batch.len(), 8);
    }

    #[test]
    fn user_message_contains_paths_question_and_truncation() {
        let msg = user_message(
            &[
                ("note.md".into(), "Texte de la note.".into()),
                ("Science/Zinc_grokipedia.md".into(), "zinc".into()),
            ],
            "Pourquoi ?",
        );
        assert!(msg.contains("## fichier: note.md"), "{msg}");
        assert!(
            msg.contains("## fichier: Science/Zinc_grokipedia.md"),
            "{msg}"
        );
        assert!(msg.contains("Texte de la note."), "{msg}");
        assert!(msg.contains("Question : Pourquoi ?"), "{msg}");
        assert_eq!(normalize_question("  "), "Résume.");
        let resumed = user_message(&[("note.md".into(), "x".into())], &normalize_question(""));
        assert!(resumed.contains("Question : Résume."), "{resumed}");

        let long = "é".repeat(DOC_CHARS + 3);
        let cut = user_message(&[("long.md".into(), long)], "ok");
        assert!(
            cut.contains("[Document tronqué à 12 000 caractères.]"),
            "{cut}"
        );
        let text = cut
            .split_once("## fichier: long.md\n\n")
            .unwrap()
            .1
            .split_once("\n[Document tronqué")
            .unwrap()
            .0;
        assert_eq!(text.chars().count(), DOC_CHARS);
        assert!(text.chars().all(|ch| ch == 'é'));
    }

    #[test]
    fn question_posts_chat_json_and_returns_the_fixed_answer() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("index.html"),
            b"<p>Bonjour le texte.</p><script>nope()</script>",
        )
        .unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = match server.server_addr() {
            tiny_http::ListenAddr::IP(addr) => addr.port(),
            other => panic!("adresse inattendue: {other:?}"),
        };
        let saw = Arc::new(AtomicBool::new(false));
        let saw_t = saw.clone();
        let handle = thread::spawn(move || {
            let mut req = server
                .recv_timeout(Duration::from_secs(5))
                .expect("recv")
                .expect("requête");
            assert_eq!(req.url(), "/api/chat");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let value: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(value["model"], "modele-test");
            assert_eq!(value["stream"], false);
            assert_eq!(value["messages"][0]["role"], "system");
            assert_eq!(
                value["messages"][0]["content"],
                "Tu réponds uniquement à partir des documents fournis. Si ces documents ne permettent pas de répondre, dis-le. Pour chaque passage utilisé, cite le nom de fichier entre crochets, par exemple [France_grokipedia.md]. N’invente pas de source. Réponds en français, de façon concise."
            );
            let user = value["messages"][1]["content"].as_str().unwrap();
            assert!(user.contains("## fichier: index.html"), "{user}");
            assert!(user.contains("Bonjour le texte."), "{user}");
            assert!(!user.contains("nope()"), "{user}");
            assert!(user.contains("Question : Résume."), "{user}");
            saw_t.store(true, Ordering::SeqCst);
            let resp = r#"{"message":{"role":"assistant","content":"Réponse fixe."}}"#;
            req.respond(tiny_http::Response::from_string(resp)).unwrap();
        });

        let pack = LivePack::new(
            dir.path().to_path_buf(),
            false,
            9,
            Arc::new(OnceLock::new()),
        );
        pack.note(&"http://127.0.0.1:9/index.html".parse().unwrap());
        let reply = pack
            .question("", &format!("http://127.0.0.1:{port}"), Some("modele-test"))
            .unwrap();
        handle.join().unwrap();
        assert!(saw.load(Ordering::SeqCst));
        assert_eq!(reply.answer, "Réponse fixe.");
        assert_eq!(reply.files, vec!["index.html".to_string()]);
    }
}
