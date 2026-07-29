//! Elgato Wave XLR settings: the microphone gain and headphone output
//! volume stored here are re-applied every time Crossfade starts, working
//! around a firmware bug where the device occasionally forgets its levels.
//!
//! The subtitle previews follow the device's LED ring: PipeWire's software
//! volume is a cubic taper (dB = 60·log10(percent/100)) and the ring maps
//! dB to lit segments through separate empirical curves for the mic input
//! and the output (see wave-xlr.md).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::audio::PulseManager;
use crate::config::Config;

pub struct XlrDeps {
    pub config: Rc<RefCell<Config>>,
    pub manager: PulseManager,
    /// Called after a volume change was written into the config.
    pub on_changed: Rc<dyn Fn()>,
}

#[derive(Clone, Copy)]
enum LedCurve {
    /// Mic input: LED ≈ 0.321·dB + 24.6
    Source,
    /// Output: LED ≈ 0.416·dB + 25.0
    Sink,
}

fn percent_to_db(percent: f64) -> f64 {
    60.0 * (percent / 100.0).log10()
}

fn leds(curve: LedCurve, percent: f64) -> u32 {
    if percent <= 0.0 {
        return 0;
    }
    let db = percent_to_db(percent);
    let l = match curve {
        LedCurve::Source => 0.321 * db + 24.6,
        LedCurve::Sink => 0.416 * db + 25.0,
    };
    l.round().clamp(0.0, 25.0) as u32
}

fn detail(curve: LedCurve, percent: f64) -> String {
    if percent <= 0.0 {
        return "0% · silent · ring dark".to_string();
    }
    format!(
        "{:.0}% · {:.1} dB · {} of 25 LEDs",
        percent,
        percent_to_db(percent),
        leds(curve, percent)
    )
}

/// One volume row: title, live detail subtitle and a percent slider.
fn volume_row(
    title: &str,
    device: &str,
    initial: f64,
    curve: LedCurve,
    on_value: impl Fn(f64) + 'static,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(detail(curve, initial))
        .use_markup(false)
        .tooltip_text(device)
        .build();
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
    scale.set_draw_value(false);
    scale.set_width_request(200);
    scale.set_valign(gtk::Align::Center);
    scale.set_value(initial);
    {
        let row = row.clone();
        scale.connect_value_changed(move |s| {
            let v = s.value();
            row.set_subtitle(&detail(curve, v));
            on_value(v);
        });
    }
    row.add_suffix(&scale);
    row
}

pub fn open(parent: &impl IsA<gtk::Widget>, deps: XlrDeps) -> adw::Dialog {
    let deps = Rc::new(deps);
    let source = deps.manager.wave_xlr_source();
    let sink = deps.manager.wave_xlr_sink();

    let dialog = adw::Dialog::builder()
        .title("Wave XLR")
        .content_width(480)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());

    if source.is_none() && sink.is_none() {
        let page = adw::StatusPage::builder()
            .icon_name("audio-card-symbolic")
            .title("No Wave XLR Detected")
            .description("Connect an Elgato Wave XLR and open this dialog again.")
            .build();
        page.set_size_request(-1, 320);
        toolbar.set_content(Some(&page));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(parent));
        return dialog;
    }

    let group = adw::PreferencesGroup::builder()
        .title("Startup Volumes")
        .description(
            "Applied again every time Crossfade starts — the Wave XLR \
             occasionally resets its volume settings on its own.",
        )
        .build();

    if let Some(src) = source {
        let initial = deps
            .config
            .borrow()
            .wave_xlr
            .mic_volume
            .unwrap_or(src.volume)
            .clamp(0.0, 100.0);
        let deps = deps.clone();
        let name = src.name.clone();
        group.add(&volume_row(
            "Microphone Gain",
            &src.description,
            initial,
            LedCurve::Source,
            move |v| {
                deps.manager.set_source_volume(&name, v);
                deps.config.borrow_mut().wave_xlr.mic_volume = Some(v);
                (deps.on_changed)();
            },
        ));
    }

    if let Some(snk) = sink {
        let initial = deps
            .config
            .borrow()
            .wave_xlr
            .output_volume
            .unwrap_or(snk.volume)
            .clamp(0.0, 100.0);
        let deps = deps.clone();
        let name = snk.name.clone();
        group.add(&volume_row(
            "Headphone Volume",
            &snk.description,
            initial,
            LedCurve::Sink,
            move |v| {
                deps.manager.set_sink_volume(&name, v);
                deps.config.borrow_mut().wave_xlr.output_volume = Some(v);
                (deps.on_changed)();
            },
        ));
    }

    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(12);
    content.set_margin_bottom(24);
    content.set_margin_start(16);
    content.set_margin_end(16);
    content.append(&group);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&content)
        .build();
    toolbar.set_content(Some(&scroller));
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(parent));
    dialog
}
