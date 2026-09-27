//! Local message history. signal-cli does not keep messages, so everything
//! shown in the UI lives here.

use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::model::{Author, ConversationId, DeliveryStatus, NewMessage, PeerId, Timestamp};

/// How many messages of a conversation are loaded into the chat view.
pub const CHAT_HISTORY_LIMIT: usize = 300;

/// Column value for [`Author::Me`]. SQLite treats NULLs as distinct in
/// UNIQUE constraints, so a real value is needed for de-duplication.
const AUTHOR_ME: &str = "";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS conversations (
    id           TEXT PRIMARY KEY,
    name         TEXT,
    last_ts      INTEGER,
    last_preview TEXT,
    unread       INTEGER NOT NULL DEFAULT 0,
    -- disappearing-message timer in seconds, NULL when off
    expires_in   INTEGER
);
CREATE TABLE IF NOT EXISTS messages (
    id           INTEGER PRIMARY KEY,
    conversation TEXT NOT NULL,
    ts           INTEGER NOT NULL,
    author       TEXT NOT NULL,
    author_name  TEXT,
    body         TEXT NOT NULL,
    status       INTEGER,
    is_read      INTEGER NOT NULL,
    edited       INTEGER NOT NULL DEFAULT 0,
    deleted      INTEGER NOT NULL DEFAULT 0,
    -- ms since epoch after which the message is purged, NULL for never
    expires_at   INTEGER,
    UNIQUE (conversation, ts, author)
);
CREATE INDEX IF NOT EXISTS messages_by_conversation ON messages (conversation, ts);
CREATE INDEX IF NOT EXISTS messages_by_ts ON messages (ts);
CREATE INDEX IF NOT EXISTS messages_by_expiry ON messages (expires_at)
    WHERE expires_at IS NOT NULL;
";

#[derive(Clone, Debug)]
pub struct ConversationSummary {
    pub id: ConversationId,
    pub name: String,
    pub last_activity: Option<Timestamp>,
    pub last_preview: Option<String>,
    pub unread: u32,
}

#[derive(Clone, Debug)]
pub struct StoredMessage {
    pub author: Author,
    pub author_name: Option<String>,
    pub timestamp: Timestamp,
    pub body: String,
    /// Only set for our own messages.
    pub status: Option<DeliveryStatus>,
    pub edited: bool,
    pub deleted: bool,
}

/// Whether a newly stored message counts towards the unread badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadState {
    Read,
    Unread,
}

/// A received message we have not yet sent a read receipt for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingReceipt {
    pub sender: PeerId,
    pub timestamp: Timestamp,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Self> {
        conn.execute_batch(SCHEMA).context("creating schema")?;
        Ok(Self { conn })
    }

    /// Records display names of contacts and groups without creating
    /// visible conversations.
    pub fn upsert_names<'a>(
        &mut self,
        names: impl IntoIterator<Item = (&'a ConversationId, &'a str)>,
    ) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO conversations (id, name) VALUES (?1, ?2)
                 ON CONFLICT (id) DO UPDATE SET name = excluded.name",
            )?;
            for (id, name) in names {
                stmt.execute(params![id.to_string(), name])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Stores a message and bumps its conversation. Returns `false` if the
    /// message was already known (signal-cli can redeliver after restarts).
    pub fn insert_message(&mut self, msg: &NewMessage, read: ReadState) -> anyhow::Result<bool> {
        let tx = self.conn.transaction()?;
        let conversation = msg.conversation.to_string();
        let status = match msg.author {
            Author::Me => Some(DeliveryStatus::Sent as i64),
            Author::Peer(_) => None,
        };
        // Signal starts a recipient's timer when the message is read; starting
        // it at send time errs on the side of forgetting early.
        let expires_at = msg
            .expires_in
            .map(|d| msg.timestamp.0.saturating_add(duration_millis(d)));
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO messages
                 (conversation, ts, author, author_name, body, status, is_read, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                conversation,
                msg.timestamp.0,
                author_column(&msg.author),
                msg.author_name,
                msg.body,
                status,
                read == ReadState::Read,
                expires_at,
            ],
        )? == 1;
        if inserted {
            let unread_delta = i64::from(read == ReadState::Unread);
            // Every message carries the conversation's current timer, so the
            // newest one also updates it. (All SET expressions see the old row.)
            tx.execute(
                "INSERT INTO conversations (id, last_ts, last_preview, unread, expires_in)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (id) DO UPDATE SET
                     last_ts = MAX(COALESCE(last_ts, 0), excluded.last_ts),
                     last_preview = CASE WHEN excluded.last_ts >= COALESCE(last_ts, 0)
                                         THEN excluded.last_preview ELSE last_preview END,
                     expires_in = CASE WHEN excluded.last_ts >= COALESCE(last_ts, 0)
                                       THEN excluded.expires_in ELSE expires_in END,
                     unread = unread + excluded.unread",
                params![
                    conversation,
                    msg.timestamp.0,
                    preview(&msg.body),
                    unread_delta,
                    msg.expires_in.map(duration_secs),
                ],
            )?;
            // A peer we only knew by number may have told us their name.
            if let (Author::Peer(_), ConversationId::Direct(_), Some(name)) =
                (&msg.author, &msg.conversation, &msg.author_name)
            {
                tx.execute(
                    "UPDATE conversations SET name = ?2 WHERE id = ?1 AND name IS NULL",
                    params![conversation, name],
                )?;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    pub fn set_expiration_timer(
        &self,
        conversation: &ConversationId,
        expires_in: Option<Duration>,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO conversations (id, expires_in) VALUES (?1, ?2)
             ON CONFLICT (id) DO UPDATE SET expires_in = excluded.expires_in",
            params![conversation.to_string(), expires_in.map(duration_secs)],
        )?;
        Ok(())
    }

    /// The disappearing-message timer to apply to messages we send.
    pub fn expiration_timer(
        &self,
        conversation: &ConversationId,
    ) -> anyhow::Result<Option<Duration>> {
        let secs: Option<i64> = self
            .conn
            .query_row(
                "SELECT expires_in FROM conversations WHERE id = ?1",
                params![conversation.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(secs
            .and_then(|s| u64::try_from(s).ok())
            .filter(|s| *s > 0)
            .map(Duration::from_secs))
    }

    /// Deletes disappearing messages whose time is up. Returns the affected
    /// conversations.
    pub fn purge_expired(&mut self, now: Timestamp) -> anyhow::Result<Vec<ConversationId>> {
        let tx = self.conn.transaction()?;
        let mut affected: Vec<String> = {
            let mut stmt =
                tx.prepare("DELETE FROM messages WHERE expires_at <= ?1 RETURNING conversation")?;
            stmt.query_map(params![now.0], |row| row.get(0))?
                .collect::<Result<_, _>>()?
        };
        affected.sort();
        affected.dedup();
        for conversation in &affected {
            // The conversation keeps its place in the list, but must not
            // leak the purged text through its preview.
            recompute_summary(&tx, conversation)?;
        }
        tx.commit()?;
        affected.into_iter().map(|c| Ok(c.parse()?)).collect()
    }

    /// Returns whether a matching message existed.
    pub fn edit_message(
        &mut self,
        conversation: &ConversationId,
        author: &Author,
        target: Timestamp,
        body: &str,
    ) -> anyhow::Result<bool> {
        let tx = self.conn.transaction()?;
        let id = conversation.to_string();
        let found = tx.execute(
            "UPDATE messages SET body = ?4, edited = 1
             WHERE conversation = ?1 AND author = ?2 AND ts = ?3 AND deleted = 0",
            params![id, author_column(author), target.0, body],
        )? > 0;
        if found {
            recompute_summary(&tx, &id)?;
        }
        tx.commit()?;
        Ok(found)
    }

    /// Returns whether a matching message existed. The message is kept as a
    /// "deleted" placeholder and no longer counts as unread.
    pub fn delete_message(
        &mut self,
        conversation: &ConversationId,
        author: &Author,
        target: Timestamp,
    ) -> anyhow::Result<bool> {
        let tx = self.conn.transaction()?;
        let id = conversation.to_string();
        let found = tx.execute(
            "UPDATE messages SET body = '', deleted = 1, is_read = 1
             WHERE conversation = ?1 AND author = ?2 AND ts = ?3",
            params![id, author_column(author), target.0],
        )? > 0;
        if found {
            recompute_summary(&tx, &id)?;
        }
        tx.commit()?;
        Ok(found)
    }

    /// Marks everything in a conversation as read and returns the messages a
    /// read receipt should be sent for.
    pub fn mark_conversation_read(
        &mut self,
        conversation: &ConversationId,
    ) -> anyhow::Result<Vec<PendingReceipt>> {
        let tx = self.conn.transaction()?;
        let id = conversation.to_string();
        let pending = {
            let mut stmt = tx.prepare(
                "SELECT author, ts FROM messages
                 WHERE conversation = ?1 AND is_read = 0 AND author != ?2",
            )?;
            stmt.query_map(params![id, AUTHOR_ME], |row| {
                Ok(PendingReceipt {
                    sender: PeerId(row.get(0)?),
                    timestamp: Timestamp(row.get(1)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        tx.execute(
            "UPDATE messages SET is_read = 1 WHERE conversation = ?1 AND is_read = 0",
            params![id],
        )?;
        tx.execute(
            "UPDATE conversations SET unread = 0 WHERE id = ?1",
            params![id],
        )?;
        tx.commit()?;
        Ok(pending)
    }

    /// Applies a "read on another device" sync. Returns the conversations
    /// whose unread count changed.
    pub fn mark_read_elsewhere(
        &mut self,
        sender: &PeerId,
        timestamp: Timestamp,
    ) -> anyhow::Result<Vec<ConversationId>> {
        let tx = self.conn.transaction()?;
        let mut affected: Vec<String> = {
            let mut stmt = tx.prepare(
                "UPDATE messages SET is_read = 1
                 WHERE author = ?1 AND ts = ?2 AND is_read = 0
                 RETURNING conversation",
            )?;
            stmt.query_map(params![sender.0, timestamp.0], |row| row.get(0))?
                .collect::<Result<_, _>>()?
        };
        affected.sort();
        affected.dedup();
        for conversation in &affected {
            recompute_summary(&tx, conversation)?;
        }
        tx.commit()?;
        affected.into_iter().map(|c| Ok(c.parse()?)).collect()
    }

    /// Upgrades the status of our own messages. Never downgrades.
    pub fn apply_receipt(
        &self,
        status: DeliveryStatus,
        timestamps: &[Timestamp],
    ) -> anyhow::Result<Vec<ConversationId>> {
        let mut stmt = self.conn.prepare_cached(
            "UPDATE messages SET status = ?1
             WHERE author = ?2 AND ts = ?3 AND status < ?1
             RETURNING conversation",
        )?;
        let mut affected = Vec::new();
        for ts in timestamps {
            let rows = stmt.query_map(params![status as i64, AUTHOR_ME, ts.0], |row| {
                row.get::<_, String>(0)
            })?;
            for conversation in rows {
                let conversation: ConversationId = conversation?.parse()?;
                if !affected.contains(&conversation) {
                    affected.push(conversation);
                }
            }
        }
        Ok(affected)
    }

    /// All known conversations, most recently active first. Contacts and
    /// groups without messages sort last.
    pub fn conversations(&self) -> anyhow::Result<Vec<ConversationSummary>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, name, last_ts, last_preview, unread FROM conversations
             ORDER BY last_ts IS NULL, last_ts DESC, name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, u32>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (id, name, last_ts, last_preview, unread) = row?;
            let id: ConversationId = id.parse()?;
            Ok(ConversationSummary {
                name: name.unwrap_or_else(|| fallback_name(&id)),
                id,
                last_activity: last_ts.map(Timestamp),
                last_preview,
                unread,
            })
        })
        .collect()
    }

    pub fn conversation(&self, id: &ConversationId) -> anyhow::Result<Option<ConversationSummary>> {
        Ok(self.conversations()?.into_iter().find(|c| &c.id == id))
    }

    /// The most recent [`CHAT_HISTORY_LIMIT`] messages, oldest first.
    pub fn messages(&self, conversation: &ConversationId) -> anyhow::Result<Vec<StoredMessage>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT author, author_name, ts, body, status, edited, deleted FROM (
                 SELECT * FROM messages WHERE conversation = ?1
                 ORDER BY ts DESC LIMIT ?2
             ) ORDER BY ts ASC",
        )?;
        let limit = i64::try_from(CHAT_HISTORY_LIMIT).expect("history limit fits in i64");
        let rows = stmt.query_map(params![conversation.to_string(), limit], |row| {
            let author: String = row.get(0)?;
            Ok(StoredMessage {
                author: if author == AUTHOR_ME {
                    Author::Me
                } else {
                    Author::Peer(PeerId(author))
                },
                author_name: row.get(1)?,
                timestamp: Timestamp(row.get(2)?),
                body: row.get(3)?,
                status: row
                    .get::<_, Option<i64>>(4)?
                    .and_then(DeliveryStatus::from_db),
                edited: row.get(5)?,
                deleted: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn total_unread(&self) -> anyhow::Result<u32> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(unread), 0) FROM conversations",
            [],
            |row| row.get(0),
        )?)
    }

    /// Display name for a peer, as known from contacts.
    pub fn peer_name(&self, peer: &PeerId) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT name FROM conversations WHERE id = ?1",
                params![ConversationId::Direct(peer.clone()).to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }
}

fn author_column(author: &Author) -> &str {
    match author {
        Author::Me => AUTHOR_ME,
        Author::Peer(peer) => &peer.0,
    }
}

/// Re-derives a conversation's preview and unread count from its messages,
/// after messages changed underneath them.
fn recompute_summary(conn: &Connection, conversation: &str) -> rusqlite::Result<()> {
    let latest: Option<String> = conn
        .query_row(
            "SELECT body FROM messages WHERE conversation = ?1 AND deleted = 0
             ORDER BY ts DESC LIMIT 1",
            params![conversation],
            |row| row.get(0),
        )
        .optional()?;
    conn.execute(
        "UPDATE conversations SET
             last_preview = ?2,
             unread = (SELECT COUNT(*) FROM messages WHERE conversation = ?1 AND is_read = 0)
         WHERE id = ?1",
        params![conversation, latest.as_deref().map(preview)],
    )?;
    Ok(())
}

fn duration_secs(d: Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

fn duration_millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

fn preview(body: &str) -> String {
    const MAX_CHARS: usize = 80;
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(MAX_CHARS) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

fn fallback_name(id: &ConversationId) -> String {
    match id {
        ConversationId::Direct(peer) => peer.0.clone(),
        ConversationId::Group(_) => "Unnamed group".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(s: &str) -> PeerId {
        PeerId(s.to_owned())
    }

    fn incoming(conv: &ConversationId, from: &str, ts: i64, body: &str) -> NewMessage {
        NewMessage {
            conversation: conv.clone(),
            author: Author::Peer(peer(from)),
            author_name: Some(format!("{from}-profile")),
            timestamp: Timestamp(ts),
            body: body.to_owned(),
            expires_in: None,
        }
    }

    #[test]
    fn insert_is_idempotent_and_tracks_unread() {
        let mut store = Store::open_in_memory().expect("in-memory db");
        let alice = ConversationId::Direct(peer("alice"));
        let msg = incoming(&alice, "alice", 10, "hello\nworld");

        assert!(
            store
                .insert_message(&msg, ReadState::Unread)
                .expect("insert")
        );
        assert!(
            !store
                .insert_message(&msg, ReadState::Unread)
                .expect("insert"),
            "duplicate must be ignored"
        );

        let convs = store.conversations().expect("list");
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].unread, 1, "duplicate must not bump unread");
        assert_eq!(convs[0].last_preview.as_deref(), Some("hello world"));
        assert_eq!(
            convs[0].name, "alice-profile",
            "profile name fills in unknown name"
        );

        let receipts = store.mark_conversation_read(&alice).expect("mark read");
        assert_eq!(
            receipts,
            vec![PendingReceipt {
                sender: peer("alice"),
                timestamp: Timestamp(10)
            }]
        );
        assert_eq!(store.total_unread().expect("unread"), 0);
        assert!(
            store
                .mark_conversation_read(&alice)
                .expect("mark read")
                .is_empty()
        );
    }

    #[test]
    fn contact_names_win_over_profile_names() {
        let mut store = Store::open_in_memory().expect("in-memory db");
        let alice = ConversationId::Direct(peer("alice"));
        store
            .upsert_names([(&alice, "Alice Contact")])
            .expect("upsert");
        store
            .insert_message(&incoming(&alice, "alice", 1, "hi"), ReadState::Read)
            .expect("insert");
        assert_eq!(
            store.conversations().expect("list")[0].name,
            "Alice Contact"
        );
    }

    #[test]
    fn ordering_puts_recent_first_and_idle_contacts_last() {
        let mut store = Store::open_in_memory().expect("in-memory db");
        let a = ConversationId::Direct(peer("a"));
        let b = ConversationId::Direct(peer("b"));
        let idle = ConversationId::Group("g".into());
        store
            .upsert_names([(&idle, "Aaa idle group")])
            .expect("upsert");
        store
            .insert_message(&incoming(&a, "a", 1, "old"), ReadState::Read)
            .expect("insert");
        store
            .insert_message(&incoming(&b, "b", 2, "new"), ReadState::Read)
            .expect("insert");
        // An older message arriving late must not replace the preview.
        store
            .insert_message(&incoming(&b, "b", 0, "older"), ReadState::Read)
            .expect("insert");

        let convs = store.conversations().expect("list");
        let ids: Vec<_> = convs.iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids, vec![b.clone(), a, idle]);
        assert_eq!(convs[0].last_preview.as_deref(), Some("new"));

        let msgs = store.messages(&b).expect("messages");
        let bodies: Vec<_> = msgs.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(bodies, vec!["older", "new"], "messages come oldest first");
    }

    #[test]
    fn receipts_never_downgrade_and_read_elsewhere_clears_unread() {
        let mut store = Store::open_in_memory().expect("in-memory db");
        let alice = ConversationId::Direct(peer("alice"));
        let mine = NewMessage {
            conversation: alice.clone(),
            author: Author::Me,
            author_name: None,
            timestamp: Timestamp(5),
            body: "yo".into(),
            expires_in: None,
        };
        store
            .insert_message(&mine, ReadState::Read)
            .expect("insert");
        assert_eq!(
            store
                .apply_receipt(DeliveryStatus::Read, &[Timestamp(5)])
                .expect("receipt"),
            vec![alice.clone()]
        );
        assert!(
            store
                .apply_receipt(DeliveryStatus::Delivered, &[Timestamp(5)])
                .expect("receipt")
                .is_empty(),
            "late delivery receipt must not downgrade"
        );
        assert_eq!(
            store.messages(&alice).expect("messages")[0].status,
            Some(DeliveryStatus::Read)
        );

        store
            .insert_message(&incoming(&alice, "alice", 6, "unread"), ReadState::Unread)
            .expect("insert");
        assert_eq!(store.total_unread().expect("unread"), 1);
        assert_eq!(
            store
                .mark_read_elsewhere(&peer("alice"), Timestamp(6))
                .expect("sync"),
            vec![alice]
        );
        assert_eq!(store.total_unread().expect("unread"), 0);
        assert_eq!(
            store
                .mark_read_elsewhere(&peer("alice"), Timestamp(6))
                .expect("sync"),
            Vec::<ConversationId>::new(),
            "second sync is a no-op"
        );
    }

    #[test]
    fn edit_and_delete() {
        let mut store = Store::open_in_memory().expect("in-memory db");
        let alice = ConversationId::Direct(peer("alice"));
        let author = Author::Peer(peer("alice"));
        store
            .insert_message(&incoming(&alice, "alice", 1, "tpyo"), ReadState::Read)
            .expect("insert");
        assert!(
            store
                .edit_message(&alice, &author, Timestamp(1), "typo")
                .expect("edit")
        );
        assert!(
            !store
                .edit_message(&alice, &Author::Me, Timestamp(1), "x")
                .expect("edit")
        );
        let msg = &store.messages(&alice).expect("messages")[0];
        assert_eq!((msg.body.as_str(), msg.edited), ("typo", true));
        let summary = store.conversation(&alice).expect("query").expect("exists");
        assert_eq!(
            summary.last_preview.as_deref(),
            Some("typo"),
            "edits update the preview"
        );

        assert!(
            store
                .delete_message(&alice, &author, Timestamp(1))
                .expect("delete")
        );
        assert!(store.messages(&alice).expect("messages")[0].deleted);
        let summary = store.conversation(&alice).expect("query").expect("exists");
        assert_eq!(
            summary.last_preview, None,
            "deleted text must not linger in the list"
        );
    }

    #[test]
    fn disappearing_messages_are_purged_with_their_preview() {
        let mut store = Store::open_in_memory().expect("in-memory db");
        let alice = ConversationId::Direct(peer("alice"));
        store
            .insert_message(
                &incoming(&alice, "alice", 1_000, "keep me"),
                ReadState::Read,
            )
            .expect("insert");
        let secret = NewMessage {
            expires_in: Some(Duration::from_secs(10)),
            ..incoming(&alice, "alice", 2_000, "secret")
        };
        store
            .insert_message(&secret, ReadState::Unread)
            .expect("insert");
        assert_eq!(
            store.expiration_timer(&alice).expect("timer"),
            Some(Duration::from_secs(10)),
            "timer is learned from incoming messages"
        );

        assert!(
            store
                .purge_expired(Timestamp(11_999))
                .expect("purge")
                .is_empty()
        );
        assert_eq!(
            store.purge_expired(Timestamp(12_000)).expect("purge"),
            vec![alice.clone()]
        );

        let bodies: Vec<_> = store
            .messages(&alice)
            .expect("messages")
            .into_iter()
            .map(|m| m.body)
            .collect();
        assert_eq!(bodies, vec!["keep me"]);
        let summary = store
            .conversation(&alice)
            .expect("query")
            .expect("conversation exists");
        assert_eq!(summary.last_preview.as_deref(), Some("keep me"));
        assert_eq!(summary.unread, 0, "purged unread messages no longer count");

        store.set_expiration_timer(&alice, None).expect("timer off");
        assert_eq!(store.expiration_timer(&alice).expect("timer"), None);
    }

    #[test]
    fn preview_truncates_on_char_boundary() {
        let long = "ä".repeat(100);
        let p = preview(&long);
        assert_eq!(p.chars().count(), 81);
        assert!(p.ends_with('…'));
    }
}
