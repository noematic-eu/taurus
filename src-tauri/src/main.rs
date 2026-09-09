// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // WebKitGTK 2.42+ DMA-BUF/GBM abort on NVIDIA/Wayland (EGL_NOT_INITIALIZED).
    // Same combo as: GDK_BACKEND=x11 WEBKIT_DISABLE_DMABUF_RENDERER=1 ./Taurus.AppImage
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
        if std::env::var_os("GDK_BACKEND").is_none() {
            std::env::set_var("GDK_BACKEND", "x11");
        }
    }
    taurus_lib::run()
}
