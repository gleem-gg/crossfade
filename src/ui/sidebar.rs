use std::cell::RefCell;

use adw::prelude::*;
use gtk::glib;

use crate::config::{Assignment, Config};

/// Sidebar summarizing the virtual devices and what feeds each channel.
pub struct Sidebar {
    pub root: gtk::ScrolledWindow,
    monitor_row: adw::ActionRow,
    stream_row: adw::ActionRow,
    vod_row: adw::ActionRow,
    channels_group: adw::PreferencesGroup,
    channel_rows: RefCell<Vec<adw::ActionRow>>,
}

impl Sidebar {
    pub fn new() -> Self {
        let outputs_group = adw::PreferencesGroup::builder()
            .title("Virtual Outputs")
            .build();

        let monitor_row = adw::ActionRow::builder()
            .title("Monitor Mix")
            .subtitle("Not routed")
            .build();
        monitor_row.add_prefix(&gtk::Image::from_icon_name("audio-headphones-symbolic"));
        outputs_group.add(&monitor_row);

        let stream_row = adw::ActionRow::builder()
            .title("Stream Mix")
            .subtitle("Available as the “Crossfade Stream Mix” microphone in OBS, Discord, etc.")
            .build();
        stream_row.add_prefix(&gtk::Image::from_icon_name("audio-input-microphone-symbolic"));
        outputs_group.add(&stream_row);

        let vod_row = adw::ActionRow::builder()
            .title("VOD Mix")
            .subtitle(
                "Available as the “Crossfade VOD Mix” microphone — use it as a \
                 second audio track to keep music out of recordings",
            )
            .visible(false)
            .build();
        vod_row.add_prefix(&gtk::Image::from_icon_name("camera-video-symbolic"));
        outputs_group.add(&vod_row);

        let channels_group = adw::PreferencesGroup::builder()
            .title("Channel Assignments")
            .build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&outputs_group);
        content.append(&channels_group);

        let root = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&content)
            .build();

        Self {
            root,
            monitor_row,
            stream_row,
            vod_row,
            channels_group,
            channel_rows: RefCell::new(Vec::new()),
        }
    }

    pub fn update(
        &self,
        config: &Config,
        monitor_device_label: &str,
        describe: &dyn Fn(&Assignment) -> String,
    ) {
        self.monitor_row
            .set_subtitle(&format!("Routed to {monitor_device_label}"));
        let _ = &self.stream_row;
        self.vod_row.set_visible(config.vod_mix_enabled);

        let mut rows = self.channel_rows.borrow_mut();
        while rows.len() > config.channels.len() {
            if let Some(row) = rows.pop() {
                self.channels_group.remove(&row);
            }
        }
        while rows.len() < config.channels.len() {
            let row = adw::ActionRow::new();
            self.channels_group.add(&row);
            rows.push(row);
        }
        for (row, ch) in rows.iter().zip(&config.channels) {
            row.set_title(&glib::markup_escape_text(&ch.name));
            let subtitle = match &ch.assignment {
                None => "No input".to_string(),
                Some(Assignment::Virtual) => {
                    format!("Virtual device — “Crossfade: {}”", ch.name)
                }
                Some(Assignment::CatchAll) => {
                    "Every app not assigned to another channel".to_string()
                }
                Some(a) => describe(a),
            };
            row.set_subtitle(&glib::markup_escape_text(&subtitle));
        }
    }
}
