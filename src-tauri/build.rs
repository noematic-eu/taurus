fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "pick_pack",
            "pick_folder",
            "open_pack_path",
            "ask_state",
            "ask_add",
            "ask_remove",
            "ask_question",
            "ask_models",
            "ask_set_model",
            "ask_save",
            "ask_open",
            "ask_rendered",
            "pick_link_check",
            "link_report",
        ]),
    ))
    .expect("erreur du script de build Tauri");
}
