//! Glue between the Signal backends, the local stores, the tray and the
//! window. Runs entirely on the GLib main thread.
//!
//! Every account has its own backend and store; the window shows the chats
//! of all of them in one list.

use std::cell::{Cell, OnceCell, RefCell};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gio, glib};

use crate::accounts;
use crate::config::{self, AccountConfig, Config};
use crate::model::{
    self, AccountId, Author, ChatRef, ConversationId, NewMessage, PeerId, SignalEvent, Timestamp,
};
use crate::signal::{self, Backend, Connection, Event, HistoryStatus, Origin, SendError};
use crate::store::{ConversationSummary, ReadState, Store};
use crate::tray::{self, TrayAction, TrayHandle};
use crate::ui::settings::{Setting, Settings};
use crate::ui::{
    AccountView, LinkState, ListedChat, MessageStatus, MessageView, Status, Ui, UiEvent,
};

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

struct Account {
    id: AccountId,
    store: RefCell<Store>,
    /// `None` in demo mode. Taken when the account is removed, which stops
    /// the backend and ends its event loop.
    backend: RefCell<Option<Backend>>,
    /// Phone number, once ready.
    number: RefCell<Option<String>>,
    ready: Cell<bool>,
    /// A running history import was announced; progress isn't.
    history_announced: Cell<bool>,
    /// Chats with a notification up, to withdraw when the account goes.
    notified: RefCell<HashSet<ConversationId>>,
}

impl Account {
    fn new(id: AccountId, store: Store, backend: Option<Backend>) -> Rc<Self> {
        Rc::new(Self {
            id,
            store: RefCell::new(store),
            backend: RefCell::new(backend),
            number: RefCell::default(),
            ready: Cell::default(),
            history_announced: Cell::default(),
            notified: RefCell::default(),
        })
    }

    fn chat(&self, conversation: ConversationId) -> ChatRef {
        ChatRef {
            account: self.id.clone(),
            conversation,
        }
    }
}

/// Opens an account's store and starts its backend.
fn open_account(
    data_dir: &Path,
    id: &AccountId,
    device_name: &str,
) -> anyhow::Result<(Rc<Account>, async_channel::Receiver<Event>)> {
    let paths = accounts::paths(data_dir, id);
    std::fs::create_dir_all(&paths.dir)?;
    let store = Store::open(&paths.messages)?;
    let (backend, events) = Backend::spawn(signal::SpawnOptions {
        label: id.to_string(),
        store_path: paths.presage,
        history_marker: paths.history_marker,
        device_name: device_name.to_owned(),
    })?;
    Ok((Account::new(id.clone(), store, Some(backend)), events))
}

pub struct Controller {
    weak_self: Weak<Self>,
    app: gtk::Application,
    config: RefCell<Config>,
    /// Where settings changes are saved; `None` in demo mode, which leaves
    /// the user's files alone.
    config_path: Option<PathBuf>,
    /// `None` in demo mode.
    data_dir: Option<PathBuf>,
    ui: Rc<Ui>,
    /// Built on first use, then hidden and re-shown.
    settings: OnceCell<Settings>,
    /// The running accounts: the configured ones that could be opened, and
    /// one being linked.
    accounts: RefCell<BTreeMap<AccountId, Rc<Account>>>,
    /// The account whose linking owns the window. At most one at a time.
    linking: RefCell<Option<AccountId>>,
    tray: Option<TrayHandle>,
    refresh_scheduled: Cell<bool>,
    /// Whether the open chat must be re-rendered on the next refresh.
    chat_dirty: Cell<bool>,
    _hold: gio::ApplicationHoldGuard,
}

impl Controller {
    pub fn new(
        app: &gtk::Application,
        mut config: Config,
        config_path: PathBuf,
        mode: Mode,
    ) -> anyhow::Result<Rc<Self>> {
        let mut opened = BTreeMap::new();
        let mut backends = Vec::new();
        let mut failed = Vec::new();
        let data_dir = match mode {
            Mode::Live => {
                let dir = config::ensure_private_data_dir()?;
                accounts::migrate_single_account(&dir, &mut config, |config| {
                    config.save(&config_path)
                })?;
                let ids: Vec<AccountId> = config.accounts.iter().map(|a| a.id.clone()).collect();
                accounts::remove_unlisted(&dir, &ids);
                for id in ids {
                    match open_account(&dir, &id, &config.device_name) {
                        Ok((account, events)) => {
                            backends.push((Rc::downgrade(&account), events));
                            opened.insert(id, account);
                        }
                        Err(err) => {
                            tracing::error!(err = format!("{err:#}"), account = %id, "opening account failed");
                            failed.push(id);
                        }
                    }
                }
                if !config.accounts.is_empty() && opened.is_empty() {
                    anyhow::bail!("none of the accounts could be opened; see the log");
                }
                Some(dir)
            }
            Mode::Demo => {
                config.accounts.clear();
                for (account_config, store) in crate::demo::accounts()? {
                    let account = Account::new(account_config.id.clone(), store, None);
                    account.ready.set(true);
                    opened.insert(account_config.id.clone(), account);
                    config.accounts.push(account_config);
                }
                None
            }
        };

        let (ui_tx, ui_rx) = async_channel::unbounded();
        let ui = Ui::new(app, &config, ui_tx);

        let (tray_tx, tray_rx) = async_channel::unbounded();
        let tray = tray::spawn(tray_tx);

        let controller = Rc::new_cyclic(|weak_self| Self {
            weak_self: weak_self.clone(),
            app: app.clone(),
            config: RefCell::new(config),
            config_path: (mode == Mode::Live).then_some(config_path),
            data_dir,
            ui,
            settings: OnceCell::new(),
            accounts: RefCell::new(opened),
            linking: RefCell::default(),
            tray,
            refresh_scheduled: Cell::default(),
            chat_dirty: Cell::default(),
            _hold: app.hold(),
        });

        controller.install_actions();
        controller.push_accounts();
        glib::spawn_future_local(controller.clone().ui_loop(ui_rx));
        glib::spawn_future_local(controller.clone().tray_loop(tray_rx));
        glib::spawn_future_local(controller.clone().purge_loop());
        for (account, events) in backends {
            glib::spawn_future_local(controller.clone().backend_loop(account, events));
        }
        for id in failed {
            let name = controller.account_name(&id);
            controller
                .ui
                .show_error(&format!("Couldn't open {name}; see the log."));
        }
        if mode == Mode::Demo {
            controller.refresh();
            controller.ui.show_chats();
        } else if controller.config.borrow().accounts.is_empty() {
            // Shown or not as asked on the command line.
            controller.start_link();
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
            let Some(chat) = target.and_then(|t| t.str()).and_then(|s| s.parse().ok()) else {
                tracing::warn!(?target, "notification action with invalid chat");
                return;
            };
            this.ui.show();
            this.open(&chat);
        });
        self.app.add_action(&open);
    }

    // ---- accounts ----------------------------------------------------------

    fn account(&self, id: &AccountId) -> Option<Rc<Account>> {
        self.accounts.borrow().get(id).cloned()
    }

    /// The account, unless it was removed (possibly replaced by a new one
    /// with the same id) in the meantime.
    fn live_account(&self, account: &Weak<Account>) -> Option<Rc<Account>> {
        let account = account.upgrade()?;
        let accounts = self.accounts.borrow();
        let current = accounts.get(&account.id)?;
        Rc::ptr_eq(current, &account).then_some(account)
    }

    fn any_ready(&self) -> bool {
        self.accounts.borrow().values().any(|a| a.ready.get())
    }

    fn is_linking(&self, id: &AccountId) -> bool {
        self.linking.borrow().as_ref() == Some(id)
    }

    fn account_name(&self, id: &AccountId) -> String {
        let config = self.config.borrow();
        match config.account(id) {
            Some(account) => self.display_name(account),
            None => "the new account".to_owned(),
        }
    }

    fn display_name(&self, account: &AccountConfig) -> String {
        let name = account.name.trim();
        if !name.is_empty() {
            return name.to_owned();
        }
        self.account(&account.id)
            .and_then(|a| a.number.borrow().clone())
            .unwrap_or_else(|| format!("Account {}", account.id))
    }

    /// Tells the window and the settings about the configured accounts.
    fn push_accounts(&self) {
        let views = self.account_views();
        if let Some(settings) = self.settings.get() {
            settings.set_accounts(&views);
        }
        self.ui.set_accounts(views);
    }

    /// Shows an account's trouble without leaving the current page; with
    /// several accounts, says which one.
    fn account_banner(&self, account: &Account, text: &str) {
        if self.config.borrow().accounts.len() > 1 {
            let name = self.account_name(&account.id);
            self.ui.show_error(&format!("{name}: {text}"));
        } else {
            self.ui.show_error(text);
        }
    }

    /// Starts linking a new account and brings up the window for it.
    fn add_account(&self) {
        self.start_link();
        self.ui.show();
    }

    /// Starts linking a new account; the window follows along if shown.
    fn start_link(&self) {
        let Some(dir) = &self.data_dir else {
            self.ui
                .show_error("Adding accounts isn't possible in the demo.");
            return;
        };
        if self.linking.borrow().is_some() {
            self.ui
                .show_error("Finish linking the other account first.");
            return;
        }
        let (id, device_name) = {
            let config = self.config.borrow();
            let mut taken: Vec<AccountId> = config.accounts.iter().map(|a| a.id.clone()).collect();
            taken.extend(self.accounts.borrow().keys().cloned());
            (accounts::next_id(dir, &taken), config.device_name.clone())
        };
        let (account, events) = match open_account(dir, &id, &device_name) {
            Ok(opened) => opened,
            Err(err) => {
                tracing::error!(err = format!("{err:#}"), "adding an account failed");
                accounts::remove(dir, &id);
                self.ui.show_error("Couldn't add an account; see the log.");
                return;
            }
        };
        tracing::info!(account = %id, "linking a new account");
        self.accounts
            .borrow_mut()
            .insert(id.clone(), account.clone());
        *self.linking.borrow_mut() = Some(id);
        if let Some(this) = self.weak_self.upgrade() {
            glib::spawn_future_local(this.backend_loop(Rc::downgrade(&account), events));
        }
        self.ui.show_status(Status::Busy, "Requesting link code…");
    }

    /// Stops the account's backend and deletes its files and settings.
    fn remove_account(&self, id: &AccountId) {
        tracing::info!(account = %id, "removing account");
        let account = self.accounts.borrow_mut().remove(id);
        if let Some(account) = account {
            drop(account.backend.borrow_mut().take());
            let notified: Vec<ConversationId> = account.notified.borrow_mut().drain().collect();
            for conversation in notified {
                self.app
                    .withdraw_notification(&account.chat(conversation).to_string());
            }
        }
        if self.is_linking(id) {
            *self.linking.borrow_mut() = None;
        }
        if self.ui.current_chat().is_some_and(|c| &c.account == id) {
            self.ui.show_list();
        }
        let listed = {
            let mut config = self.config.borrow_mut();
            let before = config.accounts.len();
            config.accounts.retain(|a| &a.id != id);
            config.accounts.len() != before
        };
        if listed {
            self.save_config();
        }
        if let Some(dir) = &self.data_dir {
            accounts::remove(dir, id);
        }
        self.push_accounts();
        self.refresh();
    }

    /// Gives up on the account being linked. One that was never linked is
    /// removed; one that lost its registration stays until removed.
    fn abandon_link(&self) {
        let Some(id) = self.linking.borrow_mut().take() else {
            return;
        };
        if self.config.borrow().account(&id).is_none() {
            self.remove_account(&id);
        }
    }

    // ---- settings ----------------------------------------------------------

    fn open_settings(&self) {
        self.settings
            .get_or_init(|| {
                let weak = self.weak_self.clone();
                let views = self.account_views();
                let settings =
                    Settings::new(&self.app, &self.config.borrow(), &views, move |setting| {
                        if let Some(this) = weak.upgrade() {
                            this.change_setting(setting);
                        }
                    });
                self.ui.watch_focus(settings.window());
                settings
            })
            .window()
            .present();
    }

    fn account_views(&self) -> Vec<AccountView> {
        let config = self.config.borrow();
        config
            .accounts
            .iter()
            .map(|account| AccountView {
                id: account.id.clone(),
                name: self.display_name(account),
                color: account.color,
            })
            .collect()
    }

    fn hide_settings(&self) {
        if let Some(settings) = self.settings.get() {
            settings.window().set_visible(false);
        }
    }

    fn change_setting(&self, setting: Setting) {
        match setting {
            Setting::Opacity(opacity) => {
                if opacity == self.config.borrow().opacity {
                    return;
                }
                self.config.borrow_mut().opacity = opacity;
                self.ui.set_opacity(opacity);
            }
            Setting::HideOnFocusLoss(enabled) => {
                if enabled == self.config.borrow().hide_on_focus_loss {
                    return;
                }
                self.config.borrow_mut().hide_on_focus_loss = enabled;
                self.ui.set_hide_on_focus_loss(enabled);
            }
            Setting::AddAccount => {
                self.hide_settings();
                self.add_account();
                return;
            }
            Setting::RemoveAccount(id) => {
                self.remove_account(&id);
                if self.data_dir.is_some() && self.accounts.borrow().is_empty() {
                    // As on first start: nothing to show but a link code.
                    self.hide_settings();
                    self.add_account();
                }
                return;
            }
            Setting::AccountName(id, name) => {
                let name = name.trim().to_owned();
                let changed = match self.config.borrow_mut().account_mut(&id) {
                    Some(account) if account.name != name => {
                        account.name = name;
                        true
                    }
                    _ => false,
                };
                // Also puts the fallback back into a cleared entry.
                self.push_accounts();
                if !changed {
                    return;
                }
            }
            Setting::AccountColor(id, color) => {
                match self.config.borrow_mut().account_mut(&id) {
                    Some(account) if account.color != color => account.color = color,
                    _ => return,
                }
                self.push_accounts();
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
                UiEvent::Open(chat) => self.open(&chat),
                UiEvent::Seen(chat) => self.mark_seen(&chat),
                UiEvent::Send { chat, text } => {
                    glib::spawn_future_local(self.clone().send(chat, text));
                }
                UiEvent::Shown => {}
                UiEvent::RetryLink => {
                    let account = self
                        .linking
                        .borrow()
                        .as_ref()
                        .and_then(|id| self.account(id));
                    if let Some(account) = account
                        && let Some(backend) = account.backend.borrow().as_ref()
                    {
                        self.ui.show_status(Status::Busy, "Requesting link code…");
                        backend.retry_link();
                    }
                }
                UiEvent::CancelLink => {
                    if self.linking.borrow().is_some() && self.any_ready() {
                        self.abandon_link();
                        self.ui.show_list();
                    } else {
                        self.ui.hide();
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
            let accounts: Vec<Rc<Account>> = self.accounts.borrow().values().cloned().collect();
            for account in accounts {
                let purged = account.store.borrow_mut().purge_expired(now);
                match purged {
                    Ok(conversations) => {
                        for conversation in conversations {
                            self.withdraw(&account, &conversation);
                            self.touch(&account.chat(conversation));
                        }
                    }
                    Err(err) => {
                        tracing::error!(%err, account = %account.id, "purging disappearing messages failed");
                    }
                }
            }
            glib::timeout_future(PURGE_INTERVAL).await;
        }
    }

    /// Ends when the account is removed.
    async fn backend_loop(
        self: Rc<Self>,
        account: Weak<Account>,
        events: async_channel::Receiver<Event>,
    ) {
        while let Ok(event) = events.recv().await {
            let Some(account) = self.live_account(&account) else {
                break;
            };
            self.on_backend_event(&account, event);
        }
    }

    fn on_backend_event(&self, account: &Rc<Account>, event: Event) {
        let linking = self.is_linking(&account.id);
        match event {
            Event::LinkCode(uri) => {
                if !linking {
                    if self.linking.borrow().is_some() {
                        tracing::warn!(account = %account.id, "needs linking again, but another account is being linked");
                        return;
                    }
                    tracing::warn!(account = %account.id, "account is no longer linked, linking again");
                    *self.linking.borrow_mut() = Some(account.id.clone());
                }
                let others = self
                    .config
                    .borrow()
                    .accounts
                    .iter()
                    .any(|a| a.id != account.id);
                let title = if others {
                    "Link another account"
                } else {
                    "Link with your phone"
                };
                self.ui.show_link(&uri, title);
            }
            Event::LinkFailed(reason) => {
                tracing::info!(%reason, account = %account.id, "link code expired");
                if linking {
                    self.ui.set_link_state(LinkState::Expired);
                }
            }
            Event::Linked => {
                let added = {
                    let mut config = self.config.borrow_mut();
                    let added = config.account(&account.id).is_none();
                    if added {
                        config.add_account(account.id.clone());
                    }
                    added
                };
                if added {
                    self.save_config();
                    self.push_accounts();
                }
            }
            Event::History(status) => {
                if linking || !self.any_ready() {
                    self.ui.show_status(Status::Busy, &history_text(&status));
                } else if !account.history_announced.replace(true) {
                    self.account_banner(account, "importing message history…");
                }
            }
            Event::Ready { account: number } => self.on_ready(account, number),
            Event::Messages {
                events,
                origin,
                committed,
            } => {
                for event in events {
                    self.apply(account, event, origin);
                }
                if let Some(committed) = committed {
                    // Every event was applied (or logged as failed) above.
                    let _ = committed.send(());
                }
            }
            Event::Names(names) => {
                let result = account
                    .store
                    .borrow_mut()
                    .upsert_names(names.iter().map(|(id, name)| (id, name.as_str())));
                match result {
                    Ok(()) => {
                        tracing::info!(count = names.len(), account = %account.id, "updated contact names");
                        self.schedule_refresh();
                    }
                    Err(err) => tracing::error!(%err, "storing contact names failed"),
                }
            }
            Event::Connection(Connection::Online) => {
                tracing::info!(account = %account.id, "connected to Signal");
            }
            Event::Connection(Connection::Offline) => {
                self.account_banner(account, "Offline, reconnecting…");
            }
            Event::Fatal(err) => {
                if linking {
                    self.abandon_link();
                }
                if self.any_ready() {
                    self.account_banner(account, &format!("Signal stopped working: {err}"));
                    if linking {
                        self.ui.show_list();
                    }
                } else {
                    self.ui
                        .show_status(Status::Failed, &format!("Signal stopped working:\n{err}"));
                }
            }
        }
    }

    fn on_ready(&self, account: &Account, number: String) {
        tracing::info!(account = %account.id, %number, "signal ready");
        account.ready.set(true);
        *account.number.borrow_mut() = Some(number.clone());
        // The number is a good first name, and can be edited from there.
        let named = match self.config.borrow_mut().account_mut(&account.id) {
            Some(config) if config.name.trim().is_empty() => {
                config.name = number;
                true
            }
            _ => false,
        };
        if named {
            self.save_config();
        }
        if self.is_linking(&account.id) {
            *self.linking.borrow_mut() = None;
        }
        self.push_accounts();
        self.refresh();
        // Linking another account keeps the window.
        if self.linking.borrow().is_none() {
            self.ui.show_chats();
        }
    }

    // ---- actions -----------------------------------------------------------

    fn open(&self, chat: &ChatRef) {
        let Some(account) = self.account(&chat.account) else {
            tracing::warn!(%chat, "opening a chat of an unknown account");
            return;
        };
        match self.message_views(&account, &chat.conversation) {
            Ok(views) => self.ui.show_chat(chat, &views),
            Err(err) => {
                tracing::error!(%err, %chat, "loading messages failed");
                self.ui.show_error("Could not load messages");
            }
        }
    }

    fn withdraw(&self, account: &Account, conversation: &ConversationId) {
        account.notified.borrow_mut().remove(conversation);
        self.app
            .withdraw_notification(&account.chat(conversation.clone()).to_string());
    }

    /// Marks a conversation read locally and tells the senders.
    fn mark_seen(&self, chat: &ChatRef) {
        let Some(account) = self.account(&chat.account) else {
            return;
        };
        self.withdraw(&account, &chat.conversation);
        let receipts = match account
            .store
            .borrow_mut()
            .mark_conversation_read(&chat.conversation)
        {
            Ok(receipts) => receipts,
            Err(err) => {
                tracing::error!(%err, %chat, "marking read failed");
                return;
            }
        };
        if receipts.is_empty() {
            return;
        }
        self.schedule_refresh();
        let backend = account.backend.borrow();
        let Some(backend) = backend.as_ref() else {
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

    async fn send(self: Rc<Self>, chat: ChatRef, text: String) {
        let timestamp = Timestamp(glib::real_time() / 1000);
        let result: Result<(), SendError> = match self.account(&chat.account) {
            Some(account) => {
                let sending = account
                    .backend
                    .borrow()
                    .as_ref()
                    .map(|b| b.send_text(&chat.conversation, &text, timestamp));
                match sending {
                    Some(sending) => sending.await,
                    // Demo mode.
                    None => Ok(()),
                }
            }
            None => Err(SendError::Offline),
        };
        // The account may have been removed while sending.
        let account = match (result, self.account(&chat.account)) {
            (Ok(()), Some(account)) => account,
            (Ok(()), None) => return,
            (Err(err), _) => {
                tracing::warn!(%err, %chat, "sending failed");
                self.ui.show_error(&format!("Not sent: {err}"));
                self.ui.restore_draft(&chat, &text);
                self.chat_dirty.set(true);
                self.schedule_refresh();
                return;
            }
        };
        // presage applies the conversation's timer when sending; mirror it.
        let expires_in = account
            .store
            .borrow()
            .expiration_timer(&chat.conversation)
            .unwrap_or_else(|err| {
                tracing::error!(%err, "reading expiration timer failed");
                None
            });
        let message = NewMessage {
            conversation: chat.conversation,
            author: Author::Me,
            author_name: None,
            timestamp,
            body: text,
            expires_in,
        };
        if let Err(err) = account
            .store
            .borrow_mut()
            .insert_message(&message, ReadState::Read)
        {
            tracing::error!(%err, "storing sent message failed");
        }
        self.chat_dirty.set(true);
        self.schedule_refresh();
    }

    /// Applies one event from Signal to the account's store and the UI.
    fn apply(&self, account: &Account, event: SignalEvent, origin: Origin) {
        if let SignalEvent::Message(message) = &event {
            if let Err(err) = self.apply_message(account, message, origin) {
                tracing::error!(%err, kind = event.kind(), "applying signal event failed");
            }
            return;
        }
        // The store borrow must end before the UI is touched.
        let affected = {
            let mut store = account.store.borrow_mut();
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
        for conversation in affected {
            if matches!(
                event,
                SignalEvent::ReadElsewhere { .. } | SignalEvent::Delete { .. }
            ) {
                self.withdraw(account, &conversation);
            }
            self.touch(&account.chat(conversation));
        }
    }

    fn apply_message(
        &self,
        account: &Account,
        message: &NewMessage,
        origin: Origin,
    ) -> anyhow::Result<()> {
        let read = match (&message.author, origin) {
            (Author::Peer(_), Origin::Live) => ReadState::Unread,
            // History was read on the phone, or it would not be history.
            (Author::Peer(_), Origin::Stored) | (Author::Me, _) => ReadState::Read,
        };
        if !account.store.borrow_mut().insert_message(message, read)? {
            return Ok(());
        }
        let chat = account.chat(message.conversation.clone());
        self.touch(&chat);
        if read == ReadState::Unread {
            if self.ui.is_viewing(&chat) {
                self.mark_seen(&chat);
            } else if !self.ui.is_focused() {
                // While the window has focus, the unread badge is enough.
                self.notify(account, message);
            }
        } else {
            // Replying from the phone means the chat was read there.
            self.withdraw(account, &message.conversation);
        }
        Ok(())
    }

    /// Records that a chat changed and schedules a UI refresh.
    fn touch(&self, chat: &ChatRef) {
        if self.ui.current_chat().as_ref() == Some(chat) {
            self.chat_dirty.set(true);
        }
        self.schedule_refresh();
    }

    fn notify(&self, account: &Account, message: &NewMessage) {
        if !self.config.borrow().notifications {
            return;
        }
        let title = match account.store.borrow().conversation(&message.conversation) {
            Ok(Some(summary)) => summary.name,
            _ => "Signal".to_owned(),
        };
        let body = match (&message.conversation, &message.author) {
            (ConversationId::Group(_), Author::Peer(peer)) => {
                format!(
                    "{}: {}",
                    peer_display_name(
                        &account.store.borrow(),
                        peer,
                        message.author_name.as_deref()
                    ),
                    message.body
                )
            }
            _ => message.body.clone(),
        };
        let chat = account.chat(message.conversation.clone()).to_string();
        let notification = gio::Notification::new(&title);
        notification.set_body(Some(&body));
        notification.set_default_action_and_target_value(
            &format!("app.{OPEN_CONVERSATION_ACTION}"),
            Some(&chat.to_variant()),
        );
        self.app.send_notification(Some(&chat), &notification);
        account
            .notified
            .borrow_mut()
            .insert(message.conversation.clone());
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
        let mut per_account = Vec::new();
        let mut unread = 0;
        for account in self.accounts.borrow().values() {
            let store = account.store.borrow();
            match store.conversations() {
                Ok(conversations) => per_account.push((account.id.clone(), conversations)),
                Err(err) => {
                    tracing::error!(%err, account = %account.id, "loading conversations failed");
                }
            }
            match store.total_unread() {
                Ok(count) => unread += count,
                Err(err) => tracing::error!(%err, account = %account.id, "counting unread failed"),
            }
        }
        self.ui.set_conversations(merge_chats(per_account));
        if let Some(tray) = &self.tray {
            tray.set_unread(unread);
        }
        if self.chat_dirty.replace(false)
            && let Some(current) = self.ui.current_chat()
            && let Some(account) = self.account(&current.account)
        {
            match self.message_views(&account, &current.conversation) {
                Ok(views) => self.ui.refresh_chat(&current, &views),
                Err(err) => tracing::error!(%err, "reloading chat failed"),
            }
        }
    }

    fn message_views(
        &self,
        account: &Account,
        id: &ConversationId,
    ) -> anyhow::Result<Vec<MessageView>> {
        let store = account.store.borrow();
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
                                    peer_display_name(&store, peer, m.author_name.as_deref())
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
}

/// The chats of all accounts in one list, ordered like each account's:
/// most recently active first, contacts without messages last by name.
fn merge_chats(per_account: Vec<(AccountId, Vec<ConversationSummary>)>) -> Vec<ListedChat> {
    let mut chats: Vec<ListedChat> = per_account
        .into_iter()
        .flat_map(|(account, conversations)| {
            conversations.into_iter().map(move |summary| ListedChat {
                account: account.clone(),
                summary,
            })
        })
        .collect();
    chats.sort_by_cached_key(|chat| {
        (
            chat.summary.last_activity.is_none(),
            Reverse(chat.summary.last_activity),
            chat.summary.name.to_lowercase(),
        )
    });
    chats
}

fn peer_display_name(store: &Store, peer: &PeerId, advertised: Option<&str>) -> String {
    store
        .peer_name(peer)
        .ok()
        .flatten()
        .or_else(|| advertised.map(str::to_owned))
        .unwrap_or_else(|| short_id(peer))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(name: &str, last: Option<i64>) -> ConversationSummary {
        ConversationSummary {
            id: ConversationId::Direct(PeerId(name.to_owned())),
            name: name.to_owned(),
            last_activity: last.map(Timestamp),
            last_preview: None,
            unread: 0,
        }
    }

    #[test]
    fn chats_of_all_accounts_merge_by_activity() {
        let account = |id: &str| AccountId::try_from(id.to_owned()).expect("valid account id");
        let merged = merge_chats(vec![
            (
                account("1"),
                vec![
                    summary("a", Some(30)),
                    summary("b", Some(10)),
                    summary("Zed", None),
                ],
            ),
            (
                account("2"),
                vec![summary("a", Some(20)), summary("ann", None)],
            ),
        ]);
        let order: Vec<(&str, &str)> = merged
            .iter()
            .map(|c| (c.account.as_str(), c.summary.name.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                ("1", "a"),
                ("2", "a"),
                ("1", "b"),
                ("2", "ann"),
                ("1", "Zed")
            ],
            "the same contact in two accounts is two chats; idle ones last by name"
        );
    }
}
