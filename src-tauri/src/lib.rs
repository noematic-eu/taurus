mod ask;
mod md;
mod pack;
mod progress;
mod wacz;
mod zip_pack;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use pack::PackServer;
use tauri::Manager;
use tauri::{DragDropEvent, Emitter, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::DialogExt;

static PACK_ID: AtomicU64 = AtomicU64::new(1);
static OPEN_GEN: AtomicU64 = AtomicU64::new(1);

struct Shutdown {
    once: AtomicBool,
    server: Arc<PackServer>,
    app: tauri::AppHandle,
    id: u64,
    pack_label: String,
    ask_label: String,
}

impl Shutdown {
    fn close(&self) {
        if self.once.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(store) = self.app.try_state::<ask::AskStore>() {
            store.remove(self.id);
        }
        self.server.stop();
        if let Some(window) = self.app.get_webview_window(&self.pack_label) {
            let _ = window.close();
        }
        if let Some(window) = self.app.get_webview_window(&self.ask_label) {
            let _ = window.close();
        }
        let reply = format!("ask-reply-{}", self.id);
        if let Some(window) = self.app.get_webview_window(&reply) {
            let _ = window.close();
        }
    }
}

/// Prépare le pack hors du thread principal et publie la progression sur l’accueil.
fn begin_open(app: &tauri::AppHandle, pack_path: PathBuf) {
    let app = app.clone();
    let gen = OPEN_GEN.fetch_add(1, Ordering::SeqCst);
    let worker = app.clone();
    let spawned = std::thread::Builder::new()
        .name("taurus-open".into())
        .spawn(move || {
            let emit = worker.clone();
            let progress = progress::Reporter::new(gen, move |event| {
                let _ = emit.emit("taurus-progress", event);
            });
            progress.force("Ouverture…", None, String::new());
            let opened = PackServer::open_with(&pack_path, &progress);
            let present = worker.clone();
            if worker
                .run_on_main_thread(move || {
                    match opened {
                        Ok(server) => {
                            if let Err(err) = present_pack(&present, pack_path, Arc::new(server)) {
                                let _ = present.emit("taurus-error", err);
                            }
                        }
                        Err(err) => {
                            let _ = present.emit("taurus-error", err);
                        }
                    }
                    let _ = present.emit("taurus-progress", progress::OpenProgress::finished(gen));
                })
                .is_err()
            {
                let _ = worker.emit("taurus-error", "Ouverture interrompue.");
                let _ = worker.emit("taurus-progress", progress::OpenProgress::finished(gen));
            }
        });
    if spawned.is_err() {
        let _ = app.emit("taurus-error", "Ouverture interrompue.");
    }
}

fn present_pack(
    app: &tauri::AppHandle,
    pack_path: PathBuf,
    server: Arc<PackServer>,
) -> Result<String, String> {
    let url = server.url();
    let title = format!("Taurus — {}", server.title);
    let id = PACK_ID.fetch_add(1, Ordering::SeqCst);
    let pack_label = format!("pack-{id}");
    let ask_label = format!("ask-{id}");
    let parsed: tauri::Url = url.parse().map_err(|e| {
        server.stop();
        format!("url: {e}")
    })?;
    let mut live = ask::LivePack::new(
        server.root.clone(),
        server.markdown,
        server.port,
        server.index.clone(),
    );
    live.point_save_at(&pack_path);
    if let Some(zip) = &server.zip {
        live.attach_zip(Arc::clone(zip));
    }
    let live = Arc::new(live);
    app.state::<ask::AskStore>().insert(id, live.clone());

    let nav = live.clone();
    let pack = WebviewWindowBuilder::new(app, &pack_label, WebviewUrl::External(parsed.clone()))
        .title(&title)
        .inner_size(1100.0, 800.0)
        .on_navigation(move |url| {
            nav.note(url);
            true
        })
        .build()
        .map_err(|e| {
            app.state::<ask::AskStore>().remove(id);
            server.stop();
            format!("fenêtre: {e}")
        })?;

    let ask = match WebviewWindowBuilder::new(app, &ask_label, WebviewUrl::App("ask.html".into()))
        .title(format!("Interroger — {}", server.title))
        .inner_size(440.0, 760.0)
        .focused(false)
        .build()
    {
        Ok(window) => window,
        Err(e) => {
            let _ = pack.close();
            app.state::<ask::AskStore>().remove(id);
            server.stop();
            return Err(format!("fenêtre: {e}"));
        }
    };

    if let (Ok(pos), Ok(size)) = (pack.outer_position(), pack.outer_size()) {
        let _ = ask.set_position(tauri::PhysicalPosition::new(
            pos.x + size.width as i32 + 12,
            pos.y,
        ));
    }
    live.note(&parsed);

    let shutdown = Arc::new(Shutdown {
        once: AtomicBool::new(false),
        server,
        app: app.clone(),
        id,
        pack_label,
        ask_label,
    });
    let on_pack = shutdown.clone();
    pack.on_window_event(move |event| {
        if matches!(
            event,
            WindowEvent::Destroyed | WindowEvent::CloseRequested { .. }
        ) {
            on_pack.close();
        }
    });
    ask.on_window_event(move |event| {
        if matches!(
            event,
            WindowEvent::Destroyed | WindowEvent::CloseRequested { .. }
        ) {
            shutdown.close();
        }
    });

    Ok(title)
}

#[tauri::command]
fn pick_pack(app: tauri::AppHandle) {
    app.dialog()
        .file()
        .add_filter("Packs (zip, wacz)", &["zip", "wacz", "warc", "gz"])
        .pick_file(move |file| {
            let Some(path) = file else {
                return;
            };
            let path = PathBuf::from(path.to_string());
            begin_open(&app, path);
        });
}

#[tauri::command]
fn pick_folder(app: tauri::AppHandle) {
    app.dialog()
        .file()
        .set_title("Ouvrir un dossier de site")
        .pick_folder(move |folder| {
            let Some(path) = folder else {
                return;
            };
            begin_open(&app, PathBuf::from(path.to_string()));
        });
}

#[tauri::command]
fn open_pack_path(app: tauri::AppHandle, path: String) {
    begin_open(&app, PathBuf::from(path));
}

#[tauri::command]
fn ask_state(app: tauri::AppHandle, id: u64) -> Result<ask::AskView, String> {
    Ok(app.state::<ask::AskStore>().get(id)?.view())
}

#[tauri::command]
fn ask_add(app: tauri::AppHandle, id: u64) -> Result<ask::AskView, String> {
    app.state::<ask::AskStore>().get(id)?.add_current()
}

#[tauri::command]
fn ask_remove(app: tauri::AppHandle, id: u64, rel: String) -> Result<ask::AskView, String> {
    Ok(app.state::<ask::AskStore>().get(id)?.remove(&rel))
}

#[tauri::command]
async fn ask_question(
    app: tauri::AppHandle,
    id: u64,
    question: String,
) -> Result<ask::AskReply, String> {
    let pack = app.state::<ask::AskStore>().get(id)?;
    let host = ask::ollama_host();
    let model = ask::ollama_model();
    let reply = tauri::async_runtime::spawn_blocking(move || {
        pack.question(&question, &host, model.as_deref())
    })
    .await
    .map_err(|_| "Interrogation interrompue.".to_string())??;
    refresh_reply(&app, id);
    Ok(reply)
}

fn refresh_reply(app: &tauri::AppHandle, id: u64) {
    let label = format!("ask-reply-{id}");
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(window) = handle.get_webview_window(&label) {
            let _ = window.eval("location.reload()");
        }
    });
}

#[tauri::command]
async fn ask_models(app: tauri::AppHandle, id: u64) -> Result<ask::ModelView, String> {
    let pack = app.state::<ask::AskStore>().get(id)?;
    let host = ask::ollama_host();
    let preferred = ask::ollama_model();
    tauri::async_runtime::spawn_blocking(move || pack.models(&host, preferred.as_deref()))
        .await
        .map_err(|_| "Interrogation interrompue.".to_string())?
}

#[tauri::command]
async fn ask_save(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
    id: u64,
) -> Result<Option<String>, String> {
    let pack = app.state::<ask::AskStore>().get(id)?;
    let markdown = if window.label() == format!("ask-reply-{id}") {
        pack.shown_markdown()?
    } else {
        pack.answer_text()?
    };
    let directory = pack.save_directory().to_path_buf();
    let picked = pick_save_path(&app, &directory, "reponse.md")?;
    let Some(file) = picked else {
        return Ok(None);
    };
    let path = ask::markdown_save_path(file);
    if pack.save_lands_in_temp(&path) {
        return Err("Ce dossier est temporaire : il disparaît à la fermeture du pack.".to_string());
    }
    let mut text = markdown;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    std::fs::write(&path, text).map_err(|err| format!("Enregistrement impossible : {err}"))?;
    Ok(Some(path.display().to_string()))
}

#[tauri::command]
async fn ask_open(app: tauri::AppHandle, id: u64) -> Result<(), String> {
    let pack = app.state::<ask::AskStore>().get(id)?;
    pack.answer_text()?;
    let title = pack
        .root
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("Réponse — {name}"))
        .unwrap_or_else(|| "Réponse".to_string());
    let label = format!("ask-reply-{id}");
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let _ = tx.send(open_reply_window(&handle, &label, title));
    })
    .map_err(|err| format!("fenêtre: {err}"))?;
    rx.recv()
        .map_err(|_| "Ouverture interrompue.".to_string())?
}

fn open_reply_window(app: &tauri::AppHandle, label: &str, title: String) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(label) {
        let _ = window.eval("location.reload()");
        let _ = window.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(app, label, WebviewUrl::App("reply.html".into()))
        .title(title)
        .inner_size(860.0, 920.0)
        .build()
        .map_err(|err| format!("fenêtre: {err}"))?;
    if let Some(window) = app.get_webview_window(label) {
        let _ = window.set_focus();
    }
    Ok(())
}

#[tauri::command]
fn ask_rendered(app: tauri::AppHandle, id: u64) -> Result<ask::AnswerPage, String> {
    app.state::<ask::AskStore>().get(id)?.answer_page()
}

#[tauri::command]
async fn ask_set_model(
    app: tauri::AppHandle,
    id: u64,
    model: String,
) -> Result<ask::ModelView, String> {
    let pack = app.state::<ask::AskStore>().get(id)?;
    let host = ask::ollama_host();
    tauri::async_runtime::spawn_blocking(move || pack.set_model(&host, &model))
        .await
        .map_err(|_| "Interrogation interrompue.".to_string())?
}

fn drop_packs(app: &tauri::AppHandle, paths: &[PathBuf]) {
    for p in paths {
        if is_pack(p) {
            begin_open(app, p.clone());
        } else {
            let _ = app.emit(
                "taurus-error",
                format!(
                    "{} n’est pas un dossier de site, un .zip, un .wacz ni un .warc",
                    p.display()
                ),
            );
        }
    }
}

fn is_pack(p: &Path) -> bool {
    pack::is_openable(p)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            pick_pack,
            pick_folder,
            open_pack_path,
            ask_state,
            ask_add,
            ask_remove,
            ask_question,
            ask_models,
            ask_set_model,
            ask_save,
            ask_open,
            ask_rendered
        ])
        .setup(|app| {
            app.manage(ask::AskStore::default());
            let handle = app.handle().clone();
            if let Some(main) = app.get_webview_window("main") {
                main.on_window_event(move |event| {
                    if let WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }) = event {
                        drop_packs(&handle, paths);
                    }
                });
            }

            let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
            let handle = app.handle().clone();
            for p in args {
                if is_pack(&p) {
                    begin_open(&handle, p);
                }
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("erreur Taurus")
        .run(
            #[allow(unused_variables)]
            |handle, event| {
                // Double-clic / drop sur l’icône : macOS seulement (RunEvent::Opened).
                // Linux/Windows : le fichier arrive en argument CLI (setup ci-dessus).
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                if let tauri::RunEvent::Opened { urls } = event {
                    let paths: Vec<PathBuf> =
                        urls.iter().filter_map(|u| u.to_file_path().ok()).collect();
                    if paths.is_empty() {
                        return;
                    }
                    let handle = handle.clone();
                    // Ce callback est `application:openURLs:`, un extern "C".
                    // Créer les fenêtres dedans panique et macOS aborte le
                    // processus. On les ouvre au tour suivant de la boucle.
                    std::thread::spawn(move || {
                        let app = handle.clone();
                        let _ = handle.run_on_main_thread(move || {
                            drop_packs(&app, &paths);
                        });
                    });
                }
            },
        );
}

fn pick_save_path(
    app: &tauri::AppHandle,
    directory: &Path,
    file_name: &str,
) -> Result<Option<PathBuf>, String> {
    #[cfg(target_os = "macos")]
    {
        return pick_save_path_macos(app, directory, file_name);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let picked = app
            .dialog()
            .file()
            .set_title("Sauvegarder la réponse")
            .set_directory(directory)
            .set_file_name(file_name)
            .add_filter("Markdown", &["md"])
            .set_can_create_directories(true)
            .blocking_save_file();
        match picked {
            None => Ok(None),
            Some(file) => file
                .into_path()
                .map(Some)
                .map_err(|_| "Chemin de sauvegarde illisible.".to_string()),
        }
    }
}

/// Le dossier et le nom sont posés à part : les réunir donne une URL qui n’existe pas.
#[cfg(target_os = "macos")]
fn pick_save_path_macos(
    app: &tauri::AppHandle,
    directory: &Path,
    file_name: &str,
) -> Result<Option<PathBuf>, String> {
    let directory = directory.to_path_buf();
    let file_name = file_name.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(present_save_panel(&directory, &file_name));
    })
    .map_err(|err| format!("fenêtre: {err}"))?;
    rx.recv()
        .map_err(|_| "Sauvegarde interrompue.".to_string())?
}

#[cfg(target_os = "macos")]
fn present_save_panel(directory: &Path, file_name: &str) -> Result<Option<PathBuf>, String> {
    use objc2::rc::autoreleasepool;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSModalResponseOK, NSSavePanel};
    use objc2_foundation::{NSArray, NSString, NSURL};

    autoreleasepool(|_| {
        let Some(mtm) = MainThreadMarker::new() else {
            return Err("Sauvegarde hors du thread principal.".to_string());
        };
        let panel = NSSavePanel::savePanel(mtm);
        panel.setMessage(Some(&NSString::from_str("Sauvegarder la réponse")));
        panel.setCanCreateDirectories(true);
        panel.setAllowsOtherFileTypes(true);
        let ext = NSString::from_str("md");
        let types = NSArray::from_slice(&[&*ext]);
        #[allow(deprecated)]
        panel.setAllowedFileTypes(Some(&types));
        if directory.is_dir() {
            if let Some(path) = directory.to_str() {
                let url = NSURL::fileURLWithPath_isDirectory(&NSString::from_str(path), true);
                panel.setDirectoryURL(Some(&url));
            }
        }
        panel.setNameFieldStringValue(&NSString::from_str(file_name));
        if panel.runModal() != NSModalResponseOK {
            return Ok(None);
        }
        let url = panel
            .URL()
            .ok_or_else(|| "Chemin de sauvegarde illisible.".to_string())?;
        let path = url
            .path()
            .ok_or_else(|| "Chemin de sauvegarde illisible.".to_string())?;
        Ok(Some(PathBuf::from(path.to_string())))
    })
}
