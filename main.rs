mod library;
mod player;
mod ui;

use std::path::Path;

use gtk::prelude::*;

fn main() -> gtk::glib::ExitCode {
    configure_windowing();
    let app = gtk::Application::builder()
        .application_id("io.github.rustmusic.RustMusic")
        .build();

    app.connect_startup(|_| {
        gtk::Window::set_default_icon_name("io.github.rustmusic.RustMusic");
        load_css();
    });
    app.connect_activate(|app| {
        if let Some(window) = app.windows().into_iter().next() {
            window.unminimize();
            window.present();
        } else {
            ui::build(app);
        }
    });
    app.run()
}

/// 选择更省内存的渲染后端，并让窗口在 Wayland 下获得圆角与阴影。
fn configure_windowing() {
    if std::env::var_os("GSK_RENDERER").is_none() {
        std::env::set_var("GSK_RENDERER", "cairo");
    }

    if std::env::var_os("GDK_BACKEND").is_some() {
        return;
    }
    let Some(display) = std::env::var_os("WAYLAND_DISPLAY") else {
        return;
    };

    let display_path = Path::new(&display);
    let socket_exists = if display_path.is_absolute() {
        display_path.exists()
    } else {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(|runtime| Path::new(&runtime).join(&display).exists())
            .unwrap_or(false)
    };

    if socket_exists {
        std::env::set_var("GDK_BACKEND", "wayland");
    }
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(include_str!("style.css"));

    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
