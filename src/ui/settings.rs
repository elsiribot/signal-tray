//! The settings window. A regular toplevel rather than a layer surface, so
//! it can be moved and focused like any other window.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, gio};

use super::AccountView;
use crate::config::{Color, Config, Opacity};
use crate::model::AccountId;

/// A change made in the settings window, applied immediately.
#[derive(Clone, Debug)]
pub enum Setting {
    Opacity(Opacity),
    HideOnFocusLoss(bool),
    AddAccount,
    /// Confirmed by the user.
    RemoveAccount(AccountId),
    AccountName(AccountId, String),
    AccountColor(AccountId, Color),
}

type OnChange = Rc<dyn Fn(Setting)>;

struct AccountRow {
    id: AccountId,
    widget: gtk::Box,
    name: gtk::Entry,
}

pub struct Settings {
    window: gtk::Window,
    accounts_box: gtk::Box,
    rows: RefCell<Vec<AccountRow>>,
    on_change: OnChange,
}

impl Settings {
    /// Builds the window, showing `config`'s current values. `on_change`
    /// runs for every change, including each step while dragging the
    /// opacity slider so the main window previews it live. Closing only
    /// hides the window.
    pub fn new(
        app: &gtk::Application,
        config: &Config,
        accounts: &[AccountView],
        on_change: impl Fn(Setting) + 'static,
    ) -> Self {
        let window = gtk::Window::builder()
            .application(app)
            .title("Signal Tray Settings")
            .default_width(320)
            .resizable(false)
            .hide_on_close(true)
            .build();
        window.add_css_class("signal-tray-settings");
        let on_change: OnChange = Rc::new(on_change);

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
        hide_on_focus_loss.connect_active_notify({
            let on_change = on_change.clone();
            move |switch| on_change(Setting::HideOnFocusLoss(switch.is_active()))
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

        let accounts_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let add = gtk::Button::builder()
            .label("Add account…")
            .halign(gtk::Align::Start)
            .build();
        add.connect_clicked({
            let on_change = on_change.clone();
            move |_| on_change(Setting::AddAccount)
        });

        let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
        content.add_css_class("page");
        content.append(&heading("Accounts"));
        content.append(&accounts_box);
        content.append(&add);
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

        let settings = Self {
            window,
            accounts_box,
            rows: RefCell::default(),
            on_change,
        };
        settings.set_accounts(accounts);
        settings
    }

    pub fn window(&self) -> &gtk::Window {
        &self.window
    }

    /// Adds and removes account rows, and refreshes names that aren't
    /// being edited. Colours only ever change through their own button.
    pub fn set_accounts(&self, accounts: &[AccountView]) {
        let removed: Vec<AccountRow> = {
            let mut rows = self.rows.borrow_mut();
            let (keep, removed) = std::mem::take(&mut *rows)
                .into_iter()
                .partition(|row| accounts.iter().any(|a| a.id == row.id));
            *rows = keep;
            removed
        };
        // Outside the borrow: a focused name entry commits when removed,
        // which comes back here.
        for row in removed {
            self.accounts_box.remove(&row.widget);
        }
        let mut rows = self.rows.borrow_mut();
        for account in accounts {
            match rows.iter().find(|row| row.id == account.id) {
                Some(row) => {
                    if !row.name.has_focus() && row.name.text() != account.name {
                        row.name.set_text(&account.name);
                    }
                }
                None => {
                    let row = self.account_row(account);
                    self.accounts_box.append(&row.widget);
                    rows.push(row);
                }
            }
        }
    }

    fn account_row(&self, account: &AccountView) -> AccountRow {
        let dialog = gtk::ColorDialog::builder()
            .title("Account colour")
            .with_alpha(false)
            .build();
        let color = gtk::ColorDialogButton::builder()
            .dialog(&dialog)
            .rgba(&to_rgba(account.color))
            .valign(gtk::Align::Center)
            .tooltip_text("Colour of the account's chats and messages")
            .build();
        color.connect_rgba_notify({
            let on_change = self.on_change.clone();
            let id = account.id.clone();
            move |button| on_change(Setting::AccountColor(id.clone(), from_rgba(&button.rgba())))
        });

        let name = gtk::Entry::builder()
            .text(&account.name)
            .hexpand(true)
            .tooltip_text("Shown next to the chat title")
            .build();
        let commit_name = {
            let on_change = self.on_change.clone();
            let id = account.id.clone();
            move |entry: &gtk::Entry| {
                on_change(Setting::AccountName(id.clone(), entry.text().into()))
            }
        };
        name.connect_activate(commit_name.clone());
        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave({
            let name = name.downgrade();
            move |_| {
                if let Some(name) = name.upgrade() {
                    commit_name(&name);
                }
            }
        });
        name.add_controller(focus);

        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove account…")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        remove.connect_clicked({
            let on_change = self.on_change.clone();
            let id = account.id.clone();
            let window = self.window.downgrade();
            let name = name.downgrade();
            move |_| {
                let label = name
                    .upgrade()
                    .map(|n| n.text().to_string())
                    .unwrap_or_default();
                confirm_removal(window.upgrade().as_ref(), &label, {
                    let on_change = on_change.clone();
                    let id = id.clone();
                    move || on_change(Setting::RemoveAccount(id))
                });
            }
        });

        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        widget.add_css_class("setting-row");
        widget.append(&color);
        widget.append(&name);
        widget.append(&remove);
        AccountRow {
            id: account.id.clone(),
            widget,
            name,
        }
    }
}

fn confirm_removal(parent: Option<&gtk::Window>, name: &str, remove: impl FnOnce() + 'static) {
    const REMOVE: i32 = 1;
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(format!("Remove {name}?"))
        .detail(
            "Its messages on this computer are deleted. Also remove this \
             computer under Settings → Linked devices on your phone.",
        )
        .buttons(["Cancel", "Remove"])
        .cancel_button(0)
        .default_button(0)
        .build();
    dialog.choose(parent, None::<&gio::Cancellable>, move |choice| {
        if choice == Ok(REMOVE) {
            remove();
        }
    });
}

fn to_rgba(color: Color) -> gdk::RGBA {
    gdk::RGBA::new(
        f32::from(color.r) / 255.0,
        f32::from(color.g) / 255.0,
        f32::from(color.b) / 255.0,
        1.0,
    )
}

fn from_rgba(rgba: &gdk::RGBA) -> Color {
    // Clamped, so the cast can't truncate.
    let channel = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color::rgb(
        channel(rgba.red()),
        channel(rgba.green()),
        channel(rgba.blue()),
    )
}

fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(["heading"])
        .build()
}
