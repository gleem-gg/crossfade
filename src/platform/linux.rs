//! Linux integration: XDG autostart.

use gtk::glib;

fn autostart_file() -> std::path::PathBuf {
    glib::user_config_dir()
        .join("autostart")
        .join(format!("{}.desktop", crate::APP_ID))
}

pub fn autostart_enabled() -> bool {
    autostart_file().exists()
}

/// Enable/disable launch-on-login via an XDG autostart entry. The entry runs
/// the current executable with --hidden so the virtual devices come up in
/// the background without opening the window.
pub fn set_autostart(enable: bool) -> std::io::Result<()> {
    let path = autostart_file();
    if !enable {
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "gleem-crossfade".to_string());
    std::fs::write(
        &path,
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Gleem Crossfade\n\
             Comment=Dual-mix virtual audio mixer for streaming\n\
             Exec={exe} --hidden\n\
             Icon={}\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n",
            crate::APP_ID
        ),
    )
}
