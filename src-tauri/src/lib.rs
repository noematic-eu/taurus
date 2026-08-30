mod pack;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use pack::PackServer;
use tauri::Manager;
use tauri::{DragDropEvent, Emitter, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::DialogExt;

static PACK_ID: AtomicU64 = AtomicU64::new(1);

fn open_zip(app: &tauri::AppHandle, zip_path: PathBuf) -> Result<String, String> {
    if zip_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| !e.eq_ignore_ascii_case("zip"))
        .unwrap_or(true)
    {
        return Err("Fichier .zip attendu.".into());
    }

    let server = PackServer::open(&zip_path)?;
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
        .add_filter("Pack web (zip)", &["zip"])
        .pick_file(move |file| {
            let Some(path) = file else {
                return;
            };
            let path = PathBuf::from(path.to_string());
            if let Err(e) = open_zip(&app, path) {
                let _ = app.emit("taurus-error", e);
            }
        });
}

#[tauri::command]
fn open_pack_path(app: tauri::AppHandle, path: String) -> Result<String, String> {
    open_zip(&app, PathBuf::from(path))
}

fn drop_zips(app: &tauri::AppHandle, paths: &[PathBuf]) {
    for p in paths {
        if is_zip(p) {
            if let Err(e) = open_zip(app, p.clone()) {
                let _ = app.emit("taurus-error", e);
            }
        } else {
            let _ = app.emit("taurus-error", format!("{} n’est pas un .zip", p.display()));
        }
    }
}

fn is_zip(p: &Path) -> bool {
    p.extension()
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
                        drop_zips(&handle, paths);
                    }
                });
            }

            let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
            let handle = app.handle().clone();
            for p in args {
                if is_zip(&p) {
                    if let Err(e) = open_zip(&handle, p) {
                        let _ = handle.emit("taurus-error", e);
                    }
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("erreur Taurus");
}
