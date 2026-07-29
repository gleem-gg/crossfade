use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;

use crate::config::MasterConfig;

use super::{label_factory, meter_pair, mute_button, MeterPair};

/// The OUTPUTS section: one row for the monitor mix (with hardware device
/// selector), one for the stream mix, and — while the VOD mix is enabled —
/// one for the VOD mix, each with level meter, master volume and master mute.
pub struct OutputsPanel {
    pub root: gtk::ListBox,
    pub monitor_device: gtk::DropDown,
    pub monitor_level: MeterPair,
    pub monitor_scale: gtk::Scale,
    pub monitor_mute: gtk::ToggleButton,
    pub stream_level: MeterPair,
    pub stream_scale: gtk::Scale,
    pub stream_mute: gtk::ToggleButton,
    pub vod_level: MeterPair,
    pub vod_scale: gtk::Scale,
    pub vod_mute: gtk::ToggleButton,
    vod_row: gtk::ListBoxRow,
    pub guard: Rc<Cell<bool>>,
    /// Sink name behind each device drop-down position; `None` = system default.
    pub sink_entries: RefCell<Vec<Option<String>>>,
    last_labels: RefCell<Vec<String>>,
    /// Keep both rows' columns equally wide.
    _middle_size_group: gtk::SizeGroup,
    _titles_size_group: gtk::SizeGroup,
}

fn master_scale(tooltip: &str) -> gtk::Scale {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
    scale.set_draw_value(false);
    scale.set_hexpand(true);
    scale.set_tooltip_text(Some(tooltip));
    scale
}

#[allow(clippy::too_many_arguments)]
fn output_row(
    icon: &str,
    title: &str,
    subtitle: &str,
    middle: &gtk::Widget,
    level: &gtk::Box,
    scale: &gtk::Scale,
    mute: &gtk::ToggleButton,
    titles_group: &gtk::SizeGroup,
) -> gtk::ListBoxRow {
    let hbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    let image = gtk::Image::from_icon_name(icon);
    image.add_css_class("dim-label");
    hbox.append(&image);

    let titles = gtk::Box::new(gtk::Orientation::Vertical, 2);
    titles.set_width_request(190);
    titles.set_valign(gtk::Align::Center);
    let title_label = gtk::Label::builder().label(title).xalign(0.0).build();
    title_label.add_css_class("heading");
    let subtitle_label = gtk::Label::builder()
        .label(subtitle)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(30)
        .tooltip_text(subtitle)
        .build();
    subtitle_label.add_css_class("caption");
    subtitle_label.add_css_class("dim-label");
    titles.append(&title_label);
    titles.append(&subtitle_label);
    titles_group.add_widget(&titles);
    hbox.append(&titles);

    middle.set_size_request(230, -1);
    hbox.append(middle);

    let meters = gtk::Box::new(gtk::Orientation::Vertical, 6);
    meters.set_hexpand(true);
    meters.set_valign(gtk::Align::Center);
    meters.append(level);
    meters.append(scale);
    hbox.append(&meters);

    hbox.append(mute);

    gtk::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .child(&hbox)
        .build()
}

impl OutputsPanel {
    pub fn new() -> Self {
        let root = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();

        let monitor_device = gtk::DropDown::builder()
            .valign(gtk::Align::Center)
            .tooltip_text("Device the monitor mix is played on")
            .build();
        monitor_device.set_factory(Some(&label_factory(18, true)));
        monitor_device.set_list_factory(Some(&label_factory(44, false)));

        let titles_group = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);

        let monitor_level = meter_pair();
        let monitor_scale = master_scale("Monitor mix master volume");
        let monitor_mute = mute_button("audio-headphones-symbolic", "Mute the monitor mix");
        let monitor_row = output_row(
            "audio-headphones-symbolic",
            "Monitor Mix",
            "What you hear locally",
            monitor_device.upcast_ref(),
            &monitor_level.root,
            &monitor_scale,
            &monitor_mute,
            &titles_group,
        );

        let stream_level = meter_pair();
        let stream_scale = master_scale("Stream mix master volume");
        let stream_mute = mute_button("media-record-symbolic", "Mute the stream mix");
        let stream_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let stream_row = output_row(
            "audio-input-microphone-symbolic",
            "Stream Mix",
            "Select “Crossfade Stream Mix” as microphone in OBS, Discord, etc.",
            stream_spacer.upcast_ref(),
            &stream_level.root,
            &stream_scale,
            &stream_mute,
            &titles_group,
        );

        let vod_level = meter_pair();
        let vod_scale = master_scale("VOD mix master volume");
        let vod_mute = mute_button("camera-video-symbolic", "Mute the VOD mix");
        let vod_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let vod_row = output_row(
            "camera-video-symbolic",
            "VOD Mix",
            "Select “Crossfade VOD Mix” as a second audio track for recordings",
            vod_spacer.upcast_ref(),
            &vod_level.root,
            &vod_scale,
            &vod_mute,
            &titles_group,
        );
        vod_row.set_visible(false);

        // Keep the middle column of all rows equally wide so the level
        // meters and master sliders line up exactly. The titles group does
        // the same for the title/subtitle column.
        let middle_group = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
        middle_group.add_widget(&monitor_device);
        middle_group.add_widget(&stream_spacer);
        middle_group.add_widget(&vod_spacer);

        root.append(&monitor_row);
        root.append(&stream_row);
        root.append(&vod_row);

        Self {
            root,
            monitor_device,
            monitor_level,
            monitor_scale,
            monitor_mute,
            stream_level,
            stream_scale,
            stream_mute,
            vod_level,
            vod_scale,
            vod_mute,
            vod_row,
            guard: Rc::new(Cell::new(false)),
            sink_entries: RefCell::new(Vec::new()),
            last_labels: RefCell::new(Vec::new()),
            _middle_size_group: middle_group,
            _titles_size_group: titles_group,
        }
    }

    pub fn load_config(&self, m: &MasterConfig) {
        self.guard.set(true);
        self.monitor_scale.set_value(m.monitor_volume);
        self.stream_scale.set_value(m.stream_volume);
        self.vod_scale.set_value(m.vod_volume);
        self.monitor_mute.set_active(m.monitor_muted);
        self.stream_mute.set_active(m.stream_muted);
        self.vod_mute.set_active(m.vod_muted);
        self.guard.set(false);
    }

    pub fn set_vod_visible(&self, visible: bool) {
        self.vod_row.set_visible(visible);
    }

    /// Rebuild the monitor output device selector.
    pub fn set_output_sinks(
        &self,
        items: &[(String, Option<String>)],
        current: &Option<String>,
    ) {
        let labels: Vec<String> = items.iter().map(|(l, _)| l.clone()).collect();
        let entries: Vec<Option<String>> = items.iter().map(|(_, n)| n.clone()).collect();
        let selected = entries.iter().position(|e| e == current).unwrap_or(0) as u32;
        if *self.last_labels.borrow() == labels && self.monitor_device.selected() == selected {
            return;
        }
        self.guard.set(true);
        let strs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let model = gtk::StringList::new(&strs);
        self.monitor_device.set_model(Some(&model));
        self.monitor_device.set_selected(selected);
        self.guard.set(false);
        *self.last_labels.borrow_mut() = labels;
        *self.sink_entries.borrow_mut() = entries;
    }
}
