//! The window: a stack of pages (status, link, chat list, chat, help) with
//! keyboard-first navigation. The UI holds no Signal logic; it renders what
//! the controller gives it and reports user intent as [`UiEvent`]s.

mod format;
mod placement;
pub mod settings;

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell as _};

use crate::config::{Color, Config, Opacity};
use crate::model::{AccountId, ChatRef, DeliveryStatus, Timestamp};
use crate::store::ConversationSummary;
pub use placement::Placement;

const CSS: &str = include_str!("style.css");

/// How long error banners stay up.
const BANNER_SECONDS: u32 = 8;
/// Rows moved by PageUp/PageDown in the chat list.
const LIST_PAGE_ROWS: i32 = 8;
/// Upper bound of rows rendered while searching, to keep typing snappy
/// with large address books.
const MAX_SEARCH_RESULTS: usize = 100;
/// Tolerance for "scrolled to the very bottom", in pixels.
const BOTTOM_SLACK: f64 = 8.0;
/// Focus moving between the app's own windows arrives as a loss and a gain,
/// so focus-loss hiding waits this long for the gain.
const FOCUS_SETTLE: std::time::Duration = std::time::Duration::from_millis(150);

pub enum UiEvent {
    Open(ChatRef),
    /// The open conversation is on screen and focused, so its messages
    /// count as read.
    Seen(ChatRef),
    Send {
        chat: ChatRef,
        text: String,
    },
    /// The window was brought up.
    Shown,
    RetryLink,
    /// Escape on the link page.
    CancelLink,
    OpenSettings,
    /// The window was dragged to a new spot.
    Moved(Placement),
    Quit,
}

impl UiEvent {
    /// For logging without leaking message contents.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Open(_) => "open",
            Self::Seen(_) => "seen",
            Self::Send { .. } => "send",
            Self::Shown => "shown",
            Self::RetryLink => "retry-link",
            Self::CancelLink => "cancel-link",
            Self::OpenSettings => "open-settings",
            Self::Moved(_) => "moved",
            Self::Quit => "quit",
        }
    }
}

/// How an account is presented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountView {
    pub id: AccountId,
    pub name: String,
    pub color: Color,
}

/// A row of the unified chat list.
#[derive(Clone, Debug)]
pub struct ListedChat {
    pub account: AccountId,
    pub summary: ConversationSummary,
}

impl ListedChat {
    fn chat(&self) -> ChatRef {
        ChatRef {
            account: self.account.clone(),
            conversation: self.summary.id.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageStatus {
    Incoming,
    Pending,
    Delivery(DeliveryStatus),
}

#[derive(Clone, Debug)]
pub struct MessageView {
    pub status: MessageStatus,
    /// Shown above incoming group messages.
    pub sender: Option<String>,
    pub body: String,
    pub timestamp: Timestamp,
    pub edited: bool,
    pub deleted: bool,
}

/// What the status page is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Something is in progress; shows a spinner.
    Busy,
    /// A dead end that needs the user's attention.
    Failed,
}

#[derive(Clone, Copy, Debug)]
pub enum LinkState {
    Waiting,
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Status,
    Link,
    List,
    Chat,
    Help,
}

impl Page {
    fn name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Link => "link",
            Self::List => "list",
            Self::Chat => "chat",
            Self::Help => "help",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        [Self::Status, Self::Link, Self::List, Self::Chat, Self::Help]
            .into_iter()
            .find(|p| p.name() == name)
    }
}

#[derive(Default)]
struct State {
    accounts: Vec<AccountView>,
    conversations: Vec<ListedChat>,
    /// Chat of each row in the chat list, in display order.
    visible: Vec<ChatRef>,
    current: Option<ChatRef>,
    /// Where F1/Escape returns to from the help page.
    page_before_help: Option<Page>,
    link_expired: bool,
    /// Keep the message list pinned to the newest message.
    stick_to_bottom: bool,
    /// Scroll offset to restore once a re-rendered chat has been laid out.
    restore_scroll: Option<f64>,
    /// Search text the chat list was last built for.
    listed_filter: String,
}

pub struct Ui {
    window: gtk::ApplicationWindow,
    /// Holds the one rule that depends on the configured opacity.
    opacity_css: gtk::CssProvider,
    /// Holds the rules for account colours.
    account_css: gtk::CssProvider,
    uses_layer_shell: bool,
    hide_on_focus_loss: Cell<bool>,
    stack: gtk::Stack,
    banner: gtk::Label,
    banner_revealer: gtk::Revealer,
    status_label: gtk::Label,
    status_spinner: gtk::Spinner,
    link_title: gtk::Label,
    link_picture: gtk::Picture,
    link_hint: gtk::Label,
    search: gtk::SearchEntry,
    list_box: gtk::ListBox,
    list_scroll: gtk::ScrolledWindow,
    list_empty: gtk::Label,
    chat_header: gtk::Box,
    chat_title: gtk::Label,
    chat_account: gtk::Label,
    messages: gtk::ListBox,
    messages_scroll: gtk::ScrolledWindow,
    compose: gtk::TextView,
    events: async_channel::Sender<UiEvent>,
    state: RefCell<State>,
    /// Bumped per banner so only the latest one's timeout hides it.
    banner_generation: Rc<Cell<u64>>,
}

impl Ui {
    pub fn new(
        app: &gtk::Application,
        config: &Config,
        events: async_channel::Sender<UiEvent>,
    ) -> Rc<Self> {
        load_css();
        let opacity_css = gtk::CssProvider::new();
        add_css_provider(&opacity_css);
        let account_css = gtk::CssProvider::new();
        add_css_provider(&account_css);

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Signal")
            .default_width(config.width)
            .default_height(config.height)
            .build();
        // Added rather than set: the default classes carry the background.
        window.add_css_class("signal-tray");
        let uses_layer_shell = gtk4_layer_shell::is_supported();
        if uses_layer_shell {
            setup_layer_shell(&window, config, events.clone());
        } else {
            tracing::info!("layer-shell unsupported, using a regular window");
        }

        let banner = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .css_classes(["banner"])
            .build();
        let banner_revealer = gtk::Revealer::builder().child(&banner).build();

        // Status page.
        let status_label = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim"])
            .build();
        let status_page = vbox(8);
        status_page.set_valign(gtk::Align::Center);
        status_page.add_css_class("page");
        let status_spinner = gtk::Spinner::new();
        status_page.append(&status_spinner);
        status_page.append(&status_label);

        // Link page.
        let link_picture = gtk::Picture::builder()
            .can_shrink(false)
            .halign(gtk::Align::Center)
            .css_classes(["qr"])
            .build();
        let link_hint = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim"])
            .build();
        let link_page = vbox(8);
        link_page.set_valign(gtk::Align::Center);
        link_page.add_css_class("page");
        let link_title = gtk::Label::builder().css_classes(["title"]).build();
        link_page.append(&link_title);
        link_page.append(&link_picture);
        link_page.append(
            &gtk::Label::builder()
                .label("Signal → Settings → Linked devices → Link new device")
                .wrap(true)
                .justify(gtk::Justification::Center)
                .build(),
        );
        link_page.append(&link_hint);

        // Chat list page.
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search")
            // Filter on every keystroke so Enter never acts on stale results.
            .search_delay(0)
            .css_classes(["search"])
            .build();
        let list_box = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Browse)
            .activate_on_single_click(true)
            .css_classes(["chat-list"])
            .build();
        let list_empty = gtk::Label::builder()
            .label("No chats yet. Type to search contacts.")
            .wrap(true)
            .vexpand(true)
            .css_classes(["dim"])
            .build();
        let list_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list_box)
            .build();
        let list_page = vbox(0);
        list_page.append(&search);
        list_page.append(&list_scroll);
        list_page.append(&list_empty);

        // Chat page.
        let back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Back (Esc)")
            .focusable(false)
            .css_classes(["flat", "back"])
            .build();
        let chat_title = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .css_classes(["chat-title"])
            .build();
        let chat_account = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .css_classes(["chat-account", "dim"])
            .build();
        let header = gtk::Box::builder().css_classes(["chat-header"]).build();
        header.append(&back);
        header.append(&chat_title);
        header.append(&chat_account);

        let messages = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["messages"])
            .build();
        let messages_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&messages)
            .build();

        let compose = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .css_classes(["compose"])
            .build();
        // Pinned to the first line, where the cursor is, rather than
        // centred: the box grows with the text.
        let compose_placeholder = gtk::Label::builder()
            .label("Message")
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .can_target(false)
            .css_classes(["placeholder"])
            .build();
        let compose_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            // A scrollbar's minimum length would make the box two lines tall
            // even when empty; the wheel and the cursor still scroll it.
            .vscrollbar_policy(gtk::PolicyType::External)
            .propagate_natural_height(true)
            .max_content_height(96)
            .child(&compose)
            .build();
        let compose_overlay = gtk::Overlay::builder()
            .child(&compose_scroll)
            .css_classes(["compose-box"])
            .build();
        compose_overlay.add_overlay(&compose_placeholder);
        compose.buffer().connect_changed(glib::clone!(
            #[weak]
            compose_placeholder,
            move |buffer| compose_placeholder.set_visible(buffer.char_count() == 0)
        ));

        let chat_page = vbox(0);
        chat_page.append(&header);
        chat_page.append(&messages_scroll);
        chat_page.append(&compose_overlay);

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(80)
            .vexpand(true)
            .build();
        stack.add_named(&status_page, Some(Page::Status.name()));
        stack.add_named(&link_page, Some(Page::Link.name()));
        stack.add_named(&list_page, Some(Page::List.name()));
        stack.add_named(&chat_page, Some(Page::Chat.name()));
        stack.add_named(&help_page(), Some(Page::Help.name()));

        let root = vbox(0);
        root.append(&banner_revealer);
        root.append(&stack);
        window.set_child(Some(&root));

        let ui = Rc::new(Self {
            window,
            opacity_css,
            account_css,
            uses_layer_shell,
            hide_on_focus_loss: Cell::new(config.hide_on_focus_loss),
            stack,
            banner,
            banner_revealer,
            status_label,
            status_spinner,
            link_title,
            link_picture,
            link_hint,
            search,
            list_box,
            list_scroll,
            list_empty,
            chat_header: header,
            chat_title,
            chat_account,
            messages,
            messages_scroll,
            compose,
            events,
            state: RefCell::default(),
            banner_generation: Rc::default(),
        });
        ui.connect_signals(&back);
        ui.set_opacity(config.opacity);
        ui.show_status(Status::Busy, "Starting…");
        ui
    }

    fn connect_signals(self: &Rc<Self>, back: &gtk::Button) {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, mods| ui.on_key(key, mods)
        ));
        self.window.add_controller(keys);

        // Never destroy the window: closing it (e.g. via the compositor)
        // just minimizes to the tray.
        self.window.connect_close_request(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            #[upgrade_or]
            glib::Propagation::Stop,
            move |_| {
                ui.hide();
                glib::Propagation::Stop
            }
        ));

        self.window.connect_is_active_notify(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |window| {
                tracing::debug!(active = window.is_active(), "main window focus changed");
                if window.is_active() {
                    ui.report_seen();
                } else {
                    ui.hide_unless_refocused();
                }
            }
        ));

        self.search.connect_search_changed(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |_| ui.rebuild_list()
        ));

        self.list_box.connect_row_activated(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |_, row| ui.open_row(row.index())
        ));

        back.connect_clicked(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |_| ui.show_list()
        ));

        let adjustment = self.messages_scroll.vadjustment();
        adjustment.connect_upper_notify(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |adj| {
                let (stick, restore) = {
                    let mut state = ui.state.borrow_mut();
                    (state.stick_to_bottom, state.restore_scroll.take())
                };
                if stick {
                    adj.set_value(adj.upper() - adj.page_size());
                } else if let Some(value) = restore {
                    adj.set_value(value);
                }
            }
        ));
        adjustment.connect_value_changed(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |adj| {
                let at_bottom = adj.value() + adj.page_size() >= adj.upper() - BOTTOM_SLACK;
                ui.state.borrow_mut().stick_to_bottom = at_bottom;
            }
        ));
    }

    fn emit(&self, event: UiEvent) {
        // The receiver lives as long as the app; failure means shutdown.
        let _ = self.events.try_send(event);
    }

    // ---- window visibility -------------------------------------------------

    /// Hides if visible and focused, otherwise shows and focuses.
    pub fn toggle(&self) {
        if self.window.is_visible() && self.window.is_active() {
            self.hide();
        } else {
            self.show();
        }
    }

    pub fn show(&self) {
        if self.uses_layer_shell && self.window.is_visible() && !self.window.is_active() {
            // Layer surfaces only get keyboard focus when mapped, so remap.
            self.window.set_visible(false);
        }
        if self.page() == Some(Page::List) {
            // Popping up is usually about the newest message.
            self.select_first();
        }
        self.window.present();
        self.focus_page();
        self.emit(UiEvent::Shown);
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    pub fn set_hide_on_focus_loss(&self, enabled: bool) {
        self.hide_on_focus_loss.set(enabled);
    }

    /// Keeps the main window up while `window` (e.g. settings) is open, and
    /// applies focus-loss hiding once it loses focus or closes, so closing
    /// it doesn't leave the main window behind.
    pub fn watch_focus(self: &Rc<Self>, window: &gtk::Window) {
        let check = glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |window: &gtk::Window| {
                if !window.is_active() || !window.is_visible() {
                    ui.hide_unless_refocused();
                }
            }
        );
        window.connect_is_active_notify(check.clone());
        window.connect_visible_notify(check);
    }

    /// Hides once focus has settled outside the app, when hiding on focus
    /// loss is enabled. Another of the app's windows being open counts as
    /// inside: it may not have received focus yet.
    fn hide_unless_refocused(self: &Rc<Self>) {
        if !self.hide_on_focus_loss.get() {
            return;
        }
        glib::timeout_add_local_once(
            FOCUS_SETTLE,
            glib::clone!(
                #[weak(rename_to = ui)]
                self,
                move || {
                    let main: &gtk::Window = ui.window.upcast_ref();
                    let in_app = ui.window.application().is_some_and(|app| {
                        app.windows()
                            .iter()
                            .any(|w| w.is_active() || (w != main && w.is_visible()))
                    });
                    if ui.hide_on_focus_loss.get() && ui.window.is_visible() && !in_app {
                        ui.hide();
                    }
                }
            ),
        );
    }

    /// Only the background is translucent; see [`Opacity`].
    pub fn set_opacity(&self, opacity: Opacity) {
        // `@theme_bg_color` rather than `@window_bg_color` or
        // `--window-bg-color`: GTK's built-in theme resolves only the former,
        // and an unresolved colour silently renders fully transparent.
        self.opacity_css.load_from_string(&format!(
            "window.signal-tray {{ background-color: alpha(@theme_bg_color, {:.2}); }}",
            opacity.get()
        ));
    }

    // ---- pages -------------------------------------------------------------

    fn page(&self) -> Option<Page> {
        self.stack
            .visible_child_name()
            .and_then(|n| Page::from_name(&n))
    }

    fn set_page(&self, page: Page) {
        self.stack.set_visible_child_name(page.name());
        self.focus_page();
    }

    fn focus_page(&self) {
        match self.page() {
            Some(Page::List) => {
                self.search.grab_focus();
            }
            Some(Page::Chat) => {
                self.compose.grab_focus();
            }
            Some(Page::Status | Page::Link | Page::Help) | None => {}
        }
    }

    pub fn show_status(&self, status: Status, text: &str) {
        self.status_label.set_label(text);
        let busy = status == Status::Busy;
        self.status_spinner.set_visible(busy);
        self.status_spinner.set_spinning(busy);
        self.set_page(Page::Status);
    }

    /// Shows the QR code the phone scans to link an account.
    pub fn show_link(&self, uri: &str, title: &str) {
        self.link_title.set_label(title);
        match qr_texture(uri) {
            Ok(texture) => self.link_picture.set_paintable(Some(&texture)),
            Err(err) => {
                tracing::error!(%err, "rendering link QR code failed");
                self.link_picture.set_paintable(None::<&gdk::Paintable>);
            }
        }
        self.set_link_state(LinkState::Waiting);
        self.set_page(Page::Link);
    }

    pub fn set_link_state(&self, state: LinkState) {
        self.state.borrow_mut().link_expired = matches!(state, LinkState::Expired);
        self.link_picture.set_opacity(match state {
            LinkState::Waiting => 1.0,
            LinkState::Expired => 0.15,
        });
        self.link_hint.set_label(match state {
            LinkState::Waiting => "Waiting for your phone…",
            LinkState::Expired => "Code expired. Press Enter for a new one.",
        });
    }

    /// Leaves the startup/link screens once the account is usable. Does not
    /// interrupt a chat or the help page.
    pub fn show_chats(&self) {
        match self.page() {
            Some(Page::List | Page::Chat) => {}
            Some(Page::Help) => {
                let mut state = self.state.borrow_mut();
                if matches!(
                    state.page_before_help,
                    Some(Page::Status | Page::Link) | None
                ) {
                    state.page_before_help = Some(Page::List);
                }
            }
            Some(Page::Status | Page::Link) | None => self.set_page(Page::List),
        }
    }

    pub fn show_list(&self) {
        let previous = self.state.borrow_mut().current.take();
        self.set_page(Page::List);
        // Come back to where we left, like on mobile.
        let index =
            previous.and_then(|id| self.state.borrow().visible.iter().position(|v| *v == id));
        if let Some(row) = index
            .and_then(|i| i32::try_from(i).ok())
            .and_then(|i| self.list_box.row_at_index(i))
        {
            self.list_box.select_row(Some(&row));
            self.scroll_list_to(&row);
        }
    }

    pub fn show_error(&self, text: &str) {
        self.banner.set_label(text);
        self.banner_revealer.set_reveal_child(true);
        let generation = self.banner_generation.get() + 1;
        self.banner_generation.set(generation);
        let latest = Rc::downgrade(&self.banner_generation);
        let revealer = self.banner_revealer.downgrade();
        glib::timeout_add_seconds_local_once(BANNER_SECONDS, move || {
            let (Some(latest), Some(revealer)) = (latest.upgrade(), revealer.upgrade()) else {
                return;
            };
            if latest.get() == generation {
                revealer.set_reveal_child(false);
            }
        });
    }

    fn toggle_help(&self) {
        let current = self.page();
        if current == Some(Page::Help) {
            let back = self.state.borrow_mut().page_before_help.take();
            self.set_page(back.unwrap_or(Page::List));
        } else {
            self.state.borrow_mut().page_before_help = current;
            self.set_page(Page::Help);
        }
    }

    // ---- chat list ---------------------------------------------------------

    /// Replaces the accounts and their colours. With a single account,
    /// chats aren't marked with it.
    pub fn set_accounts(&self, accounts: Vec<AccountView>) {
        self.account_css.load_from_string(&account_css(&accounts));
        self.state.borrow_mut().accounts = accounts;
        self.rebuild_list();
        let current = self.state.borrow().current.clone();
        if let Some(current) = current {
            self.update_chat_title(&current);
        }
    }

    fn several_accounts(&self) -> bool {
        self.state.borrow().accounts.len() > 1
    }

    /// Replaces the chats of all accounts, in display order, and refreshes
    /// the list.
    pub fn set_conversations(&self, conversations: Vec<ListedChat>) {
        self.state.borrow_mut().conversations = conversations;
        self.rebuild_list();
        let current = self.state.borrow().current.clone();
        if let Some(current) = current {
            self.update_chat_title(&current);
        }
    }

    fn rebuild_list(&self) {
        let filter = self.search.text().to_lowercase();
        let selected_id = self.list_box.selected_row().and_then(|row| {
            self.state
                .borrow()
                .visible
                .get(usize::try_from(row.index()).ok()?)
                .cloned()
        });

        let several_accounts = self.several_accounts();
        let mut state = self.state.borrow_mut();
        // Right after linking without history there are no chats yet, so
        // list everyone rather than show an empty window.
        let any_activity = state
            .conversations
            .iter()
            .any(|c| c.summary.last_activity.is_some());
        let matches: Vec<&ListedChat> = state
            .conversations
            .iter()
            .filter(|c| {
                if filter.is_empty() {
                    c.summary.last_activity.is_some() || !any_activity
                } else {
                    c.summary.name.to_lowercase().contains(&filter)
                }
            })
            .take(if filter.is_empty() {
                usize::MAX
            } else {
                MAX_SEARCH_RESULTS
            })
            .collect();

        self.list_box.remove_all();
        for conversation in &matches {
            self.list_box
                .append(&conversation_row(conversation, several_accounts));
        }
        let visible: Vec<ChatRef> = matches.iter().map(|c| c.chat()).collect();
        // A background refresh must not move the selection under the
        // user's fingers; a new search starts at the best match.
        let select_index = selected_id
            .filter(|_| filter == state.listed_filter)
            .and_then(|id| visible.iter().position(|v| *v == id))
            .unwrap_or(0);
        state.listed_filter = filter;
        self.list_empty.set_visible(visible.is_empty());
        self.list_scroll.set_visible(!visible.is_empty());
        state.visible = visible;
        drop(state);

        if let Some(row) = self
            .list_box
            .row_at_index(i32::try_from(select_index).unwrap_or(0))
        {
            self.list_box.select_row(Some(&row));
        }
    }

    fn select_first(&self) {
        if let Some(row) = self.list_box.row_at_index(0) {
            self.list_box.select_row(Some(&row));
            self.list_scroll.vadjustment().set_value(0.0);
        }
    }

    fn move_selection(&self, delta: i32) {
        let len = i32::try_from(self.state.borrow().visible.len()).unwrap_or(i32::MAX);
        if len == 0 {
            return;
        }
        let current = self.list_box.selected_row().map_or(0, |r| r.index());
        let target = (current + delta).clamp(0, len - 1);
        let Some(row) = self.list_box.row_at_index(target) else {
            return;
        };
        self.list_box.select_row(Some(&row));
        self.scroll_list_to(&row);
    }

    fn scroll_list_to(&self, row: &gtk::ListBoxRow) {
        let Some(bounds) = row.compute_bounds(&self.list_box) else {
            return;
        };
        let adj = self.list_scroll.vadjustment();
        let (top, bottom) = (
            f64::from(bounds.y()),
            f64::from(bounds.y() + bounds.height()),
        );
        if top < adj.value() {
            adj.set_value(top);
        } else if bottom > adj.value() + adj.page_size() {
            adj.set_value(bottom - adj.page_size());
        }
    }

    fn open_row(&self, index: i32) {
        let id = usize::try_from(index)
            .ok()
            .and_then(|i| self.state.borrow().visible.get(i).cloned());
        if let Some(id) = id {
            self.emit(UiEvent::Open(id));
        }
    }

    /// Opens the conversation `delta` positions away from the current one.
    fn open_neighbour(&self, delta: i32) {
        let target = {
            let state = self.state.borrow();
            let len = i32::try_from(state.visible.len()).unwrap_or(i32::MAX);
            if len == 0 {
                return;
            }
            let position = state
                .current
                .as_ref()
                .and_then(|c| state.visible.iter().position(|v| v == c))
                .and_then(|p| i32::try_from(p).ok());
            match position {
                Some(p) => (p + delta).clamp(0, len - 1),
                None => 0,
            }
        };
        self.open_row(target);
    }

    // ---- chat --------------------------------------------------------------

    /// Whether the window is on screen and has keyboard focus.
    pub fn is_focused(&self) -> bool {
        self.window.is_visible() && self.window.is_active()
    }

    /// Whether the user is currently looking at this conversation.
    pub fn is_viewing(&self, id: &ChatRef) -> bool {
        self.is_focused()
            && self.page() == Some(Page::Chat)
            && self.state.borrow().current.as_ref() == Some(id)
    }

    pub fn current_chat(&self) -> Option<ChatRef> {
        self.state.borrow().current.clone()
    }

    fn report_seen(&self) {
        if self.page() != Some(Page::Chat) {
            return;
        }
        if let Some(current) = self.current_chat() {
            self.emit(UiEvent::Seen(current));
        }
    }

    fn update_chat_title(&self, id: &ChatRef) {
        let state = self.state.borrow();
        let name = state
            .conversations
            .iter()
            .find(|c| c.account == id.account && c.summary.id == id.conversation)
            .map(|c| c.summary.name.clone());
        self.chat_title
            .set_label(&name.unwrap_or_else(|| id.conversation.to_string()));
        let account = state
            .accounts
            .iter()
            .find(|a| a.id == id.account)
            .filter(|_| state.accounts.len() > 1);
        self.chat_account
            .set_label(&account.map(|a| format!("· {}", a.name)).unwrap_or_default());
        self.chat_header
            .set_css_classes(&["chat-header", &account_class(&id.account)]);
    }

    pub fn show_chat(&self, id: &ChatRef, messages: &[MessageView]) {
        let switching = self.state.borrow().current.as_ref() != Some(id);
        if switching {
            self.compose.buffer().set_text("");
        }
        self.state.borrow_mut().current = Some(id.clone());
        self.update_chat_title(id);
        {
            let mut state = self.state.borrow_mut();
            state.stick_to_bottom = true;
            state.restore_scroll = None;
        }
        self.render_messages(messages);
        // If the new content has the same height as the old, `upper` won't
        // change and the pinning handler won't fire, so scroll explicitly.
        let adj = self.messages_scroll.vadjustment();
        adj.set_value(adj.upper() - adj.page_size());
        // Opening from a search result: go back to the full list afterwards.
        if !self.search.text().is_empty() {
            self.search.set_text("");
        }
        if self.page() != Some(Page::Chat) {
            self.set_page(Page::Chat);
        }
        self.report_seen();
    }

    /// Re-renders the open conversation if it is `id`, keeping the scroll
    /// position unless the view is pinned to the newest message.
    pub fn refresh_chat(&self, id: &ChatRef, messages: &[MessageView]) {
        let mut state = self.state.borrow_mut();
        if state.current.as_ref() != Some(id) {
            return;
        }
        if !state.stick_to_bottom {
            state.restore_scroll = Some(self.messages_scroll.vadjustment().value());
        }
        drop(state);
        self.render_messages(messages);
    }

    fn render_messages(&self, messages: &[MessageView]) {
        self.messages.remove_all();
        let Some(account) = self.current_chat().map(|c| c.account) else {
            return;
        };
        let mut last_day = None;
        for message in messages {
            let day = format::day_key(message.timestamp);
            if day != last_day {
                last_day = day;
                self.messages.append(&day_separator(message.timestamp));
            }
            self.messages.append(&message_row(message, &account));
        }
    }

    fn append_pending(&self, account: &AccountId, text: &str) {
        self.state.borrow_mut().stick_to_bottom = true;
        let view = MessageView {
            status: MessageStatus::Pending,
            sender: None,
            body: text.to_owned(),
            timestamp: Timestamp(glib::real_time() / 1000),
            edited: false,
            deleted: false,
        };
        self.messages.append(&message_row(&view, account));
    }

    /// Puts the text of a failed send back into the compose box.
    pub fn restore_draft(&self, id: &ChatRef, text: &str) {
        let buffer = self.compose.buffer();
        if self.state.borrow().current.as_ref() == Some(id) && buffer.char_count() == 0 {
            buffer.set_text(text);
        }
    }

    fn send_compose(&self) {
        let Some(chat) = self.current_chat() else {
            return;
        };
        let buffer = self.compose.buffer();
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.append_pending(&chat.account, text);
        self.emit(UiEvent::Send {
            chat,
            text: text.to_owned(),
        });
        buffer.set_text("");
    }

    fn scroll_messages(&self, pages: f64) {
        let adj = self.messages_scroll.vadjustment();
        adj.set_value(adj.value() + pages * adj.page_increment());
    }

    // ---- keyboard ----------------------------------------------------------

    fn on_key(&self, key: gdk::Key, mods: gdk::ModifierType) -> glib::Propagation {
        let mods = mods & gtk::accelerator_get_default_mod_mask();
        let ctrl = mods == gdk::ModifierType::CONTROL_MASK;
        let alt = mods == gdk::ModifierType::ALT_MASK;
        let plain = mods.is_empty();
        let key = key.to_lower();
        let page = self.page();

        let handled = match (key, page) {
            (gdk::Key::q, _) if ctrl => {
                self.emit(UiEvent::Quit);
                true
            }
            (gdk::Key::F1, _) if plain => {
                self.toggle_help();
                true
            }
            (gdk::Key::comma, _) if ctrl => {
                self.emit(UiEvent::OpenSettings);
                true
            }
            (gdk::Key::Escape, Some(Page::Help)) => {
                self.toggle_help();
                true
            }
            (gdk::Key::Escape, Some(Page::Chat)) => {
                self.show_list();
                true
            }
            (gdk::Key::Left, Some(Page::Chat)) if alt => {
                self.show_list();
                true
            }
            (gdk::Key::Escape, Some(Page::Link)) => {
                self.emit(UiEvent::CancelLink);
                true
            }
            (gdk::Key::Escape, Some(Page::List)) if !self.search.text().is_empty() => {
                self.search.set_text("");
                true
            }
            (gdk::Key::Escape, _) => {
                self.hide();
                true
            }
            (gdk::Key::Return | gdk::Key::KP_Enter, Some(Page::Link)) => {
                if self.state.borrow().link_expired {
                    self.emit(UiEvent::RetryLink);
                }
                true
            }
            (_, Some(Page::List)) => self.on_list_key(key, plain, ctrl, alt),
            (_, Some(Page::Chat)) => self.on_chat_key(key, plain, ctrl, alt),
            _ => false,
        };
        if handled {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    }

    fn on_list_key(&self, key: gdk::Key, plain: bool, ctrl: bool, alt: bool) -> bool {
        match key {
            gdk::Key::Up if plain || alt => self.move_selection(-1),
            gdk::Key::Down if plain || alt => self.move_selection(1),
            gdk::Key::p | gdk::Key::k if ctrl => self.move_selection(-1),
            gdk::Key::n | gdk::Key::j if ctrl => self.move_selection(1),
            gdk::Key::Page_Up if plain => self.move_selection(-LIST_PAGE_ROWS),
            gdk::Key::Page_Down if plain => self.move_selection(LIST_PAGE_ROWS),
            gdk::Key::Return | gdk::Key::KP_Enter if plain => {
                if let Some(row) = self.list_box.selected_row() {
                    self.open_row(row.index());
                }
            }
            _ => return self.on_jump_key(key, alt),
        }
        true
    }

    fn on_chat_key(&self, key: gdk::Key, plain: bool, ctrl: bool, alt: bool) -> bool {
        match key {
            gdk::Key::Return | gdk::Key::KP_Enter if plain => self.send_compose(),
            gdk::Key::Page_Up if plain => self.scroll_messages(-1.0),
            gdk::Key::Page_Down if plain => self.scroll_messages(1.0),
            gdk::Key::Up if alt => self.open_neighbour(-1),
            gdk::Key::Down if alt => self.open_neighbour(1),
            gdk::Key::Page_Up if ctrl => self.open_neighbour(-1),
            gdk::Key::Page_Down if ctrl => self.open_neighbour(1),
            _ => return self.on_jump_key(key, alt),
        }
        true
    }

    /// Alt+1…9 opens the n-th chat of the list.
    fn on_jump_key(&self, key: gdk::Key, alt: bool) -> bool {
        if !alt {
            return false;
        }
        let Some(digit) = key.to_unicode().and_then(|c| c.to_digit(10)) else {
            return false;
        };
        if digit == 0 {
            return false;
        }
        self.open_row(i32::try_from(digit - 1).expect("single digit fits"));
        true
    }
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    add_css_provider(&provider);
}

fn add_css_provider(provider: &gtk::CssProvider) {
    let display = gdk::Display::default().expect("GTK is initialized before building the UI");
    gtk::style_context_add_provider_for_display(
        &display,
        provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn setup_layer_shell(
    window: &gtk::ApplicationWindow,
    config: &Config,
    events: async_channel::Sender<UiEvent>,
) {
    window.init_layer_shell();
    window.set_namespace(Some("signal-tray"));
    window.set_layer(Layer::Top);
    // Take keyboard focus when shown, but let the user click elsewhere.
    window.set_keyboard_mode(KeyboardMode::OnDemand);
    let placement = Placement {
        corner: config.corner,
        margin: config.margin,
    };
    placement::apply(window, placement);
    placement::enable_dragging(window, placement, move |placement| {
        // Fails only once the app is shutting down.
        let _ = events.try_send(UiEvent::Moved(placement));
    });
}

fn vbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Vertical, spacing)
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    for class in classes {
        label.add_css_class(class);
    }
    label
}

fn account_class(account: &AccountId) -> String {
    format!("acct-{account}")
}

/// Colours outgoing bubbles, and with several accounts, the stripe of
/// list rows and the chat header's underline. Generated rather than using
/// CSS variables, which need GTK 4.16.
fn account_css(accounts: &[AccountView]) -> String {
    let mut css = String::new();
    for account in accounts {
        let (class, color) = (account_class(&account.id), account.color);
        let _ = writeln!(
            css,
            ".bubble.outgoing.{class} {{ background: {color}; color: {}; }}",
            color.text_color()
        );
        if accounts.len() > 1 {
            let _ = writeln!(css, ".{class} > .stripe {{ background: {color}; }}");
            let _ = writeln!(
                css,
                ".chat-header.{class} {{ border-bottom: 2px solid {color}; }}"
            );
        }
    }
    css
}

fn conversation_row(chat: &ListedChat, show_account: bool) -> gtk::ListBoxRow {
    let conversation = &chat.summary;
    let name = label(&conversation.name, &["name"]);
    name.set_ellipsize(pango::EllipsizeMode::End);
    name.set_hexpand(true);
    if conversation.unread > 0 {
        name.add_css_class("unread");
    }
    let time = label(
        &conversation
            .last_activity
            .map(format::list_time)
            .unwrap_or_default(),
        &["time"],
    );
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    top.append(&name);
    top.append(&time);

    let preview = label(
        conversation.last_preview.as_deref().unwrap_or(""),
        &["preview"],
    );
    preview.set_ellipsize(pango::EllipsizeMode::End);
    preview.set_hexpand(true);
    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bottom.append(&preview);
    if conversation.unread > 0 {
        let badge = label(&conversation.unread.to_string(), &["badge"]);
        badge.set_valign(gtk::Align::Center);
        bottom.append(&badge);
    }

    let content = vbox(0);
    content.add_css_class("conversation");
    content.set_hexpand(true);
    content.append(&top);
    // Contacts without messages get a single line.
    if conversation
        .last_preview
        .as_deref()
        .is_some_and(|p| !p.is_empty())
        || conversation.unread > 0
    {
        content.append(&bottom);
    }
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.add_css_class(&account_class(&chat.account));
    if show_account {
        row.append(&gtk::Box::builder().css_classes(["stripe"]).build());
    }
    row.append(&content);
    gtk::ListBoxRow::builder().child(&row).build()
}

fn day_separator(ts: Timestamp) -> gtk::ListBoxRow {
    let day = gtk::Label::builder()
        .label(format::day_label(ts))
        .css_classes(["day"])
        .build();
    gtk::ListBoxRow::builder()
        .child(&day)
        .activatable(false)
        .focusable(false)
        .build()
}

fn message_row(message: &MessageView, account: &AccountId) -> gtk::ListBoxRow {
    let outgoing = !matches!(message.status, MessageStatus::Incoming);
    let bubble = vbox(0);
    bubble.add_css_class("bubble");
    bubble.add_css_class(if outgoing { "outgoing" } else { "incoming" });
    bubble.add_css_class(&account_class(account));
    bubble.set_halign(if outgoing {
        gtk::Align::End
    } else {
        gtk::Align::Start
    });

    if let Some(sender) = &message.sender {
        bubble.append(&label(sender, &["sender"]));
    }

    let body = if message.deleted {
        label("This message was deleted", &["body", "deleted"])
    } else {
        label(&message.body, &["body"])
    };
    body.set_wrap(true);
    body.set_wrap_mode(pango::WrapMode::WordChar);
    body.set_max_width_chars(34);
    bubble.append(&body);

    let mut meta = format::message_time(message.timestamp);
    if message.edited {
        meta.push_str(" · edited");
    }
    let tick = match message.status {
        MessageStatus::Incoming => None,
        MessageStatus::Pending => Some("…"),
        MessageStatus::Delivery(DeliveryStatus::Sent) => Some("✓"),
        MessageStatus::Delivery(DeliveryStatus::Delivered | DeliveryStatus::Read) => Some("✓✓"),
    };
    if let Some(tick) = tick {
        meta.push(' ');
        meta.push_str(tick);
    }
    let meta = label(&meta, &["meta"]);
    meta.set_halign(gtk::Align::End);
    if message.status == MessageStatus::Delivery(DeliveryStatus::Read) {
        meta.add_css_class("read");
    }
    bubble.append(&meta);

    gtk::ListBoxRow::builder()
        .child(&bubble)
        .activatable(false)
        .focusable(false)
        .build()
}

fn help_page() -> gtk::ScrolledWindow {
    const SHORTCUTS: &[(&str, &str)] = &[
        ("Global", ""),
        ("signal-tray toggle", "bind in your WM"),
        ("Esc", "back / clear / hide"),
        ("Ctrl+Q", "quit"),
        ("F1", "this help"),
        ("Ctrl+,", "settings"),
        ("Super+drag", "move window"),
        ("Chat list", ""),
        ("type", "search chats & contacts"),
        ("↑ ↓  Ctrl+J/K", "select"),
        ("Enter", "open"),
        ("Alt+1…9", "open n-th chat"),
        ("Chat", ""),
        ("Enter", "send"),
        ("Shift+Enter", "new line"),
        ("Alt+↑ ↓", "previous / next chat"),
        ("PgUp PgDn", "scroll history"),
        ("Ctrl+.", "emoji"),
    ];
    let grid = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(2)
        .css_classes(["help"])
        .build();
    for (row, (key, what)) in (0..).zip(SHORTCUTS) {
        if what.is_empty() {
            grid.attach(&label(key, &["heading"]), 0, row, 2, 1);
        } else {
            grid.attach(&label(key, &["key"]), 0, row, 1, 1);
            grid.attach(&label(what, &["dim"]), 1, row, 1, 1);
        }
    }
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&grid)
        .build()
}

fn qr_texture(data: &str) -> Result<gdk::MemoryTexture, qrcode::types::QrError> {
    const SCALE: usize = 5;
    const QUIET_ZONE: usize = 2;
    let code = qrcode::QrCode::new(data.as_bytes())?;
    let modules = code.width();
    let colors = code.to_colors();
    let size = (modules + 2 * QUIET_ZONE) * SCALE;
    let mut pixels = vec![0xffu8; size * size * 3];
    for (i, color) in colors.iter().enumerate() {
        if *color != qrcode::Color::Dark {
            continue;
        }
        let (mx, my) = (i % modules + QUIET_ZONE, i / modules + QUIET_ZONE);
        for y in my * SCALE..(my + 1) * SCALE {
            let row = y * size * 3;
            pixels[row + mx * SCALE * 3..row + (mx + 1) * SCALE * 3].fill(0);
        }
    }
    let side = i32::try_from(size).expect("QR codes are at most a few hundred pixels");
    Ok(gdk::MemoryTexture::new(
        side,
        side,
        gdk::MemoryFormat::R8g8b8,
        &glib::Bytes::from_owned(pixels),
        size * 3,
    ))
}
