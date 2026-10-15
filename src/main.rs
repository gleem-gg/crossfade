// No console window behind the GUI on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod audio;
mod config;
#[cfg(target_os = "linux")]
mod fx;
#[cfg(target_os = "linux")]
mod lv2;
mod midi;
mod platform;
mod ui;
mod vst;

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

pub const APP_ID: &str = "gg.gleem.Crossfade";

fn main() -> glib::ExitCode {
    // Bundled resources (symbolic icons not shipped by system themes).
    let resources = gtk::gio::Resource::from_data(&glib::Bytes::from_static(
        include_bytes!(concat!(env!("OUT_DIR"), "/gleem-crossfade.gresource")),
    ))
    .expect("embedded gresource is valid");
    gtk::gio::resources_register(&resources);

    // One-time migration of config/state from the pre-rename OpenWave identity.
    if config::migrate_from_openwave() {
        // A legacy autostart entry existed; recreate it under the new app id.
        if let Err(e) = platform::set_autostart(true) {
            eprintln!("gleem-crossfade: could not migrate the autostart entry: {e}");
        }
    }

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.add_main_option(
        "hidden",
        glib::Char::from(0u8),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Start with the window hidden (used by autostart)",
        None,
    );
    #[cfg(target_os = "linux")]
    add_effect_options(&app);
    let start_hidden = Rc::new(Cell::new(false));
    {
        let start_hidden = start_hidden.clone();
        app.connect_handle_local_options(move |_, options| {
            #[cfg(target_os = "linux")]
            if let Some(code) = handle_effect_options(options) {
                return std::ops::ControlFlow::Break(code);
            }
            if options.contains("hidden") {
                start_hidden.set(true);
            }
            std::ops::ControlFlow::Continue(()) // continue normal processing
        });
    }
    app.connect_startup(|_| {
        ui::load_css();
        // Scan the LV2 world in the background so channel wiring and the
        // effects dialog don't freeze the UI on first use.
        #[cfg(target_os = "linux")]
        lv2::warm();
        // Warm the VST discovery cache so the plugin picker opens fast.
        #[cfg(target_os = "linux")]
        if vst::available() {
            std::thread::spawn(|| {
                let _ = vst::scan();
            });
        }
    });
    app.connect_activate(move |app| {
        // Closing only hides the window; re-activation brings it back.
        if let Some(window) = app
            .active_window()
            .or_else(|| app.windows().into_iter().next())
        {
            window.present();
            return;
        }
        let window = ui::window::build(app);
        // With --hidden the window is created but not shown: the virtual
        // devices come up in the background, the window appears on the next
        // activation.
        if !start_hidden.replace(false) {
            window.present();
        }
    });
    app.set_accels_for_action("app.quit", &["<primary>q"]);
    app.run()
}

#[cfg(target_os = "linux")]
fn add_effect_options(app: &adw::Application) {
    app.add_main_option(
        "list-lv2",
        glib::Char::from(0u8),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "List usable LV2 plugins and exit (diagnostic)",
        None,
    );
    app.add_main_option(
        "list-vst",
        glib::Char::from(0u8),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "List usable VST plugins and exit (diagnostic)",
        None,
    );
}

/// `--list-lv2` / `--list-vst`: print the plugin catalog and exit.
#[cfg(target_os = "linux")]
fn handle_effect_options(options: &glib::VariantDict) -> Option<glib::ExitCode> {
    if options.contains("list-lv2") {
        match lv2::catalog() {
            Some(cat) => {
                for p in &cat.plugins {
                    let ch = if p.is_mono() { "mono" } else { "stereo" };
                    println!("{} [{ch}, {} controls]\n    {}", p.name, p.controls.len(), p.uri);
                }
                eprintln!("{} usable plugins", cat.plugins.len());
                return Some(glib::ExitCode::SUCCESS);
            }
            None => {
                eprintln!("liblilv could not be loaded");
                return Some(glib::ExitCode::FAILURE);
            }
        }
    }
    if options.contains("list-vst") {
        if !vst::available() {
            eprintln!("VST hosting unavailable (Carla and/or python3 missing)");
            return Some(glib::ExitCode::FAILURE);
        }
        let entries = vst::scan();
        for e in &entries {
            println!(
                "{} [{}] {}{}",
                e.name,
                e.format.as_str(),
                e.path,
                if e.label.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", e.label)
                }
            );
        }
        eprintln!("{} usable plugins", entries.len());
        return Some(glib::ExitCode::SUCCESS);
    }
    None
}
