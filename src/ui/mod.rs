pub mod channel_strip;
pub mod dbus;
pub mod effects;
pub mod input_picker;
pub mod midi;
pub mod outputs;
pub mod setup;
pub mod sidebar;
pub mod tray;
pub mod wave_xlr;
pub mod window;

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{cairo, gdk, glib, pango};

const CSS: &str = r#"
.channel-strip {
    padding: 12px;
    min-width: 128px;
}
.channel-strip scale.fader {
    min-height: 200px;
}
/* The channel color button: a slim chip, not a full-height button. */
.channel-strip .color-chip {
    min-height: 0;
    padding: 2px 4px;
}
.meter {
    min-width: 32px;
    min-height: 5px;
}
.mute-toggle:checked {
    color: #e01b24;
    background-color: alpha(#e01b24, 0.12);
}
"#;

pub fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        // Make the bundled symbolic icons (registered as a GResource in
        // main) resolvable by name like theme icons.
        gtk::IconTheme::for_display(&display)
            .add_resource_path("/gg/gleem/Crossfade/icons");
    }
}

/// Meter scale and ballistics: the bar's 0..1 spans METER_MIN_DB..0 dBFS,
/// attack is instant, and the bar falls at METER_DECAY_DB_PER_SEC
/// (PPM-style, close to OBS's fast peak meter).
const METER_MIN_DB: f64 = -60.0;
const METER_DECAY_DB_PER_SEC: f64 = 40.0;

/// Fill colour by *position on the scale*, as (upper bound in dBFS, RGB):
/// green up to -20 dBFS, yellow up to -9 dBFS, red above. The boundaries are
/// hard stops so they stay legible as the dBFS thresholds they are.
const METER_STOPS: [(f64, u32); 3] = [(-20.0, 0x33d17a), (-9.0, 0xf5c211), (0.0, 0xe01b24)];
/// Tint of the unfilled trough, taken from the foreground colour so it
/// follows the theme.
const METER_TROUGH_ALPHA: f64 = 0.15;
const METER_RADIUS: f64 = 3.0;

/// A horizontal audio level meter on a dBFS scale, painted with a fixed
/// green → yellow → red gradient (breaking at -20 / -9 dBFS) that is clipped
/// to the current level — so the body of the signal stays green and only the
/// tip enters yellow or red.
#[derive(Clone)]
pub struct Meter {
    pub widget: gtk::DrawingArea,
    /// Level currently drawn (normalized), between `target` and the last peak.
    value: Rc<Cell<f64>>,
    /// Latest measured level (normalized); the decay tick falls toward it.
    target: Rc<Cell<f64>>,
    animating: Rc<Cell<bool>>,
}

impl Meter {
    /// Feed a linear amplitude peak (0..1) from a peek stream. Rises show
    /// immediately; falls are animated at the decay rate.
    pub fn set_value(&self, linear: f64) {
        let norm = Self::normalize(linear);
        self.target.set(norm);
        if norm >= self.value.get() {
            if self.value.replace(norm) != norm {
                self.widget.queue_draw();
            }
            return;
        }
        if self.animating.replace(true) {
            return;
        }
        let value = self.value.clone();
        let target = self.target.clone();
        let animating = self.animating.clone();
        let last = Cell::new(None::<i64>);
        self.widget.add_tick_callback(move |area, clock| {
            let now = clock.frame_time();
            let dt = last.get().map_or(0.0, |p| (now - p) as f64 / 1_000_000.0);
            last.set(Some(now));
            let step = dt * METER_DECAY_DB_PER_SEC / -METER_MIN_DB;
            let next = (value.get() - step).max(target.get());
            value.set(next);
            area.queue_draw();
            if next <= target.get() {
                animating.set(false);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }

    /// Map linear amplitude onto the bar's dBFS scale.
    fn normalize(linear: f64) -> f64 {
        if linear <= 0.0 {
            return 0.0;
        }
        let db = 20.0 * linear.log10();
        (1.0 + db / -METER_MIN_DB).clamp(0.0, 1.0)
    }
}

/// Paint the trough, then the scale-wide gradient clipped to `value`.
fn draw_meter(area: &gtk::DrawingArea, cr: &cairo::Context, w: f64, h: f64, value: f64) {
    let radius = METER_RADIUS.min(w / 2.0).min(h / 2.0);
    let fg = area.color();
    meter_outline(cr, w, h, radius);
    cr.set_source_rgba(
        fg.red() as f64,
        fg.green() as f64,
        fg.blue() as f64,
        METER_TROUGH_ALPHA * fg.alpha() as f64,
    );
    let _ = cr.fill();
    if value <= 0.0 {
        return;
    }

    // The gradient spans the whole scale and the clip cuts it to the current
    // level, so a given dBFS value always lands on the same colour.
    meter_outline(cr, w, h, radius);
    cr.clip();
    cr.rectangle(0.0, 0.0, w * value, h);
    cr.clip();
    let channel = |hex: u32, shift: u32| ((hex >> shift) & 0xff) as f64 / 255.0;
    let gradient = cairo::LinearGradient::new(0.0, 0.0, w, 0.0);
    let mut from = 0.0;
    for (db, hex) in METER_STOPS {
        let to = 1.0 + db / -METER_MIN_DB;
        let (r, g, b) = (channel(hex, 16), channel(hex, 8), channel(hex, 0));
        gradient.add_color_stop_rgb(from, r, g, b);
        gradient.add_color_stop_rgb(to, r, g, b);
        from = to;
    }
    let _ = cr.set_source(&gradient);
    let _ = cr.paint();
}

/// The meter's rounded-rectangle outline, as a fresh path.
fn meter_outline(cr: &cairo::Context, w: f64, h: f64, radius: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.new_path();
    cr.arc(w - radius, radius, radius, -FRAC_PI_2, 0.0);
    cr.arc(w - radius, h - radius, radius, 0.0, FRAC_PI_2);
    cr.arc(radius, h - radius, radius, FRAC_PI_2, PI);
    cr.arc(radius, radius, radius, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

/// Two stacked meter bars for stereo signals. The right bar hides itself
/// while the meter is fed mono levels.
pub struct MeterPair {
    pub root: gtk::Box,
    left: Meter,
    right: Meter,
}

impl MeterPair {
    /// Feed measured peaks: one value for mono, two for stereo.
    pub fn set_levels(&self, values: &[f64]) {
        let stereo = values.len() >= 2;
        self.right.widget.set_visible(stereo);
        self.left.set_value(values.first().copied().unwrap_or(0.0));
        self.right.set_value(if stereo { values[1] } else { 0.0 });
    }
}

/// A stereo (left/right) audio level meter, dBFS-scaled like `meter_bar`.
pub fn meter_pair() -> MeterPair {
    let left = meter_bar();
    let right = meter_bar();
    right.widget.set_visible(false);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 2);
    root.append(&left.widget);
    root.append(&right.widget);
    MeterPair { root, left, right }
}

/// A horizontal audio level meter carrying a fixed green → yellow → red
/// gradient along its length, clipped to the current level.
pub fn meter_bar() -> Meter {
    let value = Rc::new(Cell::new(0.0));
    let widget = gtk::DrawingArea::builder()
        .valign(gtk::Align::Center)
        .accessible_role(gtk::AccessibleRole::Meter)
        .build();
    widget.add_css_class("meter");
    widget.set_draw_func({
        let value = value.clone();
        move |area, cr, w, h| draw_meter(area, cr, f64::from(w), f64::from(h), value.get())
    });
    Meter {
        widget,
        value,
        target: Rc::new(Cell::new(0.0)),
        animating: Rc::new(Cell::new(false)),
    }
}

/// List-item factory rendering plain strings with end-ellipsizing, for use in
/// drop-downs whose entries can be long device names. With `fixed` the label
/// always requests exactly `chars` characters, so the drop-down (and its
/// container) keeps the same width regardless of the selected item.
pub fn label_factory(chars: i32, fixed: bool) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, obj| {
        let Some(item) = obj.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .max_width_chars(chars)
            .build();
        if fixed {
            label.set_width_chars(chars);
        }
        item.set_child(Some(&label));
    });
    factory.connect_bind(|_, obj| {
        let Some(item) = obj.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(label) = item.child().and_downcast::<gtk::Label>() else {
            return;
        };
        let text = item
            .item()
            .and_downcast::<gtk::StringObject>()
            .map(|s| s.string().to_string())
            .unwrap_or_default();
        label.set_text(&text);
        label.set_tooltip_text(Some(&text));
    });
    factory
}

pub fn heading_label(text: &str) -> gtk::Label {
    let label = gtk::Label::builder().label(text).xalign(0.0).build();
    label.add_css_class("heading");
    label
}

pub fn mute_button(icon: &str, tooltip: &str) -> gtk::ToggleButton {
    let btn = gtk::ToggleButton::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    btn.add_css_class("flat");
    btn.add_css_class("circular");
    btn.add_css_class("mute-toggle");
    btn
}
