//! Interrogation locale d’un pack par Ollama.
//!
//! Le lot et la page courante restent en mémoire le temps du pack. Les
//! documents ne partent qu’avec la question, et uniquement vers l’hôte
//! configuré. La liste des modèles et la taille de fenêtre sont lues avant.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pulldown_cmark::{html, CowStr, Event, Options, Parser, Tag};
use serde::Serialize;

pub(crate) const DOC_CHARS: usize = 12_000;
pub(crate) const BATCH_MAX: usize = 8;
/// Silence maximale pendant une génération. Les jetons qui arrivent
/// repoussent cette limite : un modèle lent peut donc dépasser deux minutes.
const CHAT_TIMEOUT: Duration = Duration::from_secs(600);
const TAGS_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) const SYSTEM_PROMPT: &str = "\
Tu réponds uniquement à partir des documents fournis. Si ces documents ne permettent pas de répondre, dis-le. Pour chaque passage utilisé, cite le nom de fichier entre crochets, par exemple [France_grokipedia.md]. N’invente pas de source. Réponds en français, de façon concise.";

pub(crate) const DEFAULT_OLLAMA_HOST: &str = "http://127.0.0.1:11434";

#[derive(Serialize)]
pub(crate) struct AskView {
    pub current: Option<String>,
    pub batch: Vec<String>,
    pub model: Option<String>,
    /// Taille de la fenêtre de contexte, en jetons.
    pub context_tokens: Option<u64>,
    /// Jetons du dernier prompt, comptés par Ollama.
    pub prompt_tokens: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelView {
    pub models: Vec<String>,
    pub model: Option<String>,
    pub context_tokens: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AskReply {
    pub answer: String,
    /// HTML sûr, produit depuis `answer`.
    pub html: String,
    pub files: Vec<String>,
    pub prompt_tokens: Option<u64>,
    pub context_tokens: Option<u64>,
}

#[derive(Serialize)]
pub(crate) struct AnswerPage {
    pub html: String,
}

pub(crate) struct LivePack {
    pub root: PathBuf,
    pub markdown: bool,
    pub port: u16,
    pub index: Arc<OnceLock<crate::md::MdIndex>>,
    last_path: Mutex<Option<String>>,
    current: Mutex<Option<String>>,
    batch: Mutex<Vec<String>>,
    model: Mutex<Option<String>>,
    context_tokens: Mutex<Option<u64>>,
    prompt_tokens: Mutex<Option<u64>>,
    /// Markdown de la dernière réponse réussie.
    answer: Mutex<Option<String>>,
    /// Markdown affiché par la fenêtre Réponse. Une question plus récente
    /// ne change pas ce que cette fenêtre enregistre.
    shown: Mutex<Option<String>>,
    /// Dossier proposé au panneau. Pour une archive, ce n’est pas l’extraction.
    save_dir: PathBuf,
    /// Dernier choix de modèle. Une réponse Ollama tardive ne réécrit pas
    /// un choix commencé après.
    choice_gen: Mutex<u64>,
}

impl LivePack {
    pub(crate) fn new(
        root: PathBuf,
        markdown: bool,
        port: u16,
        index: Arc<OnceLock<crate::md::MdIndex>>,
    ) -> Self {
        let save_dir = root.clone();
        Self {
            root,
            markdown,
            port,
            index,
            last_path: Mutex::new(None),
            current: Mutex::new(None),
            batch: Mutex::new(Vec::new()),
            model: Mutex::new(None),
            context_tokens: Mutex::new(None),
            prompt_tokens: Mutex::new(None),
            answer: Mutex::new(None),
            shown: Mutex::new(None),
            save_dir,
            choice_gen: Mutex::new(0),
        }
    }

    /// Une archive s’enregistre à côté du fichier, pas dans l’extraction.
    pub(crate) fn point_save_at(&mut self, opened: &Path) {
        self.save_dir = durable_save_dir(opened, &self.root);
    }

    pub(crate) fn save_directory(&self) -> &Path {
        &self.save_dir
    }

    /// Vrai quand le fichier disparaîtrait avec le dossier temporaire d’une archive.
    pub(crate) fn save_lands_in_temp(&self, path: &Path) -> bool {
        if self.save_dir == self.root {
            return false;
        }
        let Some(parent) = path.parent() else {
            return false;
        };
        let root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        let parent = parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf());
        parent.starts_with(&root)
    }

    fn begin_choice(&self) -> u64 {
        let mut gen = lock(&self.choice_gen);
        *gen = gen.wrapping_add(1);
        *gen
    }

    fn choice_current(&self, gen: u64) -> bool {
        *lock(&self.choice_gen) == gen
    }

    fn model_view(&self, models: Vec<String>) -> ModelView {
        ModelView {
            models,
            model: lock(&self.model).clone(),
            context_tokens: *lock(&self.context_tokens),
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
            model: lock(&self.model).clone(),
            context_tokens: *lock(&self.context_tokens),
            prompt_tokens: *lock(&self.prompt_tokens),
        }
    }

    /// Liste les modèles et retient le choix déjà fait, sinon le modèle
    /// préféré, sinon le premier. La fenêtre est lue sans envoyer de document.
    pub(crate) fn models(&self, host: &str, preferred: Option<&str>) -> Result<ModelView, String> {
        let names = fetch_model_names(host)?;
        if names.is_empty() {
            return Err("Aucun modèle Ollama n’est installé.".into());
        }
        let stored = lock(&self.model).clone();
        let Some(model) = pick_installed(&names, stored.as_deref(), preferred) else {
            return Err("Aucun modèle Ollama n’est installé.".into());
        };
        let changed = stored.as_deref() != Some(model.as_str());
        let known = *lock(&self.context_tokens);
        if !changed && known.is_some() {
            return Ok(ModelView {
                models: names,
                model: Some(model),
                context_tokens: known,
            });
        }
        // Rafraîchir une fenêtre inconnue ne doit pas annuler un choix
        // commencé pendant le GET. On ne réserve la génération que pour
        // remplacer le modèle.
        let claim = if changed {
            Some(self.begin_choice())
        } else {
            None
        };
        let window = fetch_context_window(host, &model).ok().flatten();
        if let Some(gen) = claim {
            if !self.choice_current(gen) {
                return Ok(self.model_view(names));
            }
            let already = lock(&self.model).clone();
            if already.as_deref() == Some(model.as_str()) {
                if lock(&self.context_tokens).is_none() {
                    *lock(&self.context_tokens) = window;
                }
            } else {
                *lock(&self.model) = Some(model);
                *lock(&self.context_tokens) = window;
                *lock(&self.prompt_tokens) = None;
            }
            return Ok(self.model_view(names));
        }
        if lock(&self.model).as_deref() == Some(model.as_str())
            && lock(&self.context_tokens).is_none()
        {
            *lock(&self.context_tokens) = window;
        }
        Ok(self.model_view(names))
    }

    pub(crate) fn set_model(&self, host: &str, model: &str) -> Result<ModelView, String> {
        let model = model.trim();
        if model.is_empty() || model.chars().any(char::is_control) {
            return Err("Modèle Ollama illisible.".into());
        }
        let gen = self.begin_choice();
        let names = fetch_model_names(host)?;
        if !self.choice_current(gen) {
            return Ok(self.model_view(names));
        }
        if !names.iter().any(|name| name == model) {
            return Err("Ce modèle n’est pas installé.".into());
        }
        let window = fetch_context_window(host, model).ok().flatten();
        if !self.choice_current(gen) {
            return Ok(self.model_view(names));
        }
        *lock(&self.model) = Some(model.to_string());
        *lock(&self.context_tokens) = window;
        *lock(&self.prompt_tokens) = None;
        Ok(ModelView {
            models: names,
            model: Some(model.to_string()),
            context_tokens: window,
        })
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
        let selected = lock(&self.model)
            .clone()
            .filter(|name| !name.trim().is_empty());
        let model = if let Some(model) = selected {
            model
        } else {
            let names = fetch_model_names(host)?;
            pick_installed(&names, None, model)
                .ok_or_else(|| "Aucun modèle Ollama n’est installé.".to_string())?
        };
        let (answer, prompt_tokens) = post_chat(host, &model, &user)?;
        let loaded = running_context(host, &model);
        // Le choix a pu changer pendant la question.
        let current = lock(&self.model).clone();
        let still = match current.as_deref() {
            Some(name) => name == model,
            None => true,
        };
        if still {
            if let Some(loaded) = loaded {
                *lock(&self.context_tokens) = Some(loaded);
            }
            if current.is_none() {
                *lock(&self.model) = Some(model);
            }
            *lock(&self.prompt_tokens) = prompt_tokens;
        }
        *lock(&self.answer) = Some(answer.clone());
        let html = render_answer(&answer);
        Ok(AskReply {
            answer,
            html,
            files: rels,
            prompt_tokens: still.then_some(prompt_tokens).flatten(),
            context_tokens: *lock(&self.context_tokens),
        })
    }

    pub(crate) fn answer_text(&self) -> Result<String, String> {
        lock(&self.answer)
            .clone()
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| "Aucune réponse.".to_string())
    }

    pub(crate) fn answer_page(&self) -> Result<AnswerPage, String> {
        let markdown = self.answer_text()?;
        *lock(&self.shown) = Some(markdown.clone());
        Ok(AnswerPage {
            html: render_answer(&markdown),
        })
    }

    /// Markdown que la fenêtre Réponse montre, pas la dernière question.
    pub(crate) fn shown_markdown(&self) -> Result<String, String> {
        lock(&self.shown)
            .clone()
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| "Aucune réponse.".to_string())
    }
}

/// Dossier durable pour le panneau. Une archive ouverte ne propose pas son extraction.
pub(crate) fn durable_save_dir(opened: &Path, served_root: &Path) -> PathBuf {
    if opened.is_file() {
        if let Some(parent) = opened.parent().filter(|parent| parent.is_dir()) {
            return parent.to_path_buf();
        }
        return fallback_save_dir();
    }
    if served_root.is_dir() {
        return served_root.to_path_buf();
    }
    if opened.is_dir() {
        return opened.to_path_buf();
    }
    fallback_save_dir()
}

fn fallback_save_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Choix du pack s’il est installé, sinon le préféré s’il l’est, sinon le premier.
fn pick_installed(
    names: &[String],
    stored: Option<&str>,
    preferred: Option<&str>,
) -> Option<String> {
    let installed = |name: &str| {
        let name = name.trim();
        !name.is_empty() && names.iter().any(|n| n == name)
    };
    stored
        .filter(|name| installed(name))
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| {
            preferred
                .filter(|name| installed(name))
                .map(str::trim)
                .map(str::to_string)
        })
        .or_else(|| names.first().cloned())
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

fn model_names(value: &serde_json::Value) -> Vec<String> {
    value["models"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter_map(|model| {
                    model["name"]
                        .as_str()
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Entier JSON, ou nombre décimal positif et entier. `0` est conservé :
/// l’appelant ignore une fenêtre nulle.
fn json_u64(value: &serde_json::Value) -> Option<u64> {
    let number = value.as_number()?;
    if let Some(n) = number.as_u64() {
        return Some(n);
    }
    let n = number.as_f64()?;
    if n.is_finite() && n > 0.0 && n.fract() == 0.0 {
        Some(n as u64)
    } else {
        None
    }
}

fn num_ctx_from_parameters(parameters: &str) -> Option<u64> {
    for line in parameters.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        for pair in parts.windows(2) {
            if pair[0] == "num_ctx" {
                if let Some(n) = pair[1].parse::<u64>().ok().filter(|n| *n > 0) {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// `num_ctx` du Modelfile l’emporte sur `*.context_length`.
fn context_window(value: &serde_json::Value) -> Option<u64> {
    if let Some(parameters) = value.get("parameters").and_then(|v| v.as_str()) {
        if let Some(n) = num_ctx_from_parameters(parameters) {
            return Some(n);
        }
    }
    value
        .get("model_info")
        .and_then(|info| info.as_object())
        .and_then(|info| {
            info.iter().find_map(|(key, val)| {
                key.ends_with(".context_length")
                    .then(|| json_u64(val).filter(|n| *n > 0))
                    .flatten()
            })
        })
}

fn fetch_model_names(host: &str) -> Result<Vec<String>, String> {
    let body = http_exchange(host, "GET", "/api/tags", None, TAGS_TIMEOUT)?;
    let value: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| "Réponse Ollama illisible.".to_string())?;
    Ok(model_names(&value))
}

fn fetch_context_window(host: &str, model: &str) -> Result<Option<u64>, String> {
    let payload = serde_json::json!({ "model": model });
    let bytes = serde_json::to_vec(&payload).map_err(|e| format!("json: {e}"))?;
    let body = http_exchange(host, "POST", "/api/show", Some(&bytes), TAGS_TIMEOUT)?;
    let value: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| "Réponse Ollama illisible.".to_string())?;
    Ok(context_window(&value))
}

/// Fenêtre réellement chargée après la question. Un échec est ignoré.
fn running_context(host: &str, model: &str) -> Option<u64> {
    let body = http_exchange(host, "GET", "/api/ps", None, TAGS_TIMEOUT).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&body).ok()?;
    let models = value.get("models")?.as_array()?;
    models.iter().find_map(|entry| {
        let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let loaded = entry.get("model").and_then(|v| v.as_str()).unwrap_or("");
        if name == model || loaded == model {
            entry
                .get("context_length")
                .and_then(json_u64)
                .filter(|n| *n > 0)
        } else {
            None
        }
    })
}

fn post_chat(host: &str, model: &str, user: &str) -> Result<(String, Option<u64>), String> {
    // Pas d’options.num_ctx : la fenêtre est affichée, pas agrandie.
    // Le flux garde la connexion active pendant la génération. think false
    // demande la réponse sans la trace de raisonnement, qui dépassait le délai.
    let payload = serde_json::json!({
        "model": model,
        "stream": true,
        "think": false,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": user}
        ]
    });
    let bytes = serde_json::to_vec(&payload).map_err(|e| format!("json: {e}"))?;
    let body = http_exchange(host, "POST", "/api/chat", Some(&bytes), CHAT_TIMEOUT)?;
    parse_chat_body(&body, host)
}

/// Concatène les deltas de `/api/chat`. Un objet unique, sans flux, convient aussi.
/// Sans `"done": true`, le texte déjà reçu n’est pas une réponse.
fn parse_chat_body(body: &[u8], host: &str) -> Result<(String, Option<u64>), String> {
    let text = std::str::from_utf8(body).map_err(|_| "Réponse Ollama illisible.".to_string())?;
    let mut answer = String::new();
    let mut prompt_tokens = None;
    let mut saw = false;
    let mut done = false;
    for line in text.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|_| "Réponse Ollama illisible.".to_string())?;
        if let Some(err) = value.get("error").and_then(|v| v.as_str()).map(str::trim) {
            if !err.is_empty() {
                return Err(format!("Ollama : {err}"));
            }
        }
        if let Some(chunk) = value
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(|content| content.as_str())
        {
            answer.push_str(chunk);
        }
        if let Some(n) = json_u64(&value["prompt_eval_count"]).filter(|n| *n > 0) {
            prompt_tokens = Some(n);
        }
        if value.get("done").and_then(|flag| flag.as_bool()) == Some(true) {
            done = true;
        }
        saw = true;
    }
    if !saw {
        return Err("Réponse Ollama illisible.".into());
    }
    if !done {
        return Err(format!("Ollama n’a pas terminé à temps ({host})."));
    }
    if answer.is_empty() {
        return Err("Ollama n’a pas renvoyé de réponse.".into());
    }
    Ok((answer, prompt_tokens))
}

/// HTML de la réponse. Le HTML brut du modèle devient du texte.
pub(crate) fn render_answer(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(markdown, options);
    let events = parser.map(|event| match event {
        Event::Html(text) | Event::InlineHtml(text) => Event::Text(text),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: safe_url(dest_url),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url: _,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: CowStr::from("#"),
            title,
            id,
        }),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    out
}

fn safe_url(dest: CowStr<'_>) -> CowStr<'static> {
    let trimmed = dest.trim();
    let lower = trimmed.to_ascii_lowercase();
    let http = lower.starts_with("https://") || lower.starts_with("http://");
    if http
        && !trimmed
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        CowStr::from(trimmed.to_string())
    } else {
        CowStr::from("#")
    }
}

/// Ajoute `.md` seulement quand le chemin choisi n’a pas d’extension.
pub(crate) fn markdown_save_path(path: PathBuf) -> PathBuf {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if !ext.is_empty() => path,
        _ => path.with_extension("md"),
    }
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
    let connect_for = timeout.min(CONNECT_TIMEOUT);
    let mut stream = TcpStream::connect_timeout(&sock, connect_for)
        .map_err(|_| format!("Ollama ne répond pas à {host}."))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|_| format!("Ollama ne répond pas à {host}."))?;
    stream
        .set_write_timeout(Some(connect_for))
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
    let buf = read_http(&mut stream, host)?;
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
    let rest = &buf[sep + 4..];
    let chunked = head.to_ascii_lowercase().lines().any(|line| {
        let line = line.trim();
        line.starts_with("transfer-encoding:") && line.contains("chunked")
    });
    let payload = if chunked {
        decode_chunked(rest)?
    } else if let Some(len) = content_length(head) {
        if rest.len() < len {
            return Err(format!("Ollama n’a pas terminé à temps ({host})."));
        }
        rest[..len].to_vec()
    } else {
        rest.to_vec()
    };
    if !(200..300).contains(&status) {
        return Err(status_error(status, &payload));
    }
    Ok(payload)
}

/// Lit jusqu’à une réponse complète. Un délai sans aucun octet, ou au milieu
/// d’une réponse inachevée, n’est pas une absence d’Ollama.
fn read_http(stream: &mut TcpStream, host: &str) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => {
                if buf.is_empty() {
                    return Err(format!("Ollama ne répond pas à {host}."));
                }
                if message_complete(&buf) || close_delimited(&buf) {
                    break;
                }
                return Err(format!("Ollama n’a pas terminé à temps ({host})."));
            }
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if message_complete(&buf) {
                    break;
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err)
                if err.kind() == ErrorKind::TimedOut || err.kind() == ErrorKind::WouldBlock =>
            {
                if message_complete(&buf) {
                    break;
                }
                return Err(format!("Ollama n’a pas terminé à temps ({host})."));
            }
            Err(_) => return Err(format!("Ollama ne répond pas à {host}.")),
        }
    }
    if buf.is_empty() {
        return Err(format!("Ollama ne répond pas à {host}."));
    }
    Ok(buf)
}

fn message_complete(buf: &[u8]) -> bool {
    let Some(sep) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let Ok(head) = std::str::from_utf8(&buf[..sep]) else {
        return false;
    };
    let body = &buf[sep + 4..];
    if head.to_ascii_lowercase().lines().any(|line| {
        let line = line.trim();
        line.starts_with("transfer-encoding:") && line.contains("chunked")
    }) {
        return chunked_finished(body);
    }
    content_length(head).is_some_and(|len| body.len() >= len)
}

/// Fin de réponse seulement quand ni chunked ni Content-Length ne bornent le corps.
fn close_delimited(buf: &[u8]) -> bool {
    let Some(sep) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let Ok(head) = std::str::from_utf8(&buf[..sep]) else {
        return false;
    };
    let chunked = head.to_ascii_lowercase().lines().any(|line| {
        let line = line.trim();
        line.starts_with("transfer-encoding:") && line.contains("chunked")
    });
    !chunked && content_length(head).is_none()
}

fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}

fn chunked_finished(input: &[u8]) -> bool {
    let mut i = 0;
    while i < input.len() {
        let Some(line_end) = input[i..].windows(2).position(|w| w == b"\r\n") else {
            return false;
        };
        let Ok(size_txt) = std::str::from_utf8(&input[i..i + line_end]) else {
            return false;
        };
        let size_txt = size_txt.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_txt, 16) else {
            return false;
        };
        let Some(after_size) = i.checked_add(line_end + 2) else {
            return false;
        };
        i = after_size;
        if size == 0 {
            return true;
        }
        let Some(next) = i.checked_add(size).and_then(|end| end.checked_add(2)) else {
            return false;
        };
        if next > input.len() {
            return false;
        }
        i = next;
    }
    false
}

fn status_error(status: u16, body: &[u8]) -> String {
    let detail = std::str::from_utf8(body)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text.trim()).ok())
        .and_then(|value| {
            value
                .get("error")
                .and_then(|err| err.as_str())
                .map(str::trim)
                .filter(|err| !err.is_empty())
                .map(str::to_string)
        });
    match detail {
        Some(detail) => {
            let short: String = detail.chars().take(180).collect();
            format!("Ollama a répondu {status} : {short}")
        }
        None => format!("Ollama a répondu {status}."),
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
    let mut finished = false;
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
        i = i
            .checked_add(line_end + 2)
            .ok_or_else(|| "Réponse Ollama illisible.".to_string())?;
        if size == 0 {
            finished = true;
            break;
        }
        let end = i
            .checked_add(size)
            .ok_or_else(|| "Réponse Ollama illisible.".to_string())?;
        if end > input.len() {
            return Err("Réponse Ollama illisible.".into());
        }
        out.extend_from_slice(&input[i..end]);
        i = end
            .checked_add(2)
            .ok_or_else(|| "Réponse Ollama illisible.".to_string())?;
    }
    if !finished {
        return Err("Réponse Ollama illisible.".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
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
        let handle = thread::spawn(move || loop {
            let Some(mut req) = server.recv_timeout(Duration::from_secs(5)).expect("recv") else {
                break;
            };
            if req.url() == "/api/tags" {
                req.respond(tiny_http::Response::from_string(
                    r#"{"models":[{"name":"modele-test"}]}"#,
                ))
                .unwrap();
                continue;
            }
            if req.url() == "/api/ps" {
                req.respond(tiny_http::Response::from_string(r#"{"models":[]}"#))
                    .unwrap();
                break;
            }
            assert_eq!(req.url(), "/api/chat");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let value: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(value["model"], "modele-test");
            assert_eq!(value["stream"], true);
            assert_eq!(value["think"], false);
            assert!(value.get("options").is_none(), "{value}");
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
            let resp = r#"{"message":{"role":"assistant","content":"Réponse fixe."},"done":true}"#;
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
        assert!(
            reply.html.contains("Réponse fixe."),
            "{html}",
            html = reply.html
        );
        assert!(
            !reply.html.to_lowercase().contains("<script"),
            "{}",
            reply.html
        );
        assert_eq!(pack.answer_text().unwrap(), "Réponse fixe.");
        assert_eq!(reply.files, vec!["index.html".to_string()]);
        assert_eq!(reply.prompt_tokens, None);
    }

    #[test]
    fn context_window_prefers_num_ctx_and_ignores_zero() {
        let names = model_names(&serde_json::json!({
            "models": [
                {"name": " un "},
                {"name": ""},
                {"model": "sans-nom"},
                {"name": "deux"}
            ]
        }));
        assert_eq!(names, vec!["un".to_string(), "deux".to_string()]);

        let both = serde_json::json!({
            "parameters": "temperature 0.2\nnum_ctx 8192\n",
            "model_info": {"llama.context_length": 131072}
        });
        assert_eq!(context_window(&both), Some(8192));

        let zero = serde_json::json!({
            "parameters": "num_ctx 0",
            "model_info": {"qwen2.context_length": 32768}
        });
        assert_eq!(context_window(&zero), Some(32768));

        let floated = serde_json::json!({
            "model_info": {
                "general.architecture": "llama",
                "llama.context_length": 4096.0
            }
        });
        assert_eq!(context_window(&floated), Some(4096));
        assert_eq!(context_window(&serde_json::json!({})), None);

        assert_eq!(json_u64(&serde_json::json!(12)), Some(12));
        assert_eq!(json_u64(&serde_json::json!(12.0)), Some(12));
        assert_eq!(json_u64(&serde_json::json!(0)), Some(0));
        assert_eq!(json_u64(&serde_json::json!(-3)), None);
        assert_eq!(json_u64(&serde_json::json!(1.5)), None);
        assert_eq!(json_u64(&serde_json::json!("12")), None);
        assert_eq!(
            json_u64(&serde_json::json!({"prompt_eval_count": 321})["prompt_eval_count"]),
            Some(321)
        );
    }

    #[test]
    fn no_installed_model_is_a_french_error() {
        let (port, server) = listen();
        let handle = thread::spawn(move || {
            let req = recv(&server);
            assert_eq!(req.url(), "/api/tags");
            req.respond(tiny_http::Response::from_string(r#"{"models":[]}"#))
                .unwrap();
        });
        let pack = LivePack::new(PathBuf::from("."), false, 9, Arc::new(OnceLock::new()));
        let err = pack
            .models(&format!("http://127.0.0.1:{port}"), None)
            .unwrap_err();
        handle.join().unwrap();
        assert_eq!(err, "Aucun modèle Ollama n’est installé.");
    }

    #[test]
    fn blank_model_is_refused_before_any_request() {
        let pack = LivePack::new(PathBuf::from("."), false, 9, Arc::new(OnceLock::new()));
        let err = pack.set_model("http://127.0.0.1:9", " \t ").unwrap_err();
        assert_eq!(err, "Modèle Ollama illisible.");
        let err = pack.set_model("http://127.0.0.1:9", "mi\nmo").unwrap_err();
        assert_eq!(err, "Modèle Ollama illisible.");
    }

    fn listen() -> (u16, tiny_http::Server) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = match server.server_addr() {
            tiny_http::ListenAddr::IP(addr) => addr.port(),
            other => panic!("adresse inattendue: {other:?}"),
        };
        (port, server)
    }

    fn recv(server: &tiny_http::Server) -> tiny_http::Request {
        server
            .recv_timeout(Duration::from_secs(5))
            .expect("recv")
            .expect("requête")
    }

    #[test]
    fn chosen_model_keeps_num_ctx_and_counts_prompt_tokens() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("index.html"), b"<p>Bonjour.</p>").unwrap();
        let (port, server) = listen();
        let handle = thread::spawn(move || {
            let tags = r#"{"models":[{"name":"un"},{"name":"deux"}]}"#;
            let req = recv(&server);
            assert_eq!(req.method().as_str(), "GET");
            assert_eq!(req.url(), "/api/tags");
            req.respond(tiny_http::Response::from_string(tags)).unwrap();

            let req = recv(&server);
            assert_eq!(req.url(), "/api/tags");
            req.respond(tiny_http::Response::from_string(tags)).unwrap();

            let mut req = recv(&server);
            assert_eq!(req.method().as_str(), "POST");
            assert_eq!(req.url(), "/api/show");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let value: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(value["model"], "deux");
            assert!(!body.contains("Bonjour"), "{body}");
            req.respond(tiny_http::Response::from_string(
                r#"{"parameters":"num_ctx 8192\n","model_info":{"llama.context_length":131072}}"#,
            ))
            .unwrap();

            let req = recv(&server);
            assert_eq!(req.url(), "/api/tags");
            req.respond(tiny_http::Response::from_string(tags)).unwrap();

            let mut req = recv(&server);
            assert_eq!(req.url(), "/api/chat");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let value: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(value["model"], "deux");
            assert_eq!(value["stream"], true);
            assert_eq!(value["think"], false);
            assert!(value.get("options").is_none(), "{value}");
            req.respond(tiny_http::Response::from_string(
                r#"{"message":{"role":"assistant","content":"Vu."},"prompt_eval_count":321,"done":true}"#,
            ))
            .unwrap();

            let req = recv(&server);
            assert_eq!(req.url(), "/api/ps");
            req.respond(tiny_http::Response::from_string(
                r#"{"models":[{"name":"un","context_length":99},{"model":"deux","context_length":4096}]}"#,
            ))
            .unwrap();
        });

        let host = format!("http://127.0.0.1:{port}");
        let pack = LivePack::new(
            dir.path().to_path_buf(),
            false,
            9,
            Arc::new(OnceLock::new()),
        );
        let err = pack.set_model(&host, "absent").unwrap_err();
        assert!(err.contains("pas installé"), "{err}");

        let chosen = pack.set_model(&host, "deux").unwrap();
        assert_eq!(chosen.models, vec!["un".to_string(), "deux".to_string()]);
        assert_eq!(chosen.model.as_deref(), Some("deux"));
        assert_eq!(chosen.context_tokens, Some(8192));

        let listed = pack.models(&host, Some("un")).unwrap();
        assert_eq!(listed.model.as_deref(), Some("deux"));
        assert_eq!(listed.context_tokens, Some(8192));

        pack.note(&"http://127.0.0.1:9/index.html".parse().unwrap());
        let reply = pack.question("quoi", &host, Some("un")).unwrap();
        handle.join().unwrap();
        assert_eq!(reply.answer, "Vu.");
        assert_eq!(reply.prompt_tokens, Some(321));
        assert_eq!(reply.context_tokens, Some(4096));
        assert_eq!(reply.files, vec!["index.html".to_string()]);
        let view = pack.view();
        assert_eq!(view.model.as_deref(), Some("deux"));
        assert_eq!(view.prompt_tokens, Some(321));
        assert_eq!(view.context_tokens, Some(4096));
    }

    fn html_pack() -> (tempfile::TempDir, LivePack) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("index.html"), b"<p>Bonjour.</p>").unwrap();
        let pack = LivePack::new(
            dir.path().to_path_buf(),
            false,
            9,
            Arc::new(OnceLock::new()),
        );
        pack.note(&"http://127.0.0.1:9/index.html".parse().unwrap());
        (dir, pack)
    }

    #[test]
    fn question_skips_an_uninstalled_preferred_model() {
        let (_dir, pack) = html_pack();
        let (port, server) = listen();
        let handle = thread::spawn(move || {
            let req = recv(&server);
            assert_eq!(req.url(), "/api/tags");
            req.respond(tiny_http::Response::from_string(
                r#"{"models":[{"name":"un"}]}"#,
            ))
            .unwrap();
            let mut req = recv(&server);
            assert_eq!(req.url(), "/api/chat");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let value: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(value["model"], "un");
            req.respond(tiny_http::Response::from_string(
                r#"{"message":{"role":"assistant","content":"Vu."},"prompt_eval_count":4,"done":true}"#,
            ))
            .unwrap();
            let req = recv(&server);
            assert_eq!(req.url(), "/api/ps");
            req.respond(tiny_http::Response::from_string(r#"{"models":[]}"#))
                .unwrap();
        });
        let reply = pack
            .question("quoi", &format!("http://127.0.0.1:{port}"), Some("absent"))
            .unwrap();
        handle.join().unwrap();
        assert_eq!(reply.answer, "Vu.");
        assert_eq!(reply.prompt_tokens, Some(4));
        assert_eq!(pack.view().model.as_deref(), Some("un"));
    }

    #[test]
    fn failed_question_does_not_keep_the_fallback_model() {
        let (_dir, pack) = html_pack();
        let (port, server) = listen();
        let handle = thread::spawn(move || {
            let req = recv(&server);
            assert_eq!(req.url(), "/api/tags");
            req.respond(tiny_http::Response::from_string(
                r#"{"models":[{"name":"un"}]}"#,
            ))
            .unwrap();
            let req = recv(&server);
            assert_eq!(req.url(), "/api/chat");
            req.respond(tiny_http::Response::from_string("non").with_status_code(500))
                .unwrap();
        });
        let err = pack
            .question("quoi", &format!("http://127.0.0.1:{port}"), Some("absent"))
            .unwrap_err();
        handle.join().unwrap();
        assert!(err.contains("500"), "{err}");
        assert_eq!(pack.view().model, None);
    }

    #[test]
    fn inflight_question_does_not_overwrite_a_newer_model() {
        use std::sync::atomic::AtomicBool;

        let (_dir, pack) = html_pack();
        let pack = Arc::new(pack);
        let (port, server) = listen();
        let chat_ready = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let chat_ready_s = chat_ready.clone();
        let release_s = release.clone();
        let handle = thread::spawn(move || {
            let tags = r#"{"models":[{"name":"un"},{"name":"deux"}]}"#;
            let req = recv(&server);
            assert_eq!(req.url(), "/api/tags");
            req.respond(tiny_http::Response::from_string(tags)).unwrap();

            let mut chat = recv(&server);
            assert_eq!(chat.url(), "/api/chat");
            let mut body = String::new();
            chat.as_reader().read_to_string(&mut body).unwrap();
            let value: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(value["model"], "un");
            chat_ready_s.store(true, Ordering::SeqCst);

            loop {
                if release_s.load(Ordering::SeqCst) {
                    break;
                }
                match server.recv_timeout(Duration::from_millis(50)) {
                    Ok(Some(mut req)) => {
                        if req.url() == "/api/show" {
                            let mut show_body = String::new();
                            req.as_reader().read_to_string(&mut show_body).unwrap();
                            assert!(show_body.contains("deux"), "{show_body}");
                            req.respond(tiny_http::Response::from_string(
                                r#"{"parameters":"num_ctx 8192"}"#,
                            ))
                            .unwrap();
                        } else {
                            assert_eq!(req.url(), "/api/tags");
                            req.respond(tiny_http::Response::from_string(tags)).unwrap();
                        }
                    }
                    Ok(None) => {}
                    Err(err) => panic!("recv: {err}"),
                }
            }
            chat.respond(tiny_http::Response::from_string(
                r#"{"message":{"role":"assistant","content":"Tard."},"prompt_eval_count":77,"done":true}"#,
            ))
            .unwrap();
            let req = recv(&server);
            assert_eq!(req.url(), "/api/ps");
            req.respond(tiny_http::Response::from_string(
                r#"{"models":[{"name":"un","context_length":9999}]}"#,
            ))
            .unwrap();
        });

        let host = format!("http://127.0.0.1:{port}");
        let asking = pack.clone();
        let host_q = host.clone();
        let question = thread::spawn(move || asking.question("quoi", &host_q, Some("absent")));
        let wait_started = std::time::Instant::now();
        while !chat_ready.load(Ordering::SeqCst) {
            if wait_started.elapsed() > Duration::from_secs(5) {
                panic!("la question n’a pas atteint /api/chat");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let chosen = pack.set_model(&host, "deux").unwrap();
        assert_eq!(chosen.model.as_deref(), Some("deux"));
        assert_eq!(chosen.context_tokens, Some(8192));
        release.store(true, Ordering::SeqCst);
        let reply = question.join().unwrap().unwrap();
        handle.join().unwrap();
        assert_eq!(reply.answer, "Tard.");
        assert_eq!(reply.prompt_tokens, None);
        let view = pack.view();
        assert_eq!(view.model.as_deref(), Some("deux"));
        assert_eq!(view.prompt_tokens, None);
        assert_eq!(view.context_tokens, Some(8192));
    }

    #[test]
    fn answer_markdown_renders_and_raw_html_stays_text() {
        let html = render_answer(
            "# Titre\n\nUn **mot**.\n\n<script>alert(1)</script>\n\n[lien](javascript:alert(1))\n\n![img](https://example.test/a.png)\n\n[doc](https://example.test/a)\n",
        );
        assert!(html.contains("<h1>"), "{html}");
        assert!(html.contains("<strong>mot</strong>"), "{html}");
        assert!(!html.to_lowercase().contains("<script"), "{html}");
        assert!(html.contains("alert(1)"), "{html}");
        assert!(!html.to_lowercase().contains("javascript:"), "{html}");
        assert!(!html.contains("example.test/a.png"), "{html}");
        assert!(html.contains("https://example.test/a"), "{html}");
        assert_eq!(
            markdown_save_path(PathBuf::from("reponse"))
                .extension()
                .unwrap(),
            "md"
        );
        assert_eq!(
            markdown_save_path(PathBuf::from("note.txt"))
                .extension()
                .unwrap(),
            "txt"
        );
        assert_eq!(
            markdown_save_path(PathBuf::from("deja.md"))
                .file_name()
                .unwrap(),
            "deja.md"
        );
        let pack = LivePack::new(PathBuf::from("."), false, 9, Arc::new(OnceLock::new()));
        assert!(pack.answer_text().unwrap_err().contains("Aucune"));
    }

    #[test]
    fn streamed_chat_joins_deltas_and_reads_prompt_tokens() {
        let body = b"{\"message\":{\"content\":\"R\xc3\xa9\"},\"done\":false}\n{\"message\":{\"content\":\"ponse.\"},\"done\":false}\n{\"message\":{\"content\":\"\"},\"done\":true,\"prompt_eval_count\":321}\n";
        let (answer, tokens) = parse_chat_body(body, "http://127.0.0.1:9").unwrap();
        assert_eq!(answer, "Réponse.");
        assert_eq!(tokens, Some(321));

        let err = parse_chat_body(
            r#"{"error":"mémoire insuffisante"}"#.as_bytes(),
            "http://127.0.0.1:9",
        )
        .unwrap_err();
        assert!(err.contains("mémoire insuffisante"), "{err}");
        let err = parse_chat_body(
            br#"{"message":{"content":""},"done":true}"#,
            "http://127.0.0.1:9",
        )
        .unwrap_err();
        assert!(err.contains("pas renvoyé"), "{err}");
        let err = parse_chat_body(
            br#"{"message":{"content":"partiel"},"done":false}"#,
            "http://127.0.0.1:9",
        )
        .unwrap_err();
        assert!(err.contains("pas terminé"), "{err}");
    }

    #[test]
    fn http_message_is_complete_once_the_body_has_arrived() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert!(message_complete(raw));
        assert!(!message_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel"
        ));
        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        assert!(message_complete(chunked));
        assert!(!message_complete(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n"
        ));
    }

    #[test]
    fn open_connection_returns_as_soon_as_content_length_is_reached() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf);
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: keep-alive\r\n\r\n{\"ok\":true}")
                .unwrap();
            thread::sleep(Duration::from_millis(800));
        });
        let started = std::time::Instant::now();
        let body = http_exchange(
            &format!("http://127.0.0.1:{port}"),
            "GET",
            "/api/tags",
            None,
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "attente {:?}",
            started.elapsed()
        );
        assert_eq!(body, b"{\"ok\":true}");
        handle.join().unwrap();
    }

    #[test]
    fn silent_ollama_is_still_working_not_absent() {
        let (port, server) = listen();
        let handle = thread::spawn(move || {
            let _req = recv(&server);
            thread::sleep(Duration::from_millis(800));
        });
        let err = http_exchange(
            &format!("http://127.0.0.1:{port}"),
            "GET",
            "/api/tags",
            None,
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(err.contains("pas terminé"), "{err}");
        assert!(!err.contains("ne répond pas"), "{err}");
        handle.join().unwrap();
    }

    #[test]
    fn closed_chunk_without_terminator_is_unfinished() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf);
            sock.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n")
                .unwrap();
        });
        let err = http_exchange(
            &format!("http://127.0.0.1:{port}"),
            "GET",
            "/api/tags",
            None,
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(err.contains("pas terminé"), "{err}");
        handle.join().unwrap();
        assert!(decode_chunked(b"5\r\nhello\r\n").is_err());
        assert_eq!(
            decode_chunked(b"5\r\nhello\r\n0\r\n\r\n").unwrap(),
            b"hello"
        );
        assert!(!chunked_finished(b"ffffffffffffffff\r\n"));
    }

    #[test]
    fn short_content_length_then_close_is_unfinished() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf);
            sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\"",
            )
            .unwrap();
        });
        let err = http_exchange(
            &format!("http://127.0.0.1:{port}"),
            "GET",
            "/api/tags",
            None,
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(err.contains("pas terminé"), "{err}");
        handle.join().unwrap();
    }

    #[test]
    fn close_delimited_body_is_complete() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf);
            sock.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"ok\":true}")
                .unwrap();
        });
        let body = http_exchange(
            &format!("http://127.0.0.1:{port}"),
            "GET",
            "/api/tags",
            None,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(body, b"{\"ok\":true}");
        handle.join().unwrap();
    }

    #[test]
    fn reply_keeps_the_markdown_it_showed() {
        let pack = LivePack::new(PathBuf::from("."), false, 9, Arc::new(OnceLock::new()));
        *lock(&pack.answer) = Some("Ancien.".into());
        let page = pack.answer_page().unwrap();
        assert!(page.html.contains("Ancien"), "{}", page.html);
        *lock(&pack.answer) = Some("Nouveau.".into());
        assert_eq!(pack.shown_markdown().unwrap(), "Ancien.");
        assert_eq!(pack.answer_text().unwrap(), "Nouveau.");
    }

    #[test]
    fn archive_save_starts_beside_the_file_not_in_the_extraction() {
        let archive_dir = tempfile::tempdir().unwrap();
        let zip = archive_dir.path().join("site.zip");
        fs::write(&zip, b"zip").unwrap();
        let extracted = tempfile::tempdir().unwrap();
        let mut pack = LivePack::new(
            extracted.path().to_path_buf(),
            false,
            9,
            Arc::new(OnceLock::new()),
        );
        pack.point_save_at(&zip);
        assert_eq!(pack.save_directory(), archive_dir.path());
        assert!(pack.save_lands_in_temp(&extracted.path().join("reponse.md")));
        assert!(!pack.save_lands_in_temp(&archive_dir.path().join("reponse.md")));

        let folder = tempfile::tempdir().unwrap();
        let mut folder_pack = LivePack::new(
            folder.path().to_path_buf(),
            false,
            9,
            Arc::new(OnceLock::new()),
        );
        folder_pack.point_save_at(folder.path());
        assert_eq!(folder_pack.save_directory(), folder.path());
        assert!(!folder_pack.save_lands_in_temp(&folder.path().join("reponse.md")));
    }
}
