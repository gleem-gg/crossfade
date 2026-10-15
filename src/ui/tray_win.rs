//! Tray indicator on Windows: a notification-area icon (`Shell_NotifyIcon`)
//! owned by a hidden message-only window on the GTK main thread. GDK's
//! event source dispatches every message of the thread, so the window
//! procedure runs inside the main loop like any other handler.
//!
//! Same behavior as the Linux StatusNotifierItem: the icon shows (and a
//! click on the menu toggles) the microphone channel's stream mute; a left
//! click presents the window.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, gdk_pixbuf, glib};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject, HBITMAP};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, GetCursorPos, HICON, HMENU, HWND_MESSAGE, ICONINFO, IDI_APPLICATION,
    LoadIconW, MF_CHECKED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CONTEXTMENU, WM_LBUTTONUP,
    WM_RBUTTONUP, WNDCLASSW,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::audio::Mix;
use crate::config::Config;

use super::window::ControlAction;

const ICON_LIVE: &str = "audio-input-microphone-symbolic";
const ICON_MUTED: &str = "microphone-sensitivity-muted-symbolic";
const ICON_SIZE: i32 = 32;

const WM_TRAY: u32 = WM_APP + 1;
const ID_MUTE: usize = 1;
const ID_SHOW: usize = 2;
const ID_QUIT: usize = 3;

pub struct TrayDeps {
    pub config: Rc<RefCell<Config>>,
    pub perform: Rc<dyn Fn(ControlAction)>,
    /// Bring the (possibly hidden) window back.
    pub present: Rc<dyn Fn()>,
    /// The `app.quit` path: save, unload the server state, exit.
    pub quit: Rc<dyn Fn()>,
}

struct Tray {
    deps: TrayDeps,
    hwnd: HWND,
    /// Explorer broadcasts this after (re)starting; the icon must be re-added.
    taskbar_created: u32,
    icon_live: HICON,
    icon_muted: HICON,
    /// Mute state as last shown, so refreshes stay silent.
    shown: Cell<Option<bool>>,
}

thread_local! {
    static TRAY: RefCell<Option<Rc<Tray>>> = const { RefCell::new(None) };
}

/// Show the icon; returns a refresh hook to call after mixer changes.
pub fn register(_application: &adw::Application, deps: TrayDeps) -> Option<Rc<dyn Fn()>> {
    // SAFETY: window class registration and a message-only window owned by
    // the main thread for the life of the process.
    let (hwnd, taskbar_created) = unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let class = w!("GleemCrossfadeTray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("Gleem Crossfade"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        (hwnd, RegisterWindowMessageW(w!("TaskbarCreated")))
    };

    let light = taskbar_is_light();
    let fallback = || {
        // SAFETY: shared system icon.
        unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() }
    };
    let tray = Rc::new(Tray {
        deps,
        hwnd,
        taskbar_created,
        icon_live: themed_icon(ICON_LIVE, light).unwrap_or_else(fallback),
        icon_muted: themed_icon(ICON_MUTED, light).unwrap_or_else(fallback),
        shown: Cell::new(None),
    });
    TRAY.with(|t| *t.borrow_mut() = Some(tray.clone()));
    tray.update(NIM_ADD);
    Some(Rc::new(move || tray.update(NIM_MODIFY)))
}

/// Windows 10/11 taskbar color scheme (light taskbar → dark glyphs).
fn taskbar_is_light() -> bool {
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: DWORD read into a correctly sized buffer.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    r.is_ok() && value != 0
}

/// Render a symbolic theme icon in the taskbar's foreground color.
fn themed_icon(name: &str, light: bool) -> Option<HICON> {
    let display = gdk::Display::default()?;
    let theme = gtk::IconTheme::for_display(&display);
    let paintable = theme.lookup_icon(
        name,
        &[],
        ICON_SIZE,
        1,
        gtk::TextDirection::None,
        gtk::IconLookupFlags::FORCE_SYMBOLIC,
    );
    let path = paintable.file()?.path()?;
    let pixbuf = gdk_pixbuf::Pixbuf::from_file_at_size(path, ICON_SIZE, ICON_SIZE).ok()?;
    let pixbuf = pixbuf.add_alpha(false, 0, 0, 0).ok()?;
    let (w, h) = (pixbuf.width() as usize, pixbuf.height() as usize);
    let stride = pixbuf.rowstride() as usize;
    let bytes = pixbuf.read_pixel_bytes();
    let fg: u8 = if light { 0x1c } else { 0xff };
    // Symbolic icons are single-color; keep only their alpha and paint it
    // in the foreground color. GDI wants premultiplied BGRA, top-down.
    let mut bgra = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let a = bytes[y * stride + x * 4 + 3];
            let c = (u16::from(fg) * u16::from(a) / 255) as u8;
            bgra.extend_from_slice(&[c, c, c, a]);
        }
    }
    // SAFETY: bitmaps sized to the pixel buffer; GDI copies them into the
    // icon, after which they can be deleted.
    unsafe {
        let color: HBITMAP = CreateBitmap(w as i32, h as i32, 1, 32, Some(bgra.as_ptr().cast()));
        let mask_bits = vec![0u8; w.div_ceil(16) * 2 * h];
        let mask: HBITMAP = CreateBitmap(w as i32, h as i32, 1, 1, Some(mask_bits.as_ptr().cast()));
        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

fn copy_wide(dst: &mut [u16], s: &str) {
    let mut n = 0;
    for (d, c) in dst.iter_mut().zip(s.encode_utf16()) {
        *d = c;
        n += 1;
    }
    if n >= dst.len() {
        n = dst.len() - 1;
    }
    dst[n] = 0;
}

impl Tray {
    /// (channel id, stream-muted, name) of the microphone channel.
    fn mic(&self) -> Option<(u64, bool, String)> {
        let config = self.deps.config.borrow();
        super::setup::mic_channel(&config).map(|c| (c.id, c.stream_muted, c.name.clone()))
    }

    fn update(&self, op: windows::Win32::UI::Shell::NOTIFY_ICON_MESSAGE) {
        let mic = self.mic();
        let muted = mic.as_ref().is_some_and(|(_, m, _)| *m);
        if op == NIM_MODIFY && self.shown.get() == Some(muted) {
            return;
        }
        self.shown.set(Some(muted));
        let tip = match &mic {
            Some((_, true, name)) => format!("Gleem Crossfade — {name} muted"),
            _ => "Gleem Crossfade".to_string(),
        };
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: if muted { self.icon_muted } else { self.icon_live },
            ..Default::default()
        };
        copy_wide(&mut data.szTip, &tip);
        // SAFETY: fully initialized NOTIFYICONDATAW for our window.
        unsafe {
            let _ = Shell_NotifyIconW(op, &data);
        }
    }

    fn show_menu(&self) {
        let mic = self.mic();
        // SAFETY: a transient popup menu, destroyed before returning.
        let chosen = unsafe {
            let Ok(menu): windows::core::Result<HMENU> = CreatePopupMenu() else {
                return;
            };
            let label = HSTRING::from(match &mic {
                Some((_, _, name)) => format!("Mute {name}"),
                None => "Mute Microphone".to_string(),
            });
            let checked = if mic.as_ref().is_some_and(|(_, m, _)| *m) {
                MF_CHECKED
            } else {
                MF_UNCHECKED
            };
            let _ = AppendMenuW(menu, MF_STRING | checked, ID_MUTE, PCWSTR(label.as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, ID_SHOW, w!("Show Crossfade"));
            let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("Quit"));
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // Required so the menu closes when clicking elsewhere.
            let _ = SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                pt.x,
                pt.y,
                None,
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
            cmd.0 as usize
        };
        match chosen {
            ID_MUTE => {
                if let Some((id, _, _)) = mic {
                    (self.deps.perform)(ControlAction::SetChannelMute {
                        id,
                        mix: Mix::Stream,
                        muted: None,
                    });
                }
            }
            ID_SHOW => (self.deps.present)(),
            ID_QUIT => (self.deps.quit)(),
            _ => {}
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        };
        // SAFETY: removing our own icon and freeing our icon handles.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
            let _ = DestroyIcon(self.icon_live);
            let _ = DestroyIcon(self.icon_muted);
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let tray = TRAY.with(|t| t.borrow().clone());
    if let Some(tray) = tray {
        if msg == WM_TRAY {
            match lparam.0 as u32 {
                WM_LBUTTONUP => {
                    // Defer out of the window procedure: presenting can
                    // pump messages itself.
                    let present = tray.deps.present.clone();
                    glib::idle_add_local_once(move || present());
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    glib::idle_add_local_once(move || tray.show_menu());
                }
                _ => {}
            }
            return LRESULT(0);
        }
        if msg == tray.taskbar_created && msg != 0 {
            tray.shown.set(None);
            tray.update(NIM_ADD);
            return LRESULT(0);
        }
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
