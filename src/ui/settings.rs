//! The settings window. A regular toplevel rather than a layer surface, so
//! it can be moved and focused like any other window.

use gtk::prelude::*;

use crate::config::{Config, Opacity};

/// A change made in the settings window, applied immediately.
#[derive(Clone, Copy, Debug)]
pub enum Setting {
    Opacity(Opacity),
    HideOnFocusLoss(bool),
}

/// Builds the window, showing `config`'s current values. `on_change` runs
/// for every change, including each step while dragging the opacity slider
/// so the main window previews it live. Closing only hides the window.
pub fn window(
    app: &gtk::Application,
    config: &Config,
    on_change: impl Fn(Setting) + 'static,
) -> gtk::Window {
    let window = gtk::Window::builder()
        .application(app)
        .title("Signal Tray Settings")
        .default_width(320)
        .resizable(false)
        .hide_on_close(true)
        .build();
    window.add_css_class("signal-tray-settings");
    let on_change = std::rc::Rc::new(on_change);

    let percent = |opacity: Opacity| (opacity.get() * 100.0).round();
    let scale = gtk::Scale::with_range(
        gtk::Orientation::Horizontal,
        percent(Opacity::MIN),
        percent(Opacity::MAX),
        5.0,
    );
    scale.set_value(percent(config.opacity));
    scale.set_round_digits(0);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_hexpand(true);
    scale.set_format_value_func(|_, value| format!("{value:.0} %"));
    scale.connect_value_changed({
        let on_change = on_change.clone();
        move |scale| match Opacity::try_from(scale.value() / 100.0) {
            Ok(opacity) => on_change(Setting::Opacity(opacity)),
            Err(err) => tracing::error!(%err, "opacity slider left its range"),
        }
    });

    let hide_on_focus_loss = gtk::Switch::builder()
        .active(config.hide_on_focus_loss)
        .valign(gtk::Align::Center)
        .build();
    hide_on_focus_loss.connect_active_notify(move |switch| {
        on_change(Setting::HideOnFocusLoss(switch.is_active()));
    });
    let hide_label = gtk::Label::builder()
        .label("Hide when another window gets focus")
        .xalign(0.0)
        .wrap(true)
        .hexpand(true)
        .mnemonic_widget(&hide_on_focus_loss)
        .build();
    let hide_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    hide_row.add_css_class("setting-row");
    hide_row.append(&hide_label);
    hide_row.append(&hide_on_focus_loss);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
    content.add_css_class("page");
    content.append(&heading("Background opacity"));
    content.append(&scale);
    content.append(
        &gtk::Label::builder()
            .label("Text and message bubbles always stay opaque.")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim"])
            .build(),
    );
    content.append(&heading("Behaviour"));
    content.append(&hide_row);
    window.set_child(Some(&content));

    let shortcuts = gtk::ShortcutController::new();
    shortcuts.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("Escape"),
        Some(gtk::NamedAction::new("window.close")),
    ));
    window.add_controller(shortcuts);
    window
}

fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(["heading"])
        .build()
}
