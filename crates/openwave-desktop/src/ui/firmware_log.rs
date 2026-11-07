//! A device's own firmware log, for firmware that offers one. Reading is opt-in
//! and read-only: the switch only asks the runtime to drain what the firmware
//! already logs; nothing on the device is configured from here.
use crate::Submit;
use adw::prelude::*;
use openwave_core::model::FirmwareLogView;
use openwave_runtime::controller::AppCommand;
use std::{cell::Cell, rc::Rc};

/// Lines kept in the view; older ones are dropped from the top.
const VIEW_LINES: i32 = 20_000;

pub struct FirmwareLogWindow {
    pub window: adw::Window,
    switch: gtk::Switch,
    status: gtk::Label,
    buffer: gtk::TextBuffer,
    view: gtk::TextView,
    scroller: gtk::ScrolledWindow,
    /// Sequence number of the next line to show.
    shown: Cell<u64>,
    syncing: Cell<bool>,
}

impl FirmwareLogWindow {
    pub fn new(parent: &gtk::Window, submit: Submit) -> Rc<Self> {
        let window = adw::Window::builder()
            .title("Device Log")
            .transient_for(parent)
            .hide_on_close(true)
            .default_width(760)
            .default_height(520)
            .build();
        if let Some(application) = parent.application() {
            window.set_application(Some(&application));
        }
        let header = adw::HeaderBar::new();
        let switch = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .tooltip_text("Read the device's firmware log")
            .build();
        let label = gtk::Label::new(Some("Read log"));
        let toggle = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        toggle.append(&label);
        toggle.append(&switch);
        header.pack_start(&toggle);
        let copy = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text("Copy log")
            .build();
        let clear = gtk::Button::builder()
            .icon_name("edit-clear-all-symbolic")
            .tooltip_text("Clear view")
            .build();
        header.pack_end(&clear);
        header.pack_end(&copy);
        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        status.add_css_class("dim-label");
        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(12)
            .right_margin(12)
            .top_margin(6)
            .bottom_margin(6)
            .build();
        let buffer = view.buffer();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&header);
        content.append(&status);
        content.append(&scroller);
        window.set_content(Some(&content));

        let this = Rc::new(Self {
            window,
            switch,
            status,
            buffer,
            view,
            scroller,
            shown: Cell::new(0),
            syncing: Cell::new(false),
        });
        let weak = Rc::downgrade(&this);
        this.switch.connect_active_notify(move |switch| {
            if weak.upgrade().is_some_and(|this| !this.syncing.get()) {
                submit(AppCommand::SetFirmwareLog {
                    enabled: switch.is_active(),
                });
            }
        });
        let weak = Rc::downgrade(&this);
        copy.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                let (start, end) = this.buffer.bounds();
                this.window
                    .clipboard()
                    .set_text(&this.buffer.text(&start, &end, false));
            }
        });
        let weak = Rc::downgrade(&this);
        clear.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.buffer.set_text("");
            }
        });
        this
    }

    pub fn present(&self) {
        self.window.present();
    }

    /// Append lines the view has not shown yet; cheap when nothing is new.
    pub fn update(&self, log: &FirmwareLogView) {
        if self.switch.is_active() != log.enabled {
            self.syncing.set(true);
            self.switch.set_active(log.enabled);
            self.syncing.set(false);
        }
        if self.status.label() != log.status {
            self.status.set_label(&log.status);
        }
        self.status.set_visible(!log.status.is_empty());
        let next = log.next();
        let shown = self.shown.get();
        if shown >= next {
            return;
        }
        let adjustment = self.scroller.vadjustment();
        let following = adjustment.value() + adjustment.page_size() >= adjustment.upper() - 1.0;
        let mut text = String::new();
        if shown < log.first {
            text.push_str(&format!(
                "… {} earlier lines were not kept …\n",
                log.first - shown
            ));
        }
        let now = glib::DateTime::now_local()
            .ok()
            .and_then(|now| now.format("%H:%M:%S").ok())
            .map(|time| time.to_string())
            .unwrap_or_default();
        for line in log
            .lines
            .iter()
            .skip((shown.max(log.first) - log.first) as usize)
        {
            text.push_str(&now);
            text.push(' ');
            text.push_str(line);
            text.push('\n');
        }
        self.buffer.insert(&mut self.buffer.end_iter(), &text);
        self.shown.set(next);
        let excess = self.buffer.line_count() - VIEW_LINES;
        if excess > 0 {
            let mut start = self.buffer.start_iter();
            if let Some(mut end) = self.buffer.iter_at_line(excess) {
                self.buffer.delete(&mut start, &mut end);
            }
        }
        if following {
            let mark = self
                .buffer
                .create_mark(None, &self.buffer.end_iter(), false);
            self.view.scroll_mark_onscreen(&mark);
            self.buffer.delete_mark(&mark);
        }
    }
}
