use adw::prelude::*;

pub(crate) mod device_slider;
pub(crate) mod editor;
pub(crate) mod fader;
pub(crate) mod fx;
pub(crate) mod scale;

/// Let the mouse wheel scroll the surrounding pane instead of moving the
/// slider. GtkRange handles wheel events with its own scroll controller,
/// which swallows them; with it switched off the event reaches the enclosing
/// GtkScrolledWindow, which keeps its smooth and kinetic scrolling. Dragging,
/// clicking and the keyboard still move the slider.
pub(crate) fn pass_wheel_to_pane(scale: &gtk::Scale) {
    let controllers = scale.observe_controllers();
    for index in 0..controllers.n_items() {
        if let Some(scroll) = controllers
            .item(index)
            .and_downcast::<gtk::EventControllerScroll>()
        {
            scroll.set_propagation_phase(gtk::PropagationPhase::None);
        }
    }
}

pub(crate) fn value_row(title: &str, width: i32) -> (adw::ActionRow, gtk::Label) {
    let row = adw::ActionRow::builder().title(title).build();
    let label = gtk::Label::builder()
        .label("—")
        .width_chars(width)
        .xalign(1.0)
        .build();
    label.add_css_class("monospace");
    row.add_suffix(&label);
    (row, label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wheel_handlers(scale: &gtk::Scale) -> usize {
        let controllers = scale.observe_controllers();
        (0..controllers.n_items())
            .filter_map(|index| {
                controllers
                    .item(index)
                    .and_downcast::<gtk::EventControllerScroll>()
            })
            .filter(|scroll| scroll.propagation_phase() != gtk::PropagationPhase::None)
            .count()
    }

    #[test]
    #[ignore = "requires the isolated installed GTK test runner"]
    fn wheel_over_a_device_slider_is_left_to_the_pane() {
        adw::init().expect("private GTK display");
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.1);
        assert!(
            wheel_handlers(&scale) > 0,
            "GtkRange handles the wheel itself"
        );
        pass_wheel_to_pane(&scale);
        assert_eq!(wheel_handlers(&scale), 0);
        // Other input still reaches the slider.
        let controllers = scale.observe_controllers();
        assert!((0..controllers.n_items()).any(|index| {
            controllers
                .item(index)
                .and_downcast::<gtk::EventController>()
                .is_some_and(|c| c.propagation_phase() != gtk::PropagationPhase::None)
        }));
    }
}
