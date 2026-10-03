mod ask;
mod md;
mod pack;
mod wacz;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use pack::PackServer;
use tauri::Manager;
use tauri::{DragDropEvent, Emitter, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::DialogExt;

static PACK_ID: AtomicU64 = AtomicU64::new(1);

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
    }
}

fn open_pack(app: &tauri::AppHandle, pack_path: PathBuf) -> Result<String, String> {
    let server = Arc::new(PackServer::open(&pack_path)?);
    let url = server.url();
    let title = format!("Taurus — {}", server.title);
    let id = PACK_ID.fetch_add(1, Ordering::SeqCst);
    let pack_label = format!("pack-{id}");
    let ask_label = format!("ask-{id}");
    let parsed: tauri::Url = url.parse().map_err(|e| {
        server.stop();
        format!("url: {e}")
    })?;
    let live = Arc::new(ask::LivePack::new(
        server.root.clone(),
        server.markdown,
        server.port,
        server.index.clone(),
    ));
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
            if let Err(e) = open_pack(&app, path) {
                let _ = app.emit("taurus-error", e);
            }
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
            let path = PathBuf::from(path.to_string());
            if let Err(e) = open_pack(&app, path) {
                let _ = app.emit("taurus-error", e);
            }
        });
}

#[tauri::command]
fn open_pack_path(app: tauri::AppHandle, path: String) -> Result<String, String> {
    open_pack(&app, PathBuf::from(path))
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
    tauri::async_runtime::spawn_blocking(move || pack.question(&question, &host, model.as_deref()))
        .await
        .map_err(|_| "Interrogation interrompue.".to_string())?
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
            if let Err(e) = open_pack(app, p.clone()) {
                let _ = app.emit("taurus-error", e);
            }
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
            ask_set_model
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
                    if let Err(e) = open_pack(&handle, p) {
                        let _ = handle.emit("taurus-error", e);
                    }
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
