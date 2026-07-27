use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio};

use crate::config::{Assignment, ChannelConfig};

use super::{label_factory, meter_pair, mute_button, MeterPair};

/// Fixed width of every channel strip (and the add-channel card).
pub const STRIP_WIDTH: i32 = 150;
/// Wider strip while the third (VOD) fader is shown.
const STRIP_WIDTH_VOD: i32 = 200;

/// Slot for the handler invoked with a newly picked channel color
/// ("#rrggbb"; `None` = color removed).
type ColorHandler = Rc<RefCell<Option<Rc<dyn Fn(Option<String>)>>>>;

/// Preset channel colors offered in the picker popover (GNOME palette hues).
const PRESET_COLORS: [(&str, &str); 10] = [
    ("Blue", "#3584e4"),
    ("Teal", "#2190a4"),
    ("Green", "#2ec27e"),
    ("Yellow", "#f5c211"),
    ("Orange", "#ff7800"),
    ("Red", "#e01b24"),
    ("Rose", "#f66fa8"),
    ("Purple", "#9141ac"),
    ("Brown", "#986a44"),
    ("Slate", "#6f8396"),
];

/// One vertical input strip: rename label, input selector, level meter and
/// independent per-mix faders (monitor, stream, and — when the VOD mix is
/// enabled — VOD) with per-mix mute buttons and a single link toggle that
/// ties all faders together.
pub struct ChannelStrip {
    pub root: gtk::Box,
    pub name: gtk::EditableLabel,
    pub remove: gtk::Button,
    pub fx: gtk::Button,
    /// Color-bar button opening the channel color picker; also the
    /// right-click anchor for binding a color pad on a MIDI controller.
    pub color: gtk::MenuButton,
    color_swatch: gtk::DrawingArea,
    color_value: Rc<RefCell<Option<gdk::RGBA>>>,
    color_changed: ColorHandler,
    pub input: gtk::DropDown,
    level: MeterPair,
    pub monitor_scale: gtk::Scale,
    pub stream_scale: gtk::Scale,
    pub vod_scale: gtk::Scale,
    pub monitor_mute: gtk::ToggleButton,
    pub stream_mute: gtk::ToggleButton,
    pub vod_mute: gtk::ToggleButton,
    pub link: gtk::ToggleButton,
    vod_caption: gtk::Label,
    /// Set while the strip is being updated programmatically so signal
    /// handlers know not to write back into the config.
    pub guard: Rc<Cell<bool>>,
    /// Assignment behind each drop-down position (index-aligned with the
    /// model). Shared with the selection handler.
    pub entries: Rc<RefCell<Vec<Option<Assignment>>>>,
    last_labels: RefCell<Vec<String>>,
}

fn fader() -> gtk::Scale {
    let scale = gtk::Scale::with_range(gtk::Orientation::Vertical, 0.0, 1.0, 0.01);
    scale.set_inverted(true);
    scale.set_draw_value(false);
    scale.set_vexpand(true);
    scale.set_halign(gtk::Align::Center);
    scale.add_css_class("fader");
    scale
}

/// One row of the fader block. Homogeneous, so every row splits the strip
/// into equal columns and the scales, mute buttons and captions line up —
/// also when the VOD column is hidden and the rows fall back to two columns.
fn mix_row() -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    row.set_homogeneous(true);
    row
}

fn caption_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    label
}

fn pill_path(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64) {
    let r = h / 2.0;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, PI / 2.0);
    cr.arc(x + r, y + r, r, PI / 2.0, 1.5 * PI);
    cr.close_path();
}

/// Round color swatch for the picker popover.
fn swatch_button(rgba: gdk::RGBA, tooltip: &str) -> gtk::Button {
    let area = gtk::DrawingArea::new();
    area.set_content_width(18);
    area.set_content_height(18);
    area.set_draw_func(move |_, cr, w, h| {
        let (w, h) = (w as f64, h as f64);
        cr.arc(w / 2.0, h / 2.0, w.min(h) / 2.0, 0.0, 2.0 * PI);
        cr.set_source_rgb(rgba.red() as f64, rgba.green() as f64, rgba.blue() as f64);
        let _ = cr.fill();
    });
    let btn = gtk::Button::builder()
        .child(&area)
        .tooltip_text(tooltip)
        .build();
    btn.add_css_class("flat");
    btn
}

impl ChannelStrip {
    pub fn new() -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .width_request(STRIP_WIDTH)
            .css_classes(["card", "channel-strip"])
            .build();

        let name = gtk::EditableLabel::builder()
            .text("")
            .xalign(0.0)
            .hexpand(true)
            .max_width_chars(12)
            .tooltip_text("Rename channel")
            .build();
        name.add_css_class("heading");

        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove channel")
            .valign(gtk::Align::Center)
            .build();
        remove.add_css_class("flat");
        remove.add_css_class("circular");

        let fx = gtk::Button::builder()
            .icon_name("sound-wave-symbolic")
            .tooltip_text("Effects")
            .valign(gtk::Align::Center)
            .build();
        fx.add_css_class("flat");
        fx.add_css_class("circular");

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        header.append(&name);
        header.append(&fx);
        header.append(&remove);

        // ---- Channel color: a color-bar button opening a swatch popover ----
        let color_value: Rc<RefCell<Option<gdk::RGBA>>> = Rc::new(RefCell::new(None));
        let color_changed: ColorHandler = Rc::new(RefCell::new(None));

        let color_swatch = gtk::DrawingArea::new();
        color_swatch.set_content_height(12);
        color_swatch.set_hexpand(true);
        // Center it so the chip keeps exactly this height instead of being
        // stretched to the button's allocation.
        color_swatch.set_valign(gtk::Align::Center);
        {
            let value = color_value.clone();
            color_swatch.set_draw_func(move |area, cr, w, h| {
                let (w, h) = (w as f64, h as f64);
                if let Some(c) = &*value.borrow() {
                    pill_path(cr, 0.0, 0.0, w, h);
                    cr.set_source_rgb(c.red() as f64, c.green() as f64, c.blue() as f64);
                    let _ = cr.fill();
                } else {
                    // Unset: a dashed outline in the theme foreground color.
                    let fg = area.color();
                    pill_path(cr, 0.5, 0.5, w - 1.0, h - 1.0);
                    cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.4);
                    cr.set_line_width(1.0);
                    cr.set_dash(&[3.0, 3.0], 0.0);
                    let _ = cr.stroke();
                }
            });
        }

        let popover = gtk::Popover::new();
        let color = gtk::MenuButton::builder()
            .tooltip_text("Channel color")
            .popover(&popover)
            .build();
        color.add_css_class("flat");
        color.add_css_class("color-chip");
        color.set_child(Some(&color_swatch));

        // Applies a pick: updates the swatch and notifies the handler.
        let choose: Rc<dyn Fn(Option<gdk::RGBA>)> = {
            let value = color_value.clone();
            let swatch = color_swatch.clone();
            let changed = color_changed.clone();
            let popover = popover.clone();
            Rc::new(move |c: Option<gdk::RGBA>| {
                popover.popdown();
                let hex = c.as_ref().map(|c| {
                    format!(
                        "#{:02x}{:02x}{:02x}",
                        (c.red() * 255.0).round() as u8,
                        (c.green() * 255.0).round() as u8,
                        (c.blue() * 255.0).round() as u8
                    )
                });
                *value.borrow_mut() = c;
                swatch.queue_draw();
                let changed = changed.borrow().clone();
                if let Some(changed) = changed {
                    changed(hex);
                }
            })
        };

        let grid = gtk::Grid::builder().row_spacing(2).column_spacing(2).build();
        for (i, (label, hex)) in PRESET_COLORS.iter().enumerate() {
            let rgba = gdk::RGBA::parse(*hex).expect("valid preset color");
            let btn = swatch_button(rgba, label);
            let choose = choose.clone();
            btn.connect_clicked(move |_| choose(Some(rgba)));
            grid.attach(&btn, (i % 5) as i32, (i / 5) as i32, 1, 1);
        }

        let custom = gtk::Button::with_label("Custom…");
        custom.add_css_class("flat");
        {
            let choose = choose.clone();
            let value = color_value.clone();
            let popover = popover.clone();
            custom.connect_clicked(move |btn| {
                popover.popdown();
                let dialog = gtk::ColorDialog::builder()
                    .title("Channel Color")
                    .with_alpha(false)
                    .build();
                let initial = value.borrow().unwrap_or_else(|| {
                    gdk::RGBA::parse(PRESET_COLORS[0].1).expect("valid preset color")
                });
                let root = btn.root().and_downcast::<gtk::Window>();
                let choose = choose.clone();
                dialog.choose_rgba(
                    root.as_ref(),
                    Some(&initial),
                    gio::Cancellable::NONE,
                    move |res| {
                        if let Ok(c) = res {
                            choose(Some(c));
                        }
                    },
                );
            });
        }

        let none = gtk::Button::with_label("No Color");
        none.add_css_class("flat");
        {
            let choose = choose.clone();
            none.connect_clicked(move |_| choose(None));
        }

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        actions.set_homogeneous(true);
        actions.append(&custom);
        actions.append(&none);
        let pop_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        pop_box.append(&grid);
        pop_box.append(&actions);
        popover.set_child(Some(&pop_box));

        let input = gtk::DropDown::builder()
            .tooltip_text("Select the input for this channel")
            .build();
        input.set_factory(Some(&label_factory(9, true)));
        input.set_list_factory(Some(&label_factory(36, false)));

        let level = meter_pair();

        let monitor_scale = fader();
        monitor_scale.set_tooltip_text(Some("Monitor mix volume"));
        let stream_scale = fader();
        stream_scale.set_tooltip_text(Some("Stream mix volume"));
        let vod_scale = fader();
        vod_scale.set_tooltip_text(Some("VOD mix volume"));

        let monitor_mute = mute_button("audio-headphones-symbolic", "Mute in the monitor mix");
        let stream_mute = mute_button("media-record-symbolic", "Mute in the stream mix");
        let vod_mute = mute_button("camera-video-symbolic", "Mute in the VOD mix");

        let link = gtk::ToggleButton::builder()
            .icon_name("insert-link-symbolic")
            .tooltip_text("Link the faders")
            .halign(gtk::Align::Center)
            .build();
        link.add_css_class("flat");
        link.add_css_class("circular");

        // Fader block: a row of scales, the link toggle, a row of mute
        // buttons and a row of captions, in equal columns so each fader's
        // controls stack up exactly below it.
        let scales_row = mix_row();
        scales_row.set_vexpand(true);
        scales_row.append(&monitor_scale);
        scales_row.append(&stream_scale);
        scales_row.append(&vod_scale);

        let mutes_row = mix_row();
        mutes_row.append(&monitor_mute);
        mutes_row.append(&stream_mute);
        mutes_row.append(&vod_mute);

        let captions_row = mix_row();
        let vod_caption = caption_label("VOD");
        captions_row.append(&caption_label("Monitor"));
        captions_row.append(&caption_label("Stream"));
        captions_row.append(&vod_caption);

        vod_scale.set_visible(false);
        vod_mute.set_visible(false);
        vod_caption.set_visible(false);

        let faders = gtk::Box::new(gtk::Orientation::Vertical, 6);
        faders.set_vexpand(true);
        faders.append(&scales_row);
        faders.append(&link);
        faders.append(&mutes_row);
        faders.append(&captions_row);

        root.append(&header);
        root.append(&color);
        root.append(&input);
        root.append(&level.root);
        root.append(&faders);

        Self {
            root,
            name,
            remove,
            fx,
            color,
            color_swatch,
            color_value,
            color_changed,
            input,
            level,
            monitor_scale,
            stream_scale,
            vod_scale,
            monitor_mute,
            stream_mute,
            vod_mute,
            link,
            vod_caption,
            guard: Rc::new(Cell::new(false)),
            entries: Rc::new(RefCell::new(Vec::new())),
            last_labels: RefCell::new(Vec::new()),
        }
    }

    /// Feed measured peaks: one value for mono inputs, two for stereo. The
    /// right meter is shown only while the input reports stereo levels.
    pub fn set_levels(&self, values: &[f64]) {
        self.level.set_levels(values);
    }

    /// Register the handler invoked when the user picks a channel color
    /// (`None` = color removed).
    pub fn connect_color_changed(&self, f: impl Fn(Option<String>) + 'static) {
        *self.color_changed.borrow_mut() = Some(Rc::new(f));
    }

    /// Show a color without invoking the change handler.
    fn set_color(&self, hex: Option<&str>) {
        *self.color_value.borrow_mut() = hex.and_then(|h| gdk::RGBA::parse(h).ok());
        self.color_swatch.queue_draw();
    }

    /// Push the current config values into the widgets without firing the
    /// user-edit handlers.
    pub fn load_config(&self, c: &ChannelConfig) {
        self.guard.set(true);
        self.name.set_text(&c.name);
        self.set_color(c.color.as_deref());
        self.monitor_scale.set_value(c.monitor_volume);
        self.stream_scale.set_value(c.stream_volume);
        self.vod_scale.set_value(c.vod_volume);
        self.monitor_mute.set_active(c.monitor_muted);
        self.stream_mute.set_active(c.stream_muted);
        self.vod_mute.set_active(c.vod_muted);
        self.link.set_active(c.linked);
        self.update_fx_indicator(c);
        self.guard.set(false);
    }

    /// Show or hide the VOD fader column (and widen the strip to fit it).
    pub fn set_vod_visible(&self, visible: bool) {
        self.vod_scale.set_visible(visible);
        self.vod_mute.set_visible(visible);
        self.vod_caption.set_visible(visible);
        self.root.set_width_request(if visible {
            STRIP_WIDTH_VOD
        } else {
            STRIP_WIDTH
        });
    }

    /// Tint the FX button while the channel processes through effects.
    pub fn update_fx_indicator(&self, c: &ChannelConfig) {
        let lv2 = c.effects.iter().filter(|e| e.enabled).count();
        let vst = c.vst_plugins.iter().filter(|p| p.enabled).count();
        if c.fx_active() {
            self.fx.add_css_class("accent");
        } else {
            self.fx.remove_css_class("accent");
        }
        let mut tip = String::from("Effects");
        let mut parts = Vec::new();
        if vst > 0 {
            parts.push(format!("{vst} VST"));
        }
        if lv2 > 0 {
            parts.push(format!("{lv2} LV2"));
        }
        if !parts.is_empty() {
            tip = format!("Effects ({} active)", parts.join(" + "));
        }
        self.fx.set_tooltip_text(Some(&tip));
    }

    /// Rebuild the input drop-down. `current` is kept selected; if it is not
    /// in `items` (device unplugged, app not running) a placeholder entry is
    /// appended so the assignment is not silently lost.
    pub fn set_input_entries(
        &self,
        items: &[(String, Option<Assignment>)],
        current: &Option<Assignment>,
    ) {
        let mut labels: Vec<String> = items.iter().map(|(l, _)| l.clone()).collect();
        let mut assigns: Vec<Option<Assignment>> = items.iter().map(|(_, a)| a.clone()).collect();
        let mut selected = assigns.iter().position(|a| a == current);
        if selected.is_none()
            && let Some(a) = current {
                let label = match a {
                    Assignment::Source { name } => format!("{name} (unavailable)"),
                    Assignment::App { name } => format!("{name} (not running)"),
                    Assignment::Virtual => "Virtual Device".to_string(),
                };
                labels.push(label);
                assigns.push(Some(a.clone()));
                selected = Some(labels.len() - 1);
            }
        let selected = selected.unwrap_or(0) as u32;
        if *self.last_labels.borrow() == labels && self.input.selected() == selected {
            return;
        }
        self.guard.set(true);
        let strs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let model = gtk::StringList::new(&strs);
        self.input.set_model(Some(&model));
        self.input.set_selected(selected);
        self.guard.set(false);
        *self.last_labels.borrow_mut() = labels;
        *self.entries.borrow_mut() = assigns;
    }
}
