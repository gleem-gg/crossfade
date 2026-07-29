pub mod channel_strip;
pub mod dbus;
pub mod effects;
pub mod midi;
pub mod outputs;
pub mod setup;
pub mod sidebar;
pub mod wave_xlr;
pub mod window;

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib, pango};

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
levelbar.meter block {
    min-height: 5px;
    border-radius: 3px;
}
levelbar.meter block.filled.meter-ok {
    background-color: #33d17a;
}
levelbar.meter block.filled.meter-warn {
    background-color: #f5c211;
}
levelbar.meter block.filled.meter-clip {
    background-color: #e01b24;
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

/// A horizontal audio level meter on a dBFS scale, styled green → yellow →
/// red at -20 / -9 dBFS.
#[derive(Clone)]
pub struct Meter {
    pub bar: gtk::LevelBar,
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
        if norm >= self.bar.value() {
            self.bar.set_value(norm);
            return;
        }
        if self.animating.replace(true) {
            return;
        }
        let target = self.target.clone();
        let animating = self.animating.clone();
        let last = Cell::new(None::<i64>);
        self.bar.add_tick_callback(move |bar, clock| {
            let now = clock.frame_time();
            let dt = last.get().map_or(0.0, |p| (now - p) as f64 / 1_000_000.0);
            last.set(Some(now));
            let step = dt * METER_DECAY_DB_PER_SEC / -METER_MIN_DB;
            let next = (bar.value() - step).max(target.get());
            bar.set_value(next);
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
        self.right.bar.set_visible(stereo);
        self.left.set_value(values.first().copied().unwrap_or(0.0));
        self.right.set_value(if stereo { values[1] } else { 0.0 });
    }
}

/// A stereo (left/right) audio level meter, dBFS-scaled like `meter_bar`.
pub fn meter_pair() -> MeterPair {
    let left = meter_bar();
    let right = meter_bar();
    right.bar.set_visible(false);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 2);
    root.append(&left.bar);
    root.append(&right.bar);
    MeterPair { root, left, right }
}

/// A horizontal audio level meter styled green → yellow → red.
pub fn meter_bar() -> Meter {
    let bar = gtk::LevelBar::builder()
        .min_value(0.0)
        .max_value(1.0)
        .mode(gtk::LevelBarMode::Continuous)
        .valign(gtk::Align::Center)
        .build();
    bar.add_css_class("meter");
    for name in ["low", "high", "full"] {
        bar.remove_offset_value(Some(name));
    }
    bar.add_offset_value("meter-ok", 1.0 + -20.0 / -METER_MIN_DB); // < -20 dBFS
    bar.add_offset_value("meter-warn", 1.0 + -9.0 / -METER_MIN_DB); // < -9 dBFS
    bar.add_offset_value("meter-clip", 1.0);
    Meter {
        bar,
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
