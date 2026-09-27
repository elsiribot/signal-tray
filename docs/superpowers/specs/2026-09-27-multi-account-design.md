# Multi-account support

Run several Signal accounts side by side, give each a color, and show all
their chats in one unified list.

## Decisions

- Accounts are added, renamed, recolored and removed in the settings window.
- The same person or group in two accounts is two separate rows. A chat
  always belongs to exactly one account and replies go out from it.
- The account color is shown as a stripe on chat-list rows, in the chat
  header, and as the background of outgoing bubbles. Notifications are not
  marked.
- Storage is one directory per account; `Store` and `Backend` stay
  single-account.

## Storage layout

```
~/.local/share/signal-tray/            (0700)
  accounts/<id>/presage.sqlite         presage keys, sessions, contacts, groups
  accounts/<id>/messages.sqlite        the app's message history
  accounts/<id>/history-import-pending marker while Link & Sync is unfinished
```

`<id>` is a local identifier, never shown: the smallest positive integer
whose directory does not exist yet. Before linking there is no phone number
or ACI to derive it from.

### Migration

On start, if `accounts` in the config is empty and `presage.sqlite` or
`messages.sqlite` exists directly in the data directory (the single-account
layout), the app moves `presage.sqlite*`, `messages.sqlite*` (including
`-wal`/`-shm`) and `history-import-pending` into `accounts/1/`. Then it adds
account `1` with the first palette color and no name to the config and saves
it. Each file is moved only if present, so an interrupted migration completes
on the next start.

### Abandoned link attempts

An account is written to the config only once linking succeeds. On start,
every directory under `accounts/` whose name is not a configured id is
deleted: it can only be a link attempt that never completed.

## Config

```toml
device_name = "signal-tray"

[[accounts]]
id = "1"
name = "Personal"   # optional; empty/missing = the phone number once known
color = "#3a76f0"
```

- `accounts` defaults to empty. `deny_unknown_fields` applies to the account
  table too.
- `color` is `#rrggbb`. Invalid values are rejected when loading. New
  accounts get the next color of a fixed palette that is not in use yet,
  cycling when all are taken.
- The display name of an account is `name` if set, otherwise the phone
  number reported by `Event::Ready`, otherwise `Account <id>`. The phone
  number is written to `name` when the account first becomes ready and
  `name` is empty, so it can be edited from there.

## Identity

- `model::AccountId(String)`.
- `model::ChatRef { account: AccountId, conversation: ConversationId }`,
  with the stable string form `<account>/<conversation>`, e.g.
  `1/d:a1a1…` or `2/g:abc==`, and a `FromStr` that inverts it. The
  account id never contains `/`.
- `ChatRef` replaces `ConversationId` everywhere the UI and the controller
  talk: `UiEvent::{Open, Seen, Send}`, the current chat, drafts,
  `is_viewing`, `refresh_chat`, `restore_draft`. It is also used as the
  notification id and as the `open-conversation` action target, so
  notifications of two accounts never collide.

## Backend (`signal/mod.rs`)

- New `Event::Linked`, emitted right after `link()` returns (before the
  history import).
- Nothing else changes. Each account gets its own `Backend`. Dropping it
  stops its thread once the thread next polls its command channel; the
  thread name becomes `signal-<id>` for readable logs.

## Controller (`app.rs`)

```rust
struct Account {
    id: AccountId,
    store: RefCell<Store>,
    /// `None` in demo mode.
    backend: Option<Backend>,
    /// Phone number, once `Event::Ready` arrived.
    number: RefCell<Option<String>>,
    ready: Cell<bool>,
}
```

- `Controller.accounts: RefCell<BTreeMap<AccountId, Rc<Account>>>`.
- `Controller.linking: RefCell<Option<AccountId>>`: the account whose link
  flow owns the window. At most one link is in progress at a time.
- One `backend_loop` per account, each knowing its `AccountId`. Events are
  handled as today, per account, with these differences:
  - `LinkCode`, `LinkFailed`: only shown if this account is `linking`.
  - `Linked`: append the account to the config and save.
  - `History(..)`: status page if this account is `linking` or no account
    is ready yet; otherwise a banner `"<name>: importing history…"` (once,
    not per progress step).
  - `Ready`: mark ready; store the number (and fill in `name` if empty);
    clear `linking` if it was this account; `refresh()` and `show_chats()`.
  - `Connection(Offline)`, `Fatal`: banner prefixed with the account name
    once any account is ready; before that, as today (banner / status page).
- `refresh()` collects `conversations()` of every account and merges them
  into one list, most recent first, idle contacts last, idle ones ordered by
  name (the same order `Store::conversations` uses). Unread for the tray is
  the sum of every store's `total_unread()`. The merge is a free function so
  it can be unit tested.
- `purge_loop` purges every store.
- `apply`, `apply_message`, `mark_seen`, `send`, `message_views`, `notify`,
  `touch` take the `Account` (or a `ChatRef`) instead of using the single
  store and backend.
- If one account's store fails to open or its backend fails to spawn at
  start, that account is skipped with an error log and a banner, and the
  others run. Start-up fails only if there are configured accounts and none
  could be opened.

### Adding an account

`Setting::AddAccount` (from settings):

1. Ignored with a banner if a link is already in progress.
2. Allocate the next id, create its directory, spawn the backend, set
   `linking`, show the main window with the status page ("Requesting link
   code…"). The link page says "Link another account" when other accounts
   exist, otherwise "Link with your phone" as today.
3. Escape on the link page, while `linking` is set and other accounts exist,
   cancels: drop the backend, delete the directory, clear `linking`, go back
   to the list. With no other account, Escape hides as today.
4. From `Linked` on, the flow is the one of the first account: history
   status page, then the unified list on `Ready`.

### Removing an account

`Setting::RemoveAccount(id)` comes after a confirmation dialog in the
settings window ("Remove <name>? Its messages on this computer are deleted.
Also remove the linked device on your phone."). The controller:

1. Removes the account from the map (dropping its backend).
2. Withdraws the notifications of its chats. The controller keeps the set of
   `ChatRef`s it has notified about per account, for this.
3. If the open chat belongs to it, goes back to the list.
4. Deletes the account directory, removes the account from the config,
   saves, and refreshes.

Removing the last account leaves the app with none. The window then starts
a new link flow, as on first start.

### Renaming and recoloring

`Setting::AccountName(id, String)` and `Setting::AccountColor(id, Color)`
update the config, save it, and push the new accounts to the UI
(`Ui::set_accounts`), which re-renders the list and the open chat header.

## UI (`ui/`)

- `Ui::set_accounts(&[AccountView])` with
  `AccountView { id, name, color }`. It regenerates a dedicated
  `CssProvider` with one block per account:

  ```css
  .acct-1 .stripe, .chat-header.acct-1 { … }
  .bubble.outgoing.acct-1 { background: #xxxxxx; color: <white|black>; }
  ```

  The text color is black if the color's relative luminance is above 0.5,
  otherwise white. Regenerating a provider follows the existing
  `opacity_css` pattern; CSS custom properties would need GTK 4.16 and the
  project targets 4.14. Account ids are digits only, so they are safe in
  class names.
- `set_conversations` takes `Vec<(AccountId, ConversationSummary)>`. Rows,
  `visible`, search and Alt+1…9 work on the merged list; search matches
  names in every account.
- **Chat list rows:** a 3 px wide `stripe` widget on the left edge, in the
  account's color. It is hidden when there is only one account, so
  single-account use looks as before.
- **Chat header:** a 2 px bottom border in the account color, and the
  account name in dim text after the title (`Alice · Work`), both only with
  more than one account.
- **Outgoing bubbles** (sent and pending): class `acct-<id>`, colored by
  the provider. With one account its color is still applied, so the color
  choice is visible in single-account use too.

## Settings window (`ui/settings.rs`)

A new "Accounts" section above "Behaviour":

- One row per account: `gtk::ColorDialogButton` (no alpha), a `gtk::Entry`
  with the name (placeholder: phone number or `Account <id>`; committed on
  Enter and focus-out), and a "Remove…" button that opens the confirmation
  `gtk::AlertDialog`.
- An "Add account…" button below the rows.
- The window is built once and reused, so the controller calls
  `settings::set_accounts` to rebuild the rows whenever accounts are added,
  removed, or become ready (name placeholder).
- `Setting` stops being `Copy` (it carries a `String`).

## Tray

The unread count is the total over all accounts. No menu changes.

## Demo mode

`demo::populate` fills two in-memory accounts: `1` "Personal" (blue) with
Alice, Bob, Family and Carol, and `2` "Work" (orange) with Dave and a "Team"
group, one of them unread. Nothing is written to disk and settings changes
are not saved, as today. In demo mode "Add account…" shows a banner that it
is unavailable, and removing works in memory only.

## Testing

Unit tests:

- config: `[[accounts]]` round-trips through save/load; unknown account keys
  and malformed colors are rejected; palette allocation skips used colors.
- `ChatRef` string round trip, including group ids containing `/`.
- account id allocation skips ids with a leftover directory.
- migration of the single-account layout into `accounts/1/` in a temp
  directory, including `-wal`/`-shm` files, and that it is a no-op when
  accounts are configured.
- merge order of conversations across accounts.
- text color choice for light and dark account colors.

Existing store and backend tests stay as they are. Manual check with
`just demo` (two accounts, stripes, header, bubble colors, recoloring and
renaming live, removing), and `just run` against a real account to check the
migration of an existing install.

## Docs

README: multiple accounts section (adding, removing, colors), new file
layout, `[[accounts]]` in the config reference, and "multiple accounts"
removed from "Not in v1".

## Out of scope

- Merging the same contact across accounts.
- Choosing which account to start a new conversation from (search shows
  each account's contacts as separate rows, so this falls out naturally).
- Per-account notification settings.
