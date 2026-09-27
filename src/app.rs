//! Glue between the Signal backend, the local store, the tray and the
//! window. Runs entirely on the GLib main thread.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gio, glib};

use crate::config::{self, Config};
use crate::model::{self, Author, ConversationId, NewMessage, PeerId, SignalEvent, Timestamp};
use crate::signal::{self, Backend, Connection, Event, HistoryStatus, Origin, SendError};
use crate::store::{ReadState, Store};
use crate::tray::{self, TrayAction, TrayHandle};
use crate::ui::settings::Setting;
use crate::ui::{self, LinkState, MessageStatus, MessageView, Status, Ui, UiEvent};

/// How often disappearing messages are checked for expiry.
const PURGE_INTERVAL: Duration = Duration::from_secs(30);

pub const OPEN_CONVERSATION_ACTION: &str = "open-conversation";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Talk to Signal.
    Live,
    /// Fake data, in-memory store, nothing leaves the machine.
    Demo,
}

pub struct Controller {
    weak_self: Weak<Self>,
    app: gtk::Application,
    config: RefCell<Config>,
    /// Where settings changes are saved; `None` in demo mode, which leaves
    /// the user's files alone.
    config_path: Option<PathBuf>,
    ui: Rc<Ui>,
    /// Built on first use, then hidden and re-shown.
    settings: OnceCell<gtk::Window>,
    store: RefCell<Store>,
    tray: Option<TrayHandle>,
    /// `None` in demo mode.
    backend: Option<Backend>,
    refresh_scheduled: Cell<bool>,
    /// Whether the open chat must be re-rendered on the next refresh.
    chat_dirty: Cell<bool>,
    _hold: gio::ApplicationHoldGuard,
}

impl Controller {
    pub fn new(
        app: &gtk::Application,
        config: Config,
        config_path: PathBuf,
        mode: Mode,
    ) -> anyhow::Result<Rc<Self>> {
        let (store, backend) = match mode {
            Mode::Live => {
                let dir = config::ensure_private_data_dir()?;
                let (backend, events) = Backend::spawn(signal::SpawnOptions {
                    store_path: dir.join("presage.sqlite"),
                    history_marker: dir.join("history-import-pending"),
                    device_name: config.device_name.clone(),
                })?;
                (
                    Store::open(&dir.join("messages.sqlite"))?,
                    Some((backend, events)),
                )
            }
            Mode::Demo => {
                let mut store = Store::open_in_memory()?;
                crate::demo::populate(&mut store)?;
                (store, None)
            }
        };
        let (backend, backend_events) = backend.unzip();

        let (ui_tx, ui_rx) = async_channel::unbounded();
        let ui = Ui::new(app, &config, ui_tx);

        let (tray_tx, tray_rx) = async_channel::unbounded();
        let tray = tray::spawn(tray_tx);

        let controller = Rc::new_cyclic(|weak_self| Self {
            weak_self: weak_self.clone(),
            app: app.clone(),
            config: RefCell::new(config),
            config_path: (mode == Mode::Live).then_some(config_path),
            ui,
            settings: OnceCell::new(),
            store: RefCell::new(store),
            tray,
            backend,
            refresh_scheduled: Cell::default(),
            chat_dirty: Cell::default(),
            _hold: app.hold(),
        });

        controller.install_actions();
        glib::spawn_future_local(controller.clone().ui_loop(ui_rx));
        glib::spawn_future_local(controller.clone().tray_loop(tray_rx));
        glib::spawn_future_local(controller.clone().purge_loop());
        match backend_events {
            Some(events) => {
                glib::spawn_future_local(controller.clone().backend_loop(events));
            }
            None => {
                controller.refresh();
                controller.ui.show_chats();
            }
        }
        Ok(controller)
    }

    pub fn ui(&self) -> &Ui {
        &self.ui
    }

    pub fn start_hidden(&self) -> bool {
        self.config.borrow().start_hidden
    }

    fn install_actions(self: &Rc<Self>) {
        let open = gio::SimpleAction::new(OPEN_CONVERSATION_ACTION, Some(glib::VariantTy::STRING));
        let weak = Rc::downgrade(self);
        open.connect_activate(move |_, target| {
            let Some(this) = weak.upgrade() else { return };
            let Some(id) = target.and_then(|t| t.str()).and_then(|s| s.parse().ok()) else {
                tracing::warn!(?target, "notification action with invalid conversation");
                return;
            };
            this.ui.show();
            this.open(&id);
        });
        self.app.add_action(&open);
    }

    // ---- settings ----------------------------------------------------------

    fn open_settings(&self) {
        self.settings
            .get_or_init(|| {
                let weak = self.weak_self.clone();
                let window =
                    ui::settings::window(&self.app, &self.config.borrow(), move |setting| {
                        if let Some(this) = weak.upgrade() {
                            this.change_setting(setting);
                        }
                    });
                self.ui.watch_focus(&window);
                window
            })
            .present();
    }

    fn change_setting(&self, setting: Setting) {
        {
            let mut config = self.config.borrow_mut();
            match setting {
                Setting::Opacity(opacity) if opacity != config.opacity => {
                    config.opacity = opacity;
                    self.ui.set_opacity(opacity);
                }
                Setting::HideOnFocusLoss(enabled) if enabled != config.hide_on_focus_loss => {
                    config.hide_on_focus_loss = enabled;
                    self.ui.set_hide_on_focus_loss(enabled);
                }
                Setting::Opacity(_) | Setting::HideOnFocusLoss(_) => return,
            }
        }
        self.save_config();
    }

    fn save_config(&self) {
        let Some(path) = &self.config_path else {
            return;
        };
        if let Err(err) = self.config.borrow().save(path) {
            tracing::error!(err = format!("{err:#}"), "saving settings failed");
            self.ui.show_error("Couldn't save settings; see the log.");
        }
    }

    // ---- event loops -------------------------------------------------------

    async fn ui_loop(self: Rc<Self>, events: async_channel::Receiver<UiEvent>) {
        while let Ok(event) = events.recv().await {
            tracing::debug!(kind = event.kind(), "ui event");
            match event {
                UiEvent::Open(id) => self.open(&id),
                UiEvent::Seen(id) => self.mark_seen(&id),
                UiEvent::Send { conversation, text } => {
                    glib::spawn_future_local(self.clone().send(conversation, text));
                }
                UiEvent::Shown => {}
                UiEvent::RetryLink => {
                    if let Some(backend) = &self.backend {
                        self.ui.show_status(Status::Busy, "Requesting link code…");
                        backend.retry_link();
                    }
                }
                UiEvent::OpenSettings => self.open_settings(),
                UiEvent::Moved(placement) => {
                    {
                        let mut config = self.config.borrow_mut();
                        config.corner = placement.corner;
                        config.margin = placement.margin;
                    }
                    self.save_config();
                }
                UiEvent::Quit => self.app.quit(),
            }
        }
    }

    async fn tray_loop(self: Rc<Self>, actions: async_channel::Receiver<TrayAction>) {
        while let Ok(action) = actions.recv().await {
            match action {
                TrayAction::Toggle => self.ui.toggle(),
                TrayAction::Settings => self.open_settings(),
                TrayAction::Quit => self.app.quit(),
            }
        }
    }

    async fn purge_loop(self: Rc<Self>) {
        loop {
            let now = Timestamp(glib::real_time() / 1000);
            let purged = self.store.borrow_mut().purge_expired(now);
            match purged {
                Ok(conversations) => {
                    for conversation in &conversations {
                        self.app.withdraw_notification(&conversation.to_string());
                        self.touch(Some(conversation));
                    }
                }
                Err(err) => tracing::error!(%err, "purging disappearing messages failed"),
            }
            glib::timeout_future(PURGE_INTERVAL).await;
        }
    }

    async fn backend_loop(self: Rc<Self>, events: async_channel::Receiver<Event>) {
        while let Ok(event) = events.recv().await {
            match event {
                Event::LinkCode(uri) => self.ui.show_link(&uri),
                Event::LinkFailed(reason) => {
                    tracing::info!(%reason, "link code expired");
                    self.ui.set_link_state(LinkState::Expired);
                }
                Event::History(status) => self.ui.show_status(Status::Busy, &history_text(&status)),
                Event::Ready { account } => {
                    tracing::info!(%account, "signal ready");
                    self.refresh();
                    self.ui.show_chats();
                }
                Event::Messages {
                    events,
                    origin,
                    committed,
                } => {
                    for event in events {
                        self.apply(event, origin);
                    }
                    if let Some(committed) = committed {
                        // Every event was applied (or logged as failed) above.
                        let _ = committed.send(());
                    }
                }
                Event::Names(names) => {
                    let result = self
                        .store
                        .borrow_mut()
                        .upsert_names(names.iter().map(|(id, name)| (id, name.as_str())));
                    match result {
                        Ok(()) => {
                            tracing::info!(count = names.len(), "updated contact names");
                            self.schedule_refresh();
                        }
                        Err(err) => tracing::error!(%err, "storing contact names failed"),
                    }
                }
                Event::Connection(Connection::Online) => tracing::info!("connected to Signal"),
                Event::Connection(Connection::Offline) => {
                    self.ui.show_error("Offline, reconnecting…");
                }
                Event::Fatal(err) => {
                    self.ui
                        .show_status(Status::Failed, &format!("Signal stopped working:\n{err}"));
                }
            }
        }
    }

    // ---- actions -----------------------------------------------------------

    fn open(&self, id: &ConversationId) {
        match self.message_views(id) {
            Ok(views) => self.ui.show_chat(id, &views),
            Err(err) => {
                tracing::error!(%err, conversation = %id, "loading messages failed");
                self.ui.show_error("Could not load messages");
            }
        }
    }

    /// Marks a conversation read locally and tells the senders.
    fn mark_seen(&self, id: &ConversationId) {
        self.app.withdraw_notification(&id.to_string());
        let receipts = match self.store.borrow_mut().mark_conversation_read(id) {
            Ok(receipts) => receipts,
            Err(err) => {
                tracing::error!(%err, conversation = %id, "marking read failed");
                return;
            }
        };
        if receipts.is_empty() {
            return;
        }
        self.schedule_refresh();
        let Some(backend) = &self.backend else {
            return;
        };
        let mut by_sender: HashMap<PeerId, Vec<Timestamp>> = HashMap::new();
        for receipt in receipts {
            by_sender
                .entry(receipt.sender)
                .or_default()
                .push(receipt.timestamp);
        }
        for (sender, timestamps) in by_sender {
            backend.send_read_receipts(sender, timestamps);
        }
    }

    async fn send(self: Rc<Self>, conversation: ConversationId, text: String) {
        let timestamp = Timestamp(glib::real_time() / 1000);
        let result: Result<(), SendError> = match &self.backend {
            Some(backend) => backend.send_text(&conversation, &text, timestamp).await,
            None => Ok(()),
        };
        if let Err(err) = result {
            tracing::warn!(%err, %conversation, "sending failed");
            self.ui.show_error(&format!("Not sent: {err}"));
            self.ui.restore_draft(&conversation, &text);
            self.chat_dirty.set(true);
            self.schedule_refresh();
            return;
        }
        // presage applies the conversation's timer when sending; mirror it.
        let expires_in = self
            .store
            .borrow()
            .expiration_timer(&conversation)
            .unwrap_or_else(|err| {
                tracing::error!(%err, "reading expiration timer failed");
                None
            });
        let message = NewMessage {
            conversation,
            author: Author::Me,
            author_name: None,
            timestamp,
            body: text,
            expires_in,
        };
        if let Err(err) = self
            .store
            .borrow_mut()
            .insert_message(&message, ReadState::Read)
        {
            tracing::error!(%err, "storing sent message failed");
        }
        self.chat_dirty.set(true);
        self.schedule_refresh();
    }

    /// Applies one event from Signal to the store and UI.
    fn apply(&self, event: SignalEvent, origin: Origin) {
        if let SignalEvent::Message(message) = &event {
            if let Err(err) = self.apply_message(message, origin) {
                tracing::error!(%err, kind = event.kind(), "applying signal event failed");
            }
            return;
        }
        // The store borrow must end before the UI is touched.
        let affected = {
            let mut store = self.store.borrow_mut();
            let with_conversation =
                |found: bool, c: &ConversationId| if found { vec![c.clone()] } else { vec![] };
            match &event {
                SignalEvent::Message(_) => unreachable!("handled above"),
                SignalEvent::Edit {
                    conversation,
                    author,
                    target,
                    body,
                } => store
                    .edit_message(conversation, author, *target, body)
                    .map(|found| with_conversation(found, conversation)),
                SignalEvent::Delete {
                    conversation,
                    author,
                    target,
                } => store
                    .delete_message(conversation, author, *target)
                    .map(|found| with_conversation(found, conversation)),
                SignalEvent::ReadElsewhere { sender, timestamp } => {
                    store.mark_read_elsewhere(sender, *timestamp)
                }
                SignalEvent::ExpirationTimer {
                    conversation,
                    expires_in,
                } => store
                    .set_expiration_timer(conversation, *expires_in)
                    .map(|()| Vec::new()),
                SignalEvent::Receipt { status, timestamps } => {
                    store.apply_receipt(*status, timestamps)
                }
            }
        };
        let affected = match affected {
            Ok(affected) => affected,
            Err(err) => {
                tracing::error!(%err, kind = event.kind(), "applying signal event failed");
                return;
            }
        };
        for conversation in &affected {
            if matches!(
                event,
                SignalEvent::ReadElsewhere { .. } | SignalEvent::Delete { .. }
            ) {
                self.app.withdraw_notification(&conversation.to_string());
            }
            self.touch(Some(conversation));
        }
    }

    fn apply_message(&self, message: &NewMessage, origin: Origin) -> anyhow::Result<()> {
        let read = match (&message.author, origin) {
            (Author::Peer(_), Origin::Live) => ReadState::Unread,
            // History was read on the phone, or it would not be history.
            (Author::Peer(_), Origin::Stored) | (Author::Me, _) => ReadState::Read,
        };
        if !self.store.borrow_mut().insert_message(message, read)? {
            return Ok(());
        }
        self.touch(Some(&message.conversation));
        if read == ReadState::Unread {
            if self.ui.is_viewing(&message.conversation) {
                self.mark_seen(&message.conversation);
            } else if !self.ui.is_focused() {
                // While the window has focus, the unread badge is enough.
                self.notify(message);
            }
        } else {
            // Replying from the phone means the chat was read there.
            self.app
                .withdraw_notification(&message.conversation.to_string());
        }
        Ok(())
    }

    /// Records that a conversation changed and schedules a UI refresh.
    fn touch(&self, conversation: Option<&ConversationId>) {
        let Some(conversation) = conversation else {
            return;
        };
        if self.ui.current_conversation().as_ref() == Some(conversation) {
            self.chat_dirty.set(true);
        }
        self.schedule_refresh();
    }

    fn notify(&self, message: &NewMessage) {
        if !self.config.borrow().notifications {
            return;
        }
        let title = match self.store.borrow().conversation(&message.conversation) {
            Ok(Some(summary)) => summary.name,
            _ => "Signal".to_owned(),
        };
        let body = match (&message.conversation, &message.author) {
            (ConversationId::Group(_), Author::Peer(peer)) => {
                format!(
                    "{}: {}",
                    self.peer_display_name(peer, message.author_name.as_deref()),
                    message.body
                )
            }
            _ => message.body.clone(),
        };
        let notification = gio::Notification::new(&title);
        notification.set_body(Some(&body));
        notification.set_default_action_and_target_value(
            &format!("app.{OPEN_CONVERSATION_ACTION}"),
            Some(&message.conversation.to_string().to_variant()),
        );
        self.app
            .send_notification(Some(&message.conversation.to_string()), &notification);
    }

    // ---- refreshing --------------------------------------------------------

    /// Coalesces bursts of events (e.g. the backlog after being offline)
    /// into a single UI update.
    fn schedule_refresh(&self) {
        if self.refresh_scheduled.replace(true) {
            return;
        }
        let this = self.weak_self.clone();
        glib::idle_add_local_once(move || {
            if let Some(this) = this.upgrade() {
                this.refresh_scheduled.set(false);
                this.refresh();
            }
        });
    }

    fn refresh(&self) {
        let store = self.store.borrow();
        match store.conversations() {
            Ok(conversations) => self.ui.set_conversations(conversations),
            Err(err) => tracing::error!(%err, "loading conversations failed"),
        }
        if let Some(tray) = &self.tray {
            match store.total_unread() {
                Ok(unread) => tray.set_unread(unread),
                Err(err) => tracing::error!(%err, "counting unread failed"),
            }
        }
        drop(store);
        if self.chat_dirty.replace(false)
            && let Some(current) = self.ui.current_conversation()
        {
            match self.message_views(&current) {
                Ok(views) => self.ui.refresh_chat(&current, &views),
                Err(err) => tracing::error!(%err, "reloading chat failed"),
            }
        }
    }

    fn message_views(&self, id: &ConversationId) -> anyhow::Result<Vec<MessageView>> {
        let store = self.store.borrow();
        let messages = store.messages(id)?;
        let is_group = matches!(id, ConversationId::Group(_));
        let mut names: HashMap<PeerId, String> = HashMap::new();
        Ok(messages
            .into_iter()
            .map(|m| {
                let (status, sender) = match &m.author {
                    Author::Me => (
                        MessageStatus::Delivery(m.status.unwrap_or(model::DeliveryStatus::Sent)),
                        None,
                    ),
                    Author::Peer(peer) => {
                        let sender = is_group.then(|| {
                            names
                                .entry(peer.clone())
                                .or_insert_with(|| {
                                    store
                                        .peer_name(peer)
                                        .ok()
                                        .flatten()
                                        .or_else(|| m.author_name.clone())
                                        .unwrap_or_else(|| short_id(peer))
                                })
                                .clone()
                        });
                        (MessageStatus::Incoming, sender)
                    }
                };
                MessageView {
                    status,
                    sender,
                    body: m.body,
                    timestamp: m.timestamp,
                    edited: m.edited,
                    deleted: m.deleted,
                }
            })
            .collect())
    }

    fn peer_display_name(&self, peer: &PeerId, advertised: Option<&str>) -> String {
        self.store
            .borrow()
            .peer_name(peer)
            .ok()
            .flatten()
            .or_else(|| advertised.map(str::to_owned))
            .unwrap_or_else(|| short_id(peer))
    }
}

/// Numbers are shown in full, uuids abbreviated.
fn short_id(peer: &PeerId) -> String {
    if peer.0.starts_with('+') {
        peer.0.clone()
    } else {
        peer.0.chars().take(8).collect()
    }
}

fn history_text(status: &HistoryStatus) -> String {
    match status {
        HistoryStatus::WaitingForPhone => {
            "Linked!\n\nOn your phone, choose \u{201c}Transfer message history\u{201d}\nto bring your chats over."
                .to_owned()
        }
        HistoryStatus::Downloading { received, total } => {
            let percent = if *total == 0 { 0 } else { received * 100 / total };
            format!("Downloading message history… {percent}%")
        }
        HistoryStatus::Importing => "Importing message history…".to_owned(),
    }
}
