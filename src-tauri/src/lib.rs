mod pack;
mod wacz;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use pack::PackServer;
use tauri::Manager;
use tauri::{DragDropEvent, Emitter, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::DialogExt;

static PACK_ID: AtomicU64 = AtomicU64::new(1);

fn open_pack(app: &tauri::AppHandle, pack_path: PathBuf) -> Result<String, String> {
    let pack_path = pack::resolve_input(&pack_path)?;
    if !is_pack_file(&pack_path) {
        return Err("Fichier .zip, .wacz ou .warc attendu.".into());
    }

    let server = PackServer::open(&pack_path)?;
    let url = server.url();
    let title = format!("Taurus — {}", server.title);
    let id = PACK_ID.fetch_add(1, Ordering::SeqCst);
    let label = format!("pack-{id}");

    let win = WebviewWindowBuilder::new(app, &label, WebviewUrl::External(url.parse().unwrap()))
        .title(&title)
        .inner_size(1100.0, 800.0)
        .build()
        .map_err(|e| format!("fenêtre: {e}"))?;

    win.on_window_event(move |event| {
        if matches!(
            event,
            WindowEvent::Destroyed | WindowEvent::CloseRequested { .. }
        ) {
            server.stop();
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
fn open_pack_path(app: tauri::AppHandle, path: String) -> Result<String, String> {
    open_pack(&app, PathBuf::from(path))
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
                format!("{} n’est pas un .zip / .wacz / .warc", p.display()),
            );
        }
    }
}

fn is_pack(p: &Path) -> bool {
    if p.is_dir() {
        return pack::find_archive_in_dir(p).is_some();
    }
    is_pack_file(p)
}

fn is_pack_file(p: &Path) -> bool {
    pack::is_web_archive(p)
        || p.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![pick_pack, open_pack_path])
        .setup(|app| {
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
        .run(|handle, event| {
            // Double-clic ou drop sur l’icône (dock/Finder) — macOS.
            if let tauri::RunEvent::Opened { urls } = event {
                let paths: Vec<PathBuf> = urls
                    .iter()
                    .filter_map(|u| u.to_file_path().ok())
                    .collect();
                drop_packs(handle, &paths);
            }
        });
}
