//! Lecteur d’un corpus de droit français tenu dans un fichier `.sqlite`.
//!
//! La base est ouverte en lecture seule. Les pages viennent des lignes :
//! rien n’est décompressé, rien n’est exporté. Un autre fichier sqlite,
//! sans la table `documents`, la vue `articles_vigueur` et la table
//! `documents_fts`, est refusé.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use tiny_http::{Header, Request, Response, StatusCode};

use crate::progress::Reporter;

const SENTINEL_END: &str = "2999-01-01";
const SEARCH_LIMIT: usize = 40;

pub(crate) const SYSTEM_PROMPT: &str = "\
Tu réponds uniquement à partir des documents fournis. S’ils ne suffisent pas pour répondre, dis-le. Cite chaque article utilisé sous la forme « Code civil, art. 1240, en vigueur depuis le 2016-10-01 ». N’invente pas de source. Réponds en français, de façon concise.";

const STYLE: &str = r#"
:root { color-scheme: light dark; }
body { font: 1.05rem/1.55 Georgia, "Iowan Old Style", serif; max-width: 42rem; margin: 2rem auto; padding: 0 1.2rem; }
.nav, form, .meta, .hit { font-family: ui-sans-serif, system-ui, sans-serif; font-size: 0.92rem; }
h1, h2 { line-height: 1.2; }
label { display: block; margin: 1rem 0 0.4rem; }
input[type=search], select { width: 100%; box-sizing: border-box; padding: 0.45rem 0.55rem; font: inherit; }
button { margin-top: 0.7rem; font: inherit; }
a { color: inherit; }
.meta { opacity: 0.75; }
.texte { white-space: pre-wrap; overflow-wrap: anywhere; }
mark { background: rgba(127,127,127,0.28); color: inherit; }
"#;

const SCHEMA_ERROR: &str = "\
Ce fichier .sqlite n’est pas un corpus de droit : il manque la table documents, la vue articles_vigueur ou la table documents_fts.";

struct Article {
    id: String,
    code_titre: String,
    nature: String,
    chemin: String,
    num: String,
    etat: String,
    date_debut: String,
    date_fin: String,
    cid: String,
    texte: String,
}

struct Version {
    id: String,
    code_titre: String,
    num: String,
    etat: String,
    date_debut: String,
    date_fin: String,
}

struct Hit {
    id: String,
    code_titre: String,
    num: String,
    etat: String,
    date_debut: String,
    date_fin: String,
    snippet: String,
}

#[derive(Default)]
struct Node {
    children: BTreeMap<String, Node>,
    nums: Vec<String>,
}

struct CodePlan {
    root: Node,
}

pub(crate) struct DroitPack {
    conn: Mutex<Connection>,
    label: String,
    codes: Vec<String>,
    /// Segments et numéros d’un code, pas le corps des articles.
    plans: Mutex<HashMap<String, Arc<CodePlan>>>,
}

impl DroitPack {
    pub(crate) fn open(path: &Path, progress: &Reporter) -> Result<Self, String> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("Base illisible : {e}"))?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|e| format!("Base illisible : {e}"))?;
        conn.pragma_update(None, "query_only", "ON")
            .map_err(|e| format!("Lecture seule impossible : {e}"))?;
        if !schema_ok(&conn)? {
            return Err(SCHEMA_ERROR.into());
        }
        progress.force("Lecture des codes", None, String::new());
        let codes = load_codes(&conn)?;
        let label = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "droit".into());
        Ok(Self {
            conn: Mutex::new(conn),
            label,
            codes,
            plans: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn respond(&self, req: Request) {
        let (status, body) = self.render(req.url());
        let mut response =
            Response::from_data(body.into_bytes()).with_status_code(StatusCode(status));
        if let Ok(header) = Header::from_bytes(b"Content-Type", b"text/html; charset=utf-8") {
            response = response.with_header(header);
        }
        if let Ok(header) = Header::from_bytes(b"X-Content-Type-Options", b"nosniff") {
            response = response.with_header(header);
        }
        if let Ok(header) = Header::from_bytes(b"Cache-Control", b"no-store") {
            response = response.with_header(header);
        }
        let _ = req.respond(response);
    }

    /// Identifiant d’article pour une URL du lecteur, s’il y en a un.
    pub(crate) fn locate(&self, url: &str) -> Option<String> {
        let (path, query) = split_url(url);
        let parts = path_parts(path);
        if parts.len() != 1 || parts[0] != "article" {
            return None;
        }
        let q = query_map(query);
        if let Some(id) = q.get("id").map(String::as_str).filter(|id| !id.is_empty()) {
            return self.article_id(id);
        }
        let code = q.get("code").map(String::as_str).unwrap_or("").trim();
        let num = q.get("num").map(String::as_str).unwrap_or("").trim();
        if code.is_empty() || num.is_empty() {
            return None;
        }
        self.vigueur_id(code, num)
    }

    /// Citation seule, sans le corps. Le sondage d’Interroger s’en sert.
    pub(crate) fn cite(&self, id: &str) -> Result<String, String> {
        let conn = lock(&self.conn);
        let found = conn
            .query_row(
                "SELECT COALESCE(code_titre, ''), COALESCE(num, ''), COALESCE(etat, ''),
                        COALESCE(date_debut, ''), COALESCE(date_fin, '')
                 FROM documents
                 WHERE id = ?1 AND kind = 'article'",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| format!("Article illisible : {e}"))?;
        let Some((code, num, etat, debut, fin)) = found else {
            return Err(format!("Article introuvable ({id})."));
        };
        Ok(citation(&code, &num, &etat, &debut, &fin))
    }

    /// Citation juridique et corps, pour Interroger.
    pub(crate) fn passage(&self, id: &str) -> Result<(String, String), String> {
        let Some(article) = self.article_by_id(id)? else {
            return Err(format!("Article introuvable ({id})."));
        };
        let cite = citation(
            &article.code_titre,
            &article.num,
            &article.etat,
            &article.date_debut,
            &article.date_fin,
        );
        Ok((cite, article.texte))
    }

    fn render(&self, url: &str) -> (u16, String) {
        let (path, query) = split_url(url);
        let parts = path_parts(path);
        if parts.iter().any(|seg| seg == "." || seg == "..") {
            return page(403, "Interdit", "<p>Chemin refusé.</p>");
        }
        let keys: Vec<&str> = parts.iter().map(String::as_str).collect();
        match keys.as_slice() {
            [] | ["index.html"] | ["index.htm"] => self.home(),
            ["recherche"] => self.search(&query_map(query)),
            ["article"] => self.article(&query_map(query)),
            ["code", rest @ ..] => self.code_page(rest),
            _ => page(
                404,
                "Introuvable",
                "<p>Introuvable.</p><p class=\"nav\"><a href=\"/\">Accueil</a></p>",
            ),
        }
    }

    fn home(&self) -> (u16, String) {
        let mut body = format!(
            "<h1>{}</h1>\
             <p class=\"meta\">Articles en vigueur. La recherche ignore les accents.</p>",
            escape(&self.label)
        );
        body.push_str(&form_html("", "", &self.codes));
        if self.codes.is_empty() {
            body.push_str("<p>Aucun code.</p>");
        } else {
            body.push_str("<h2>Codes</h2><ul>");
            for code in &self.codes {
                body.push_str(&format!(
                    "<li><a href=\"/code/{}\">{}</a></li>",
                    encode_component(code),
                    escape(code)
                ));
            }
            body.push_str("</ul>");
        }
        page(200, &self.label, &body)
    }

    fn search(&self, q: &HashMap<String, String>) -> (u16, String) {
        let query = q.get("q").map(String::as_str).unwrap_or("").trim();
        let code = q.get("code").map(String::as_str).unwrap_or("").trim();
        let mut body = String::from(
            "<p class=\"nav\"><a href=\"/\">Accueil</a></p><h1>Recherche</h1>\
             <p class=\"meta\">Articles en vigueur seulement. Les accents sont ignorés.</p>",
        );
        body.push_str(&form_html(query, code, &self.codes));
        let Some(match_q) = match_query(query) else {
            body.push_str("<p>Indiquez au moins un mot.</p>");
            return page(200, "Recherche", &body);
        };
        if !code.is_empty() && !self.codes.iter().any(|known| known == code) {
            body.push_str("<p>Ce code n’est pas dans la base.</p>");
            return page(200, "Recherche", &body);
        }
        match self.search_rows(&match_q, code) {
            Err(err) => {
                body.push_str(&format!("<p>{}</p>", escape(&err)));
                page(200, "Recherche", &body)
            }
            Ok(rows) => {
                if rows.is_empty() {
                    body.push_str("<p>Aucun article en vigueur.</p>");
                } else {
                    if rows.len() == SEARCH_LIMIT {
                        body.push_str("<p class=\"meta\">40 résultats au plus.</p>");
                    }
                    body.push_str("<ul>");
                    for hit in rows {
                        let label = citation(
                            &hit.code_titre,
                            &hit.num,
                            &hit.etat,
                            &hit.date_debut,
                            &hit.date_fin,
                        );
                        body.push_str(&format!(
                            "<li class=\"hit\"><a href=\"/article?id={}\">{}</a><p>{}</p></li>",
                            encode_component(&hit.id),
                            escape(&label),
                            snippet_html(&hit.snippet)
                        ));
                    }
                    body.push_str("</ul>");
                }
                page(200, "Recherche", &body)
            }
        }
    }

    fn article(&self, q: &HashMap<String, String>) -> (u16, String) {
        let found = if let Some(id) = q.get("id").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            self.article_by_id(id)
        } else if let (Some(code), Some(num)) = (
            q.get("code").map(|s| s.trim()).filter(|s| !s.is_empty()),
            q.get("num").map(|s| s.trim()).filter(|s| !s.is_empty()),
        ) {
            self.article_in_force(code, num)
        } else {
            return missing_article();
        };
        let article = match found {
            Ok(Some(article)) => article,
            Ok(None) => return missing_article(),
            Err(err) => return page(500, "Erreur", &format!("<p>{}</p>", escape(&err))),
        };
        let versions = self.versions(&article.code_titre, &article.num);
        let html = match &versions {
            Ok(list) => article_html(&article, Ok(list)),
            Err(err) => article_html(&article, Err(err)),
        };
        page(200, &article_title(&article), &html)
    }

    fn code_page(&self, rest: &[&str]) -> (u16, String) {
        let Some(code) = rest.first().copied() else {
            return page(
                404,
                "Introuvable",
                "<p>Code introuvable.</p><p class=\"nav\"><a href=\"/\">Accueil</a></p>",
            );
        };
        if !self.codes.iter().any(|known| known == code) {
            return page(
                404,
                "Introuvable",
                "<p>Code introuvable.</p><p class=\"nav\"><a href=\"/\">Accueil</a></p>",
            );
        }
        let plan = match self.plan(code) {
            Ok(plan) => plan,
            Err(err) => return page(500, "Erreur", &format!("<p>{}</p>", escape(&err))),
        };
        let segs = &rest[1..];
        let Some(node) = at(&plan.root, segs) else {
            return page(
                404,
                "Introuvable",
                "<p>Cet endroit du plan n’existe pas.</p><p class=\"nav\"><a href=\"/\">Accueil</a></p>",
            );
        };
        page(200, code, &plan_html(code, segs, node))
    }

    fn plan(&self, code: &str) -> Result<Arc<CodePlan>, String> {
        if let Some(found) = lock(&self.plans).get(code).cloned() {
            return Ok(found);
        }
        let built = Arc::new(CodePlan {
            root: build_tree(self.plan_rows(code)?),
        });
        let mut plans = lock(&self.plans);
        Ok(Arc::clone(plans.entry(code.to_string()).or_insert(built)))
    }

    fn plan_rows(&self, code: &str) -> Result<Vec<(String, String)>, String> {
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT COALESCE(chemin, ''), COALESCE(num, '')
                 FROM articles_vigueur
                 WHERE code_titre = ?1
                 ORDER BY chemin, num",
            )
            .map_err(|e| format!("Plan illisible : {e}"))?;
        let mut rows = stmt
            .query([code])
            .map_err(|e| format!("Plan illisible : {e}"))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|e| format!("Plan illisible : {e}"))? {
            out.push((
                row.get(0).unwrap_or_default(),
                row.get(1).unwrap_or_default(),
            ));
        }
        Ok(out)
    }

    fn search_rows(&self, match_q: &str, code: &str) -> Result<Vec<Hit>, String> {
        let conn = lock(&self.conn);
        let sql = if code.is_empty() {
            "SELECT a.id, COALESCE(a.code_titre, ''), COALESCE(a.num, ''), COALESCE(a.etat, ''),
                    COALESCE(a.date_debut, ''), COALESCE(a.date_fin, ''),
                    snippet(documents_fts, 5, char(1), char(2), '…', 24)
             FROM documents_fts
             JOIN documents d ON d.rowid = documents_fts.rowid
             JOIN articles_vigueur a ON a.id = d.id
             WHERE documents_fts MATCH ?1
             ORDER BY rank
             LIMIT 40"
        } else {
            "SELECT a.id, COALESCE(a.code_titre, ''), COALESCE(a.num, ''), COALESCE(a.etat, ''),
                    COALESCE(a.date_debut, ''), COALESCE(a.date_fin, ''),
                    snippet(documents_fts, 5, char(1), char(2), '…', 24)
             FROM documents_fts
             JOIN documents d ON d.rowid = documents_fts.rowid
             JOIN articles_vigueur a ON a.id = d.id
             WHERE documents_fts MATCH ?1 AND a.code_titre = ?2
             ORDER BY rank
             LIMIT 40"
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| format!("Recherche impossible : {e}"))?;
        let rows = if code.is_empty() {
            stmt.query(rusqlite::params![match_q])
        } else {
            stmt.query(rusqlite::params![match_q, code])
        }
        .map_err(|e| format!("Recherche impossible : {e}"))?;
        collect_hits(rows)
    }

    fn article_in_force(&self, code: &str, num: &str) -> Result<Option<Article>, String> {
        let conn = lock(&self.conn);
        conn.query_row(
            "SELECT id, COALESCE(code_titre, ''), COALESCE(nature, ''), COALESCE(chemin, ''),
                    COALESCE(num, ''), COALESCE(etat, ''), COALESCE(date_debut, ''),
                    COALESCE(date_fin, ''), COALESCE(cid, ''), COALESCE(texte, '')
             FROM articles_vigueur
             WHERE code_titre = ?1 AND num = ?2
             ORDER BY date_debut DESC
             LIMIT 1",
            [code, num],
            article_from_row,
        )
        .optional()
        .map_err(|e| format!("Article illisible : {e}"))
    }

    fn article_by_id(&self, id: &str) -> Result<Option<Article>, String> {
        let conn = lock(&self.conn);
        conn.query_row(
            "SELECT id, COALESCE(code_titre, ''), COALESCE(nature, ''), COALESCE(chemin, ''),
                    COALESCE(num, ''), COALESCE(etat, ''), COALESCE(date_debut, ''),
                    COALESCE(date_fin, ''), COALESCE(cid, ''), COALESCE(texte, '')
             FROM documents
             WHERE id = ?1 AND kind = 'article'",
            [id],
            article_from_row,
        )
        .optional()
        .map_err(|e| format!("Article illisible : {e}"))
    }

    fn article_id(&self, id: &str) -> Option<String> {
        let conn = lock(&self.conn);
        conn.query_row(
            "SELECT id FROM documents WHERE id = ?1 AND kind = 'article'",
            [id],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    fn vigueur_id(&self, code: &str, num: &str) -> Option<String> {
        let conn = lock(&self.conn);
        conn.query_row(
            "SELECT id FROM articles_vigueur
             WHERE code_titre = ?1 AND num = ?2
             ORDER BY date_debut DESC
             LIMIT 1",
            [code, num],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    fn versions(&self, code: &str, num: &str) -> Result<Vec<Version>, String> {
        if code.is_empty() || num.is_empty() {
            return Ok(Vec::new());
        }
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT id, COALESCE(code_titre, ''), COALESCE(num, ''), COALESCE(etat, ''),
                        COALESCE(date_debut, ''), COALESCE(date_fin, '')
                 FROM documents
                 WHERE kind = 'article' AND code_titre = ?1 AND num = ?2
                 ORDER BY date_debut",
            )
            .map_err(|e| format!("Versions illisibles : {e}"))?;
        let rows = stmt
            .query_map([code, num], |row| {
                Ok(Version {
                    id: row.get(0)?,
                    code_titre: row.get(1)?,
                    num: row.get(2)?,
                    etat: row.get(3)?,
                    date_debut: row.get(4)?,
                    date_fin: row.get(5)?,
                })
            })
            .map_err(|e| format!("Versions illisibles : {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| format!("Versions illisibles : {e}"))?);
        }
        Ok(out)
    }
}

fn article_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Article> {
    Ok(Article {
        id: row.get(0)?,
        code_titre: row.get(1)?,
        nature: row.get(2)?,
        chemin: row.get(3)?,
        num: row.get(4)?,
        etat: row.get(5)?,
        date_debut: row.get(6)?,
        date_fin: row.get(7)?,
        cid: row.get(8)?,
        texte: row.get(9)?,
    })
}

fn collect_hits(mut rows: rusqlite::Rows<'_>) -> Result<Vec<Hit>, String> {
    let mut out = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("Recherche impossible : {e}"))?
    {
        out.push(Hit {
            id: row
                .get(0)
                .map_err(|e| format!("Recherche impossible : {e}"))?,
            code_titre: row
                .get(1)
                .map_err(|e| format!("Recherche impossible : {e}"))?,
            num: row
                .get(2)
                .map_err(|e| format!("Recherche impossible : {e}"))?,
            etat: row
                .get(3)
                .map_err(|e| format!("Recherche impossible : {e}"))?,
            date_debut: row
                .get(4)
                .map_err(|e| format!("Recherche impossible : {e}"))?,
            date_fin: row
                .get(5)
                .map_err(|e| format!("Recherche impossible : {e}"))?,
            snippet: row
                .get(6)
                .map_err(|e| format!("Recherche impossible : {e}"))?,
        });
        if out.len() == SEARCH_LIMIT {
            break;
        }
    }
    Ok(out)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

fn schema_ok(conn: &Connection) -> Result<bool, String> {
    Ok(object_is(conn, "documents", "table", false)?
        && object_is(conn, "articles_vigueur", "view", false)?
        && object_is(conn, "documents_fts", "table", true)?)
}

fn object_is(
    conn: &Connection,
    name: &str,
    kind: &str,
    virtual_table: bool,
) -> Result<bool, String> {
    let found: Option<(String, String)> = conn
        .query_row(
            "SELECT type, COALESCE(sql, '') FROM sqlite_master WHERE name = ?1",
            [name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| format!("Schéma illisible : {e}"))?;
    let Some((ty, sql)) = found else {
        return Ok(false);
    };
    if !ty.eq_ignore_ascii_case(kind) {
        return Ok(false);
    }
    let is_virtual = sql.to_ascii_lowercase().contains("virtual table");
    Ok(is_virtual == virtual_table)
}

fn load_codes(conn: &Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT code_titre FROM documents
             WHERE code_titre <> ''
             ORDER BY code_titre",
        )
        .map_err(|e| format!("Lecture des codes impossible : {e}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("Lecture des codes impossible : {e}"))?;
    let mut codes = Vec::new();
    for code in rows {
        let code = code.map_err(|e| format!("Lecture des codes impossible : {e}"))?;
        let code = code.trim();
        if !code.is_empty() {
            codes.push(code.to_string());
        }
    }
    Ok(codes)
}

fn build_tree(rows: Vec<(String, String)>) -> Node {
    let mut root = Node::default();
    for (chemin, num) in rows {
        let segs = split_chemin(&chemin);
        insert(&mut root, &segs, &num);
    }
    root
}

fn insert(node: &mut Node, segs: &[String], num: &str) {
    if segs.is_empty() {
        if !num.is_empty() && !node.nums.iter().any(|have| have == num) {
            node.nums.push(num.to_string());
        }
        return;
    }
    let child = node.children.entry(segs[0].clone()).or_default();
    insert(child, &segs[1..], num);
}

fn at<'a>(node: &'a Node, segs: &[&str]) -> Option<&'a Node> {
    let mut cur = node;
    for seg in segs {
        cur = cur.children.get(*seg)?;
    }
    Some(cur)
}

fn split_chemin(chemin: &str) -> Vec<String> {
    chemin
        .split('/')
        .map(str::trim)
        .filter(|seg| !seg.is_empty())
        .map(str::to_string)
        .collect()
}

/// `2999-01-01` n’est pas une date du calendrier.
fn shown_date(date: &str) -> Option<&str> {
    let date = date.trim();
    if date.is_empty() || date == SENTINEL_END {
        None
    } else {
        Some(date)
    }
}

pub(crate) fn citation(code: &str, num: &str, etat: &str, debut: &str, fin: &str) -> String {
    let code = code.trim();
    let num = num.trim();
    let etat = etat.trim();
    let debut = shown_date(debut);
    let fin = shown_date(fin);
    let mut out = String::new();
    if !code.is_empty() {
        out.push_str(code);
    }
    if !num.is_empty() {
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str("art. ");
        out.push_str(num);
    }
    if etat == "VIGUEUR" && fin.is_none() {
        out.push_str(", en vigueur");
        if let Some(debut) = debut {
            out.push_str(" depuis le ");
            out.push_str(debut);
        }
        return out;
    }
    if etat == "VIGUEUR_DIFF" && fin.is_none() {
        out.push_str(", VIGUEUR_DIFF");
        if let Some(debut) = debut {
            out.push_str(", depuis le ");
            out.push_str(debut);
        }
        return out;
    }
    if !etat.is_empty() {
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(etat);
    }
    match (debut, fin) {
        (Some(debut), Some(fin)) => {
            out.push_str(", du ");
            out.push_str(debut);
            out.push_str(" au ");
            out.push_str(fin);
        }
        (Some(debut), None) => {
            out.push_str(", depuis le ");
            out.push_str(debut);
        }
        (None, Some(fin)) => {
            out.push_str(", jusqu’au ");
            out.push_str(fin);
        }
        (None, None) => {}
    }
    out
}

fn article_title(article: &Article) -> String {
    if article.num.is_empty() {
        article.code_titre.clone()
    } else if article.code_titre.is_empty() {
        format!("art. {}", article.num)
    } else {
        format!("{}, art. {}", article.code_titre, article.num)
    }
}

fn article_html(article: &Article, versions: Result<&[Version], &str>) -> String {
    let cite = citation(
        &article.code_titre,
        &article.num,
        &article.etat,
        &article.date_debut,
        &article.date_fin,
    );
    let mut body = String::from("<p class=\"nav\"><a href=\"/\">Accueil</a>");
    if !article.code_titre.is_empty() {
        body.push_str(&format!(
            " · <a href=\"/code/{}\">{}</a>",
            encode_component(&article.code_titre),
            escape(&article.code_titre)
        ));
    }
    body.push_str("</p>");
    body.push_str(&format!("<h1>{}</h1>", escape(&article_title(article))));
    body.push_str(&format!("<p class=\"meta\">{}</p>", escape(&cite)));
    if !article.etat.is_empty() {
        body.push_str(&format!(
            "<p class=\"meta\">État : {}</p>",
            escape(&article.etat)
        ));
    }
    if !article.nature.is_empty() {
        body.push_str(&format!(
            "<p class=\"meta\">Nature : {}</p>",
            escape(&article.nature)
        ));
    }
    if !article.chemin.is_empty() {
        body.push_str("<p class=\"meta\">");
        body.push_str(&chemin_links(&article.code_titre, &article.chemin));
        body.push_str("</p>");
    }
    if !article.cid.is_empty() {
        body.push_str(&format!(
            "<p class=\"meta\">CID : {}</p>",
            escape(&article.cid)
        ));
    }
    body.push_str(&format!(
        "<article class=\"texte\">{}</article>",
        escape(&article.texte)
    ));
    match versions {
        Err(err) => {
            body.push_str(&format!("<p>{}</p>", escape(err)));
        }
        Ok(versions) => {
            let others: Vec<&Version> = versions.iter().filter(|v| v.id != article.id).collect();
            if !others.is_empty() {
                body.push_str("<h2>Autres versions</h2><ul>");
                for version in others {
                    let label = citation(
                        &version.code_titre,
                        &version.num,
                        &version.etat,
                        &version.date_debut,
                        &version.date_fin,
                    );
                    body.push_str(&format!(
                        "<li><a href=\"/article?id={}\">{}</a></li>",
                        encode_component(&version.id),
                        escape(&label)
                    ));
                }
                body.push_str("</ul>");
            }
        }
    }
    body
}

fn plan_html(code: &str, segs: &[&str], node: &Node) -> String {
    let mut body = String::from("<p class=\"nav\"><a href=\"/\">Accueil</a>");
    body.push_str(&format!(
        " · <a href=\"/code/{}\">{}</a>",
        encode_component(code),
        escape(code)
    ));
    let mut acc = Vec::new();
    for seg in segs {
        acc.push((*seg).to_string());
        body.push_str(&format!(
            " · <a href=\"{}\">{}</a>",
            code_href(code, &acc),
            escape(seg)
        ));
    }
    body.push_str("</p>");
    let title = segs.last().copied().unwrap_or(code);
    body.push_str(&format!("<h1>{}</h1>", escape(title)));
    if node.children.is_empty() && node.nums.is_empty() {
        body.push_str("<p>Aucun article en vigueur à cet endroit.</p>");
        return body;
    }
    if !node.children.is_empty() {
        body.push_str("<h2>Plan</h2><ul>");
        for (name, _) in &node.children {
            let mut child = acc.clone();
            child.push(name.clone());
            body.push_str(&format!(
                "<li><a href=\"{}\">{}</a></li>",
                code_href(code, &child),
                escape(name)
            ));
        }
        body.push_str("</ul>");
    }
    if !node.nums.is_empty() {
        body.push_str("<h2>Articles</h2><ul>");
        for num in &node.nums {
            body.push_str(&format!(
                "<li><a href=\"{}\">art. {}</a></li>",
                article_href(code, num),
                escape(num)
            ));
        }
        body.push_str("</ul>");
    }
    body
}

fn chemin_links(code: &str, chemin: &str) -> String {
    let segs = split_chemin(chemin);
    if code.is_empty() || segs.is_empty() {
        return escape(chemin);
    }
    let mut out = String::new();
    let mut acc = Vec::new();
    for (i, seg) in segs.iter().enumerate() {
        if i > 0 {
            out.push_str(" / ");
        }
        acc.push(seg.clone());
        out.push_str(&format!(
            "<a href=\"{}\">{}</a>",
            code_href(code, &acc),
            escape(seg)
        ));
    }
    out
}

fn form_html(query: &str, code: &str, codes: &[String]) -> String {
    let mut out = format!(
        "<form action=\"/recherche\" method=\"get\">\
         <label for=\"q\">Rechercher</label>\
         <input id=\"q\" type=\"search\" name=\"q\" value=\"{}\">\
         <label for=\"code\">Code</label>\
         <select id=\"code\" name=\"code\"><option value=\"\">Tous les codes</option>",
        escape(query)
    );
    for known in codes {
        let selected = if known == code { " selected" } else { "" };
        out.push_str(&format!(
            "<option value=\"{}\"{}>{}</option>",
            escape(known),
            selected,
            escape(known)
        ));
    }
    out.push_str("</select><button type=\"submit\">Rechercher</button></form>");
    out
}

fn article_href(code: &str, num: &str) -> String {
    format!(
        "/article?code={}&num={}",
        encode_component(code),
        encode_component(num)
    )
}

fn code_href(code: &str, segs: &[String]) -> String {
    let mut out = format!("/code/{}", encode_component(code));
    for seg in segs {
        out.push('/');
        out.push_str(&encode_component(seg));
    }
    out
}

fn missing_article() -> (u16, String) {
    page(
        404,
        "Introuvable",
        "<p>Article introuvable.</p><p class=\"nav\"><a href=\"/\">Accueil</a></p>",
    )
}

fn page(status: u16, title: &str, body: &str) -> (u16, String) {
    let html = format!(
        "<!DOCTYPE html><html lang=\"fr\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src 'self'; form-action 'self'; base-uri 'none'\">\
         <title>{}</title><style>{}</style></head><body>{}</body></html>",
        escape(title),
        STYLE,
        body
    );
    (status, html)
}

fn match_query(raw: &str) -> Option<String> {
    let mut parts = Vec::new();
    for word in raw.split_whitespace() {
        let mut token = String::new();
        for c in word.chars() {
            if c == '"' {
                token.push('"');
            }
            token.push(c);
        }
        if token.is_empty() {
            continue;
        }
        parts.push(format!("\"{token}\""));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

fn snippet_html(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        match c {
            '\u{1}' => out.push_str("<mark>"),
            '\u{2}' => out.push_str("</mark>"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn split_url(url: &str) -> (&str, &str) {
    let url = url.split('#').next().unwrap_or(url);
    url.split_once('?').unwrap_or((url, ""))
}

fn path_parts(path: &str) -> Vec<String> {
    path.split('/')
        .map(crate::md::percent_decode)
        .filter(|seg| !seg.is_empty())
        .collect()
}

fn query_map(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = crate::md::percent_decode(&key.replace('+', " "));
        let value = crate::md::percent_decode(&value.replace('+', " "));
        if !key.is_empty() {
            out.insert(key, value);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use crate::ask::{self, LivePack};
    use crate::pack::PackServer;

    const SCHEMA: &str = "
        CREATE TABLE documents (
          id TEXT PRIMARY KEY,
          base TEXT NOT NULL,
          kind TEXT NOT NULL,
          cid TEXT,
          nature TEXT,
          titre TEXT,
          num TEXT,
          etat TEXT,
          date_debut TEXT,
          date_fin TEXT,
          date_texte TEXT,
          code_titre TEXT,
          texte_titre TEXT,
          chemin TEXT,
          fonds TEXT,
          juridiction TEXT,
          formation TEXT,
          solution TEXT,
          ecli TEXT,
          nor TEXT,
          texte TEXT,
          nota TEXT,
          source_archive TEXT NOT NULL
        );
        CREATE VIEW articles_vigueur AS
        SELECT id, base, code_titre, texte_titre, nature, chemin, num, etat,
               date_debut, date_fin, date_texte, cid, texte
        FROM documents
        WHERE kind = 'article' AND etat IN ('VIGUEUR', 'VIGUEUR_DIFF');
        CREATE INDEX idx_documents_code ON documents(code_titre, chemin, num);
        CREATE INDEX idx_documents_ecli ON documents(ecli);
        CREATE VIRTUAL TABLE documents_fts USING fts5(
          code_titre, texte_titre, chemin, num, titre, texte, nota,
          content='documents', content_rowid='rowid',
          tokenize='unicode61 remove_diacritics 1'
        );
    ";

    fn corpus(path: &Path, texte_1240: &str) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let insert = "INSERT INTO documents (
            id, base, kind, cid, nature, titre, num, etat, date_debut, date_fin,
            code_titre, texte_titre, chemin, texte, source_archive
        ) VALUES (?1, 'LEGIFRANCE', ?2, ?3, 'CODE', ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10, ?11, 'test')";
        conn.execute(
            insert,
            rusqlite::params![
                "art-1240",
                "article",
                "LEGITEXT000006070721",
                "Article 1240",
                "1240",
                "VIGUEUR",
                "2016-10-01",
                "2999-01-01",
                "Code civil",
                "Livre III / Titre III",
                texte_1240,
            ],
        )
        .unwrap();
        conn.execute(
            insert,
            rusqlite::params![
                "art-1240-old",
                "article",
                "LEGITEXT000006070721",
                "Article 1240 ancien",
                "1240",
                "ABROGE",
                "1804-03-21",
                "2016-10-01",
                "Code civil",
                "Livre III / Titre III",
                "Ancienne responsabilité abrogée, marque-abrogee.",
            ],
        )
        .unwrap();
        conn.execute(
            insert,
            rusqlite::params![
                "art-1241",
                "article",
                "LEGITEXT000006070721",
                "Article 1241",
                "1241",
                "VIGUEUR",
                "2016-10-01",
                "2999-01-01",
                "Code civil",
                "Livre Ier / Titre Ier",
                "Le second article reste en vigueur.",
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents_fts(documents_fts) VALUES('rebuild')",
            [],
        )
        .unwrap();
    }

    fn sample() -> (tempfile::TempDir, std::path::PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("droit.sqlite");
        let mut texte = String::from("Tout fait qui cause un dommage engage la responsabilité.\n");
        texte.extend(std::iter::repeat('é').take(ask::DOC_CHARS));
        corpus(&path, &texte);
        (dir, path, texte)
    }

    #[test]
    fn sentinel_end_is_not_a_calendar_date() {
        let cite = citation("Code civil", "1240", "VIGUEUR", "2016-10-01", "2999-01-01");
        assert_eq!(
            cite,
            "Code civil, art. 1240, en vigueur depuis le 2016-10-01"
        );
        assert!(!cite.contains("2999"));
        let old = citation("Code civil", "1240", "ABROGE", "1804-03-21", "2016-10-01");
        assert_eq!(
            old,
            "Code civil, art. 1240, ABROGE, du 1804-03-21 au 2016-10-01"
        );
    }

    #[test]
    fn sqlite_without_the_corpus_objects_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE notes (id INTEGER)", []).unwrap();
        drop(conn);
        let err = match PackServer::open(&path) {
            Ok(_) => panic!("un sqlite sans le schéma a été accepté"),
            Err(err) => err,
        };
        assert!(err.contains("corpus de droit"), "{err}");
    }

    #[test]
    fn article_page_shows_the_text_in_force_and_the_other_version() {
        let (_dir, path, _) = sample();
        let server = PackServer::open(&path).unwrap();
        assert!(server.droit.is_some());
        assert!(!server.markdown);
        let (status, body) = http_get(server.port, "/article?code=Code%20civil&num=1240");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("engage la responsabilité"), "{body}");
        assert!(body.contains("en vigueur depuis le 2016-10-01"), "{body}");
        assert!(body.contains("État : VIGUEUR"), "{body}");
        assert!(body.contains("/article?id=art-1240-old"), "{body}");
        assert!(body.contains("ABROGE"), "{body}");
        assert!(body.contains("1804-03-21"), "{body}");
        assert!(!body.contains("2999-01-01"), "{body}");
        assert!(!body.contains("marque-abrogee"), "{body}");

        let (status, body) = http_get(server.port, "/article?id=art-1240-old");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("marque-abrogee"), "{body}");
        assert!(body.contains("État : ABROGE"), "{body}");
        assert!(body.contains("1804-03-21"), "{body}");
        assert!(body.contains("2016-10-01"), "{body}");
        assert!(!body.contains("2999-01-01"), "{body}");

        let (status, home) = http_get(server.port, "/");
        assert_eq!(status, 200, "{home}");
        assert!(home.contains("href=\"/code/Code%20civil\""), "{home}");

        let (status, plan) = http_get(server.port, "/code/Code%20civil");
        assert_eq!(status, 200, "{plan}");
        assert!(plan.contains("Livre III"), "{plan}");
        assert!(plan.contains("Livre Ier"), "{plan}");
        assert!(!plan.contains("dommage"), "{plan}");

        let (status, title) = http_get(server.port, "/code/Code%20civil/Livre%20III/Titre%20III");
        assert_eq!(status, 200, "{title}");
        assert!(title.contains("art. 1240"), "{title}");
        assert!(title.contains("num=1240"), "{title}");
        assert!(!title.contains("dommage"), "{title}");
        server.stop();
    }

    #[test]
    fn unaccented_search_finds_the_accented_text_and_skips_the_repealed_one() {
        let (_dir, path, _) = sample();
        let server = PackServer::open(&path).unwrap();
        let (status, body) = http_get(server.port, "/recherche?q=responsabilite");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("responsabilité"), "{body}");
        assert!(body.contains("/article?id=art-1240"), "{body}");
        assert!(!body.contains("marque-abrogee"), "{body}");
        assert!(!body.contains("art-1241"), "{body}");
        assert!(!body.contains("2999-01-01"), "{body}");

        let (status, empty) = http_get(server.port, "/recherche?q=");
        assert_eq!(status, 200, "{empty}");
        assert!(empty.contains("Indiquez au moins un mot"), "{empty}");
        assert!(!empty.contains("responsabilité"), "{empty}");
        server.stop();
    }

    #[test]
    fn ollama_question_receives_the_legal_citation_and_truncates() {
        let (_dir, path, _) = sample();
        let droit = Arc::new(DroitPack::open(&path, &Reporter::silent()).unwrap());
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = match server.server_addr() {
            tiny_http::ListenAddr::IP(addr) => addr.port(),
            other => panic!("adresse inattendue: {other:?}"),
        };
        let saw = Arc::new(AtomicBool::new(false));
        let saw_t = Arc::clone(&saw);
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
            assert_eq!(
                value["messages"][0]["content"].as_str().unwrap(),
                SYSTEM_PROMPT
            );
            let user = value["messages"][1]["content"].as_str().unwrap();
            assert!(
                user.contains("## Code civil, art. 1240, en vigueur depuis le 2016-10-01"),
                "{user}"
            );
            assert!(!user.contains("## fichier:"), "{user}");
            assert!(user.contains("engage la responsabilité"), "{user}");
            assert!(
                user.contains("[Document tronqué à 12 000 caractères.]"),
                "{user}"
            );
            let text = user
                .split_once("## Code civil, art. 1240, en vigueur depuis le 2016-10-01\n\n")
                .unwrap()
                .1
                .split_once("\n[Document tronqué")
                .unwrap()
                .0;
            assert_eq!(text.chars().count(), ask::DOC_CHARS);
            saw_t.store(true, Ordering::SeqCst);
            req.respond(tiny_http::Response::from_string(
                r#"{"message":{"role":"assistant","content":"Réponse fixe."},"done":true}"#,
            ))
            .unwrap();
        });

        let mut pack = LivePack::new(path.clone(), false, 9, Arc::new(std::sync::OnceLock::new()));
        pack.attach_droit(Arc::clone(&droit));
        pack.note(
            &"http://127.0.0.1:9/article?code=Code%20civil&num=1240"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            pack.view().current.as_deref(),
            Some("Code civil, art. 1240, en vigueur depuis le 2016-10-01")
        );
        let reply = pack
            .question("", &format!("http://127.0.0.1:{port}"), Some("modele-test"))
            .unwrap();
        handle.join().unwrap();
        assert!(saw.load(Ordering::SeqCst));
        assert_eq!(reply.answer, "Réponse fixe.");
        assert_eq!(
            reply.files,
            vec!["Code civil, art. 1240, en vigueur depuis le 2016-10-01".to_string()]
        );
    }

    #[test]
    fn search_links_the_in_force_row_that_matched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deux.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let insert = "INSERT INTO documents (
            id, base, kind, cid, nature, titre, num, etat, date_debut, date_fin,
            code_titre, texte_titre, chemin, texte, source_archive
        ) VALUES (?1, 'LEGIFRANCE', 'article', 'LEGITEXT', 'CODE', ?2, 'L643-8',
                   'VIGUEUR_DIFF', ?3, ?4, 'Code de commerce', 'Code de commerce', '', ?5, 'test')";
        conn.execute(
            insert,
            rusqlite::params![
                "art-ancien",
                "Article L643-8 ancien",
                "2027-01-01",
                "2029-01-01",
                "formulation ancienne unique",
            ],
        )
        .unwrap();
        conn.execute(
            insert,
            rusqlite::params![
                "art-recent",
                "Article L643-8 recent",
                "2029-01-01",
                "2999-01-01",
                "formulation recente unique",
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents_fts(documents_fts) VALUES('rebuild')",
            [],
        )
        .unwrap();
        drop(conn);

        let server = PackServer::open(&path).unwrap();
        let (status, body) = http_get(server.port, "/recherche?q=ancienne");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("/article?id=art-ancien"), "{body}");
        assert!(body.contains("ancienne"), "{body}");
        assert!(!body.contains("/article?id=art-recent"), "{body}");
        assert!(!body.contains("2999-01-01"), "{body}");

        let (status, old) = http_get(server.port, "/article?id=art-ancien");
        assert_eq!(status, 200, "{old}");
        assert!(old.contains("formulation ancienne unique"), "{old}");
        assert!(old.contains("/article?id=art-recent"), "{old}");

        let (status, latest) =
            http_get(server.port, "/article?code=Code%20de%20commerce&num=L643-8");
        assert_eq!(status, 200, "{latest}");
        assert!(latest.contains("formulation recente unique"), "{latest}");
        assert!(!latest.contains("formulation ancienne unique"), "{latest}");
        assert!(!latest.contains("2999-01-01"), "{latest}");
        server.stop();
    }

    #[test]
    fn a_markdown_folder_still_opens_and_a_sqlite_inside_it_does_not() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.md"), b"# Note\n\nToujours la").unwrap();
        corpus(&dir.path().join("droit.sqlite"), "texte");
        let server = PackServer::open(dir.path()).unwrap();
        assert!(server.markdown);
        assert!(server.droit.is_none());
        let (status, body) = http_get(server.port, "/");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("note.md"), "{body}");
        assert!(!body.contains("Articles en vigueur"), "{body}");
        server.stop();

        let only = tempfile::tempdir().unwrap();
        corpus(&only.path().join("droit.sqlite"), "texte");
        let err = match PackServer::open(only.path()) {
            Ok(_) => panic!("un dossier qui contient seulement un sqlite a été ouvert"),
            Err(err) => err,
        };
        assert!(err.contains("index.html"), "{err}");
    }

    /// Ouverture manuelle du corpus réel. `cargo test` ne le lance pas :
    /// il ne s’exécute qu’avec `--ignored` et `TAURUS_DROIT_SQLITE`.
    #[test]
    #[ignore]
    fn real_corpus_serves_article_1240_a_plan_and_a_search() {
        let Ok(path) = std::env::var("TAURUS_DROIT_SQLITE") else {
            eprintln!("TAURUS_DROIT_SQLITE absent, corpus réel non ouvert.");
            return;
        };
        let path = std::path::PathBuf::from(path);
        let started = std::time::Instant::now();
        let server = PackServer::open(&path).unwrap();
        let open_ms = started.elapsed().as_millis();
        assert!(server.droit.is_some());

        let timed = |path: &str| {
            let t = std::time::Instant::now();
            let got = http_get_within(server.port, path, Duration::from_secs(120));
            (got.0, got.1, t.elapsed().as_millis())
        };

        let (status, article, article_ms) = timed("/article?code=Code%20civil&num=1240");
        assert_eq!(status, 200, "{article}");
        assert!(
            article.contains("Tout fait quelconque de l'homme"),
            "{article}"
        );
        assert!(
            article.contains("en vigueur depuis le 2016-10-01"),
            "{article}"
        );
        assert!(
            article.contains("/article?id=LEGIARTI000006437044"),
            "{article}"
        );
        assert!(!article.contains("2999-01-01"), "{article}");

        let (status, plan, plan_ms) = timed("/code/Code%20civil");
        assert_eq!(status, 200, "{plan}");
        assert!(plan.contains("Livre III"), "{plan}");
        assert!(!plan.contains("Tout fait quelconque"), "{plan}");

        let full = article
            .match_indices("href=\"")
            .filter_map(|(at, _)| {
                let rest = article.get(at + 6..)?;
                let end = rest.find('"')?;
                let href = &rest[..end];
                href.starts_with("/code/").then_some(href)
            })
            .max_by_key(|href| href.len())
            .expect("lien de plan")
            .to_string();
        let (status, title, title_ms) = timed(&full);
        assert_eq!(status, 200, "{title}");
        assert!(title.contains("art. 1240"), "{title}");
        assert!(!title.contains("Tout fait quelconque"), "{title}");

        let (status, found, search_ms) = timed("/recherche?q=fait%20quelconque&code=Code%20civil");
        assert_eq!(status, 200, "{found}");
        assert!(
            found.contains("/article?id=LEGIARTI000032041571"),
            "{found}"
        );
        assert!(!found.contains("2999-01-01"), "{found}");

        eprintln!(
            "corpus réel : ouverture {open_ms} ms, article {article_ms} ms, plan {plan_ms} ms, titre {title_ms} ms, recherche {search_ms} ms"
        );
        server.stop();
    }

    fn http_get(port: u16, path: &str) -> (u16, String) {
        http_get_within(port, path, Duration::from_secs(2))
    }

    fn http_get_within(port: u16, path: &str, timeout: Duration) -> (u16, String) {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(timeout)).unwrap();
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
