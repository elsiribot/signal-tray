//! Local message history. signal-cli does not keep messages, so everything
//! shown in the UI lives here.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::model::{
    AttachmentKind, Author, ConversationId, DeliveryStatus, NewMessage, PeerId, Timestamp,
    text_with_attachments,
};

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
-- Images and audio of messages. Deleted together with their message.
CREATE TABLE IF NOT EXISTS attachments (
    id           INTEGER PRIMARY KEY,
    message_id   INTEGER NOT NULL,
    -- position among the message's attachments
    idx          INTEGER NOT NULL,
    content_type TEXT NOT NULL,
    file_name    TEXT,
    width        INTEGER,
    height       INTEGER,
    -- Signal's AttachmentPointer (protobuf), to download it with
    pointer      BLOB NOT NULL,
    -- name in the account's attachments directory, NULL until downloaded
    file         TEXT,
    UNIQUE (message_id, idx)
);
-- Pictures of contacts and groups.
CREATE TABLE IF NOT EXISTS avatars (
    -- ConversationId string form
    subject      TEXT PRIMARY KEY,
    -- name in the account's avatars directory, NULL for no picture
    file         TEXT,
    -- ms since epoch
    fetched_at   INTEGER NOT NULL
);
";

/// Columns added to `attachments` after it was introduced, with their
/// definitions. Added to existing databases on open.
const ATTACHMENT_COLUMNS: &[(&str, &str)] = &[
    // AttachmentKind::as_str
    ("kind", "TEXT NOT NULL DEFAULT 'image'"),
    // audio only: whether it was listened to
    ("played", "INTEGER NOT NULL DEFAULT 1"),
    // audio only: length, NULL until probed
    ("duration_ms", "INTEGER"),
];

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
    pub attachments: Vec<StoredAttachment>,
    /// Only set for our own messages.
    pub status: Option<DeliveryStatus>,
    pub edited: bool,
    pub deleted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AttachmentId(pub i64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredAttachment {
    pub id: AttachmentId,
    pub kind: AttachmentKind,
    pub file_name: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Name in the attachments directory; `None` until downloaded.
    pub file: Option<String>,
    /// For audio: listened to.
    pub played: bool,
    /// For audio, once probed.
    pub duration: Option<Duration>,
}

/// An attachment that is yet to be downloaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingAttachment {
    pub id: AttachmentId,
    pub content_type: String,
    pub pointer: Vec<u8>,
}

/// The picture of a contact or group, as last fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredAvatar {
    /// Name in the avatars directory; `None` if there's no picture.
    pub file: Option<String>,
    pub fetched_at: Timestamp,
}

/// What [`Store::purge_expired`] removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Purged {
    pub conversations: Vec<ConversationId>,
    /// Downloaded attachments of the purged messages, to delete.
    pub files: Vec<String>,
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
        add_missing_columns(&conn, "attachments", ATTACHMENT_COLUMNS)
            .context("upgrading schema")?;
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

    /// Stores a message with its images and bumps its conversation. Returns
    /// `false` if the message was already known (history hand-overs repeat
    /// messages received live).
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
            let message_id = tx.last_insert_rowid();
            let mut stmt = tx.prepare(
                "INSERT INTO attachments
                     (message_id, idx, kind, content_type, file_name, width, height, pointer,
                      played)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            // What's read is taken as heard: history and our own messages.
            let played = read == ReadState::Read;
            for (idx, attachment) in (0i64..).zip(&msg.attachments) {
                stmt.execute(params![
                    message_id,
                    idx,
                    attachment.kind.as_str(),
                    attachment.content_type,
                    attachment.file_name,
                    attachment.width,
                    attachment.height,
                    attachment.pointer,
                    played,
                ])?;
            }
            drop(stmt);
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
                    preview(&text_with_attachments(
                        &msg.body,
                        &msg.attachments.iter().map(|a| a.kind).collect::<Vec<_>>(),
                    )),
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

    /// Deletes disappearing messages whose time is up.
    pub fn purge_expired(&mut self, now: Timestamp) -> anyhow::Result<Purged> {
        let tx = self.conn.transaction()?;
        let files = delete_attachments(
            &tx,
            "SELECT id FROM messages WHERE expires_at <= ?1",
            params![now.0],
        )?;
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
        Ok(Purged {
            conversations: affected
                .into_iter()
                .map(|c| c.parse())
                .collect::<Result<_, _>>()?,
            files,
        })
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

    /// The message is kept as a "deleted" placeholder and no longer counts
    /// as unread; its images are dropped. Returns `None` if no message
    /// matched, else the files of its downloaded images, to delete.
    pub fn delete_message(
        &mut self,
        conversation: &ConversationId,
        author: &Author,
        target: Timestamp,
    ) -> anyhow::Result<Option<Vec<String>>> {
        let tx = self.conn.transaction()?;
        let id = conversation.to_string();
        let files = delete_attachments(
            &tx,
            "SELECT id FROM messages WHERE conversation = ?1 AND author = ?2 AND ts = ?3",
            params![id, author_column(author), target.0],
        )?;
        let found = tx.execute(
            "UPDATE messages SET body = '', deleted = 1, is_read = 1
             WHERE conversation = ?1 AND author = ?2 AND ts = ?3",
            params![id, author_column(author), target.0],
        )? > 0;
        if found {
            recompute_summary(&tx, &id)?;
        }
        tx.commit()?;
        Ok(found.then_some(files))
    }

    /// Images of the conversation's recent messages (those
    /// [`Store::messages`] returns) that are yet to be downloaded.
    pub fn missing_attachments(
        &self,
        conversation: &ConversationId,
    ) -> anyhow::Result<Vec<MissingAttachment>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT a.id, a.content_type, a.pointer FROM attachments a
             JOIN (SELECT id FROM messages WHERE conversation = ?1
                   ORDER BY ts DESC LIMIT ?2) m ON a.message_id = m.id
             WHERE a.file IS NULL
             ORDER BY a.id",
        )?;
        let rows = stmt.query_map(params![conversation.to_string(), history_limit()], |row| {
            Ok(MissingAttachment {
                id: AttachmentId(row.get(0)?),
                content_type: row.get(1)?,
                pointer: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The images of a message, in order.
    pub fn message_attachments(
        &self,
        conversation: &ConversationId,
        author: &Author,
        timestamp: Timestamp,
    ) -> anyhow::Result<Vec<AttachmentId>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT a.id FROM attachments a JOIN messages m ON a.message_id = m.id
             WHERE m.conversation = ?1 AND m.author = ?2 AND m.ts = ?3
             ORDER BY a.idx",
        )?;
        let rows = stmt.query_map(
            params![conversation.to_string(), author_column(author), timestamp.0],
            |row| Ok(AttachmentId(row.get(0)?)),
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Records that an attachment was saved as `file`. Returns its
    /// conversation, or `None` if the attachment is gone (its message was
    /// deleted meanwhile), in which case the file should be deleted.
    pub fn set_attachment_file(
        &self,
        id: AttachmentId,
        file: &str,
    ) -> anyhow::Result<Option<ConversationId>> {
        let conversation: Option<String> = self
            .conn
            .query_row(
                "UPDATE attachments SET file = ?2 WHERE id = ?1 RETURNING message_id",
                params![id.0, file],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(|message_id| {
                self.conn.query_row(
                    "SELECT conversation FROM messages WHERE id = ?1",
                    params![message_id],
                    |row| row.get(0),
                )
            })
            .transpose()?;
        Ok(conversation.map(|c| c.parse()).transpose()?)
    }

    /// Marks audio as listened to. Returns whether it wasn't before.
    pub fn mark_played(&self, id: AttachmentId) -> anyhow::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE attachments SET played = 1 WHERE id = ?1 AND played = 0",
            params![id.0],
        )? > 0)
    }

    pub fn set_duration(&self, id: AttachmentId, duration: Duration) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE attachments SET duration_ms = ?2 WHERE id = ?1",
            params![id.0, duration_millis(duration)],
        )?;
        Ok(())
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
        let mut attachments = self.recent_attachments(conversation)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT author, author_name, ts, body, status, edited, deleted, id FROM (
                 SELECT * FROM messages WHERE conversation = ?1
                 ORDER BY ts DESC LIMIT ?2
             ) ORDER BY ts ASC",
        )?;
        let rows = stmt.query_map(params![conversation.to_string(), history_limit()], |row| {
            let author: String = row.get(0)?;
            let message_id: i64 = row.get(7)?;
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
                attachments: attachments.remove(&message_id).unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Images of the messages [`Store::messages`] returns, by message id.
    fn recent_attachments(
        &self,
        conversation: &ConversationId,
    ) -> anyhow::Result<HashMap<i64, Vec<StoredAttachment>>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT a.message_id, a.id, a.file_name, a.width, a.height, a.file, a.kind,
                    a.played, a.duration_ms
             FROM attachments a
             JOIN (SELECT id FROM messages WHERE conversation = ?1
                   ORDER BY ts DESC LIMIT ?2) m ON a.message_id = m.id
             ORDER BY a.message_id, a.idx",
        )?;
        let rows = stmt.query_map(params![conversation.to_string(), history_limit()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                StoredAttachment {
                    id: AttachmentId(row.get(1)?),
                    file_name: row.get(2)?,
                    width: row.get(3)?,
                    height: row.get(4)?,
                    file: row.get(5)?,
                    kind: AttachmentKind::from_db(&row.get::<_, String>(6)?),
                    played: row.get(7)?,
                    duration: row
                        .get::<_, Option<i64>>(8)?
                        .and_then(|ms| u64::try_from(ms).ok())
                        .map(Duration::from_millis),
                },
            ))
        })?;
        let mut attachments: HashMap<i64, Vec<StoredAttachment>> = HashMap::new();
        for row in rows {
            let (message_id, attachment) = row?;
            attachments.entry(message_id).or_default().push(attachment);
        }
        Ok(attachments)
    }

    /// The pictures of contacts and groups, as last fetched.
    pub fn avatars(&self) -> anyhow::Result<HashMap<ConversationId, StoredAvatar>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT subject, file, fetched_at FROM avatars")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                StoredAvatar {
                    file: row.get(1)?,
                    fetched_at: Timestamp(row.get(2)?),
                },
            ))
        })?;
        rows.map(|row| {
            let (subject, avatar) = row?;
            Ok((subject.parse()?, avatar))
        })
        .collect()
    }

    /// Records a fetched picture. Returns the file it replaces, to delete.
    pub fn set_avatar(
        &mut self,
        subject: &ConversationId,
        avatar: &StoredAvatar,
    ) -> anyhow::Result<Option<String>> {
        let tx = self.conn.transaction()?;
        let subject = subject.to_string();
        let previous: Option<String> = tx
            .query_row(
                "SELECT file FROM avatars WHERE subject = ?1",
                params![subject],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        tx.execute(
            "INSERT INTO avatars (subject, file, fetched_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (subject) DO UPDATE SET
                 file = excluded.file, fetched_at = excluded.fetched_at",
            params![subject, avatar.file, avatar.fetched_at.0],
        )?;
        tx.commit()?;
        Ok(previous.filter(|p| Some(p) != avatar.file.as_ref()))
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

fn add_missing_columns(
    conn: &Connection,
    table: &str,
    columns: &[(&str, &str)],
) -> rusqlite::Result<()> {
    let existing: Vec<String> = conn
        .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for (name, definition) in columns {
        if !existing.iter().any(|e| e == name) {
            conn.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {name} {definition}"
            ))?;
        }
    }
    Ok(())
}

fn history_limit() -> i64 {
    i64::try_from(CHAT_HISTORY_LIMIT).expect("history limit fits in i64")
}

/// Deletes the images of the messages `messages` selects (by id) and
/// returns the files of those that were downloaded.
fn delete_attachments(
    conn: &Connection,
    messages: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!(
        "DELETE FROM attachments WHERE message_id IN ({messages}) RETURNING file"
    ))?;
    stmt.query_map(params, |row| row.get::<_, Option<String>>(0))?
        .filter_map(Result::transpose)
        .collect()
}

/// Re-derives a conversation's preview and unread count from its messages,
/// after messages changed underneath them.
fn recompute_summary(conn: &Connection, conversation: &str) -> rusqlite::Result<()> {
    let latest: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, body FROM messages WHERE conversation = ?1 AND deleted = 0
             ORDER BY ts DESC LIMIT 1",
            params![conversation],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let latest = match latest {
        Some((id, body)) => {
            let kinds: Vec<AttachmentKind> = conn
                .prepare_cached("SELECT kind FROM attachments WHERE message_id = ?1 ORDER BY idx")?
                .query_map(params![id], |row| {
                    Ok(AttachmentKind::from_db(&row.get::<_, String>(0)?))
                })?
                .collect::<Result<_, _>>()?;
            Some(preview(&text_with_attachments(&body, &kinds)))
        }
        None => None,
    };
    conn.execute(
        "UPDATE conversations SET
             last_preview = ?2,
             unread = (SELECT COUNT(*) FROM messages WHERE conversation = ?1 AND is_read = 0)
         WHERE id = ?1",
        params![conversation, latest],
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
    use crate::model::NewAttachment;

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
            attachments: Vec::new(),
            expires_in: None,
        }
    }

    fn image(n: u8) -> NewAttachment {
        NewAttachment {
            kind: AttachmentKind::Image,
            content_type: "image/jpeg".into(),
            file_name: Some(format!("{n}.jpg")),
            width: Some(640),
            height: Some(480),
            pointer: vec![n],
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
            attachments: Vec::new(),
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

        assert_eq!(
            store
                .delete_message(&alice, &author, Timestamp(1))
                .expect("delete"),
            Some(Vec::new())
        );
        assert_eq!(
            store
                .delete_message(&alice, &Author::Me, Timestamp(1))
                .expect("delete"),
            None
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

        assert_eq!(
            store.purge_expired(Timestamp(11_999)).expect("purge"),
            Purged::default()
        );
        assert_eq!(
            store
                .purge_expired(Timestamp(12_000))
                .expect("purge")
                .conversations,
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
    fn images_are_stored_once_and_tracked_until_downloaded() {
        let mut store = Store::open_in_memory().expect("in-memory db");
        let alice = ConversationId::Direct(peer("alice"));
        let msg = NewMessage {
            attachments: vec![image(1), image(2)],
            ..incoming(&alice, "alice", 10, "")
        };
        assert!(store.insert_message(&msg, ReadState::Read).expect("insert"));
        assert!(!store.insert_message(&msg, ReadState::Read).expect("insert"));
        assert_eq!(
            store
                .conversation(&alice)
                .expect("query")
                .expect("exists")
                .last_preview
                .as_deref(),
            Some("🖼 2 photos")
        );

        let missing = store.missing_attachments(&alice).expect("missing");
        let pointers: Vec<_> = missing.iter().map(|m| m.pointer.clone()).collect();
        assert_eq!(
            pointers,
            vec![vec![1], vec![2]],
            "duplicates are not stored"
        );
        let ids = store
            .message_attachments(&alice, &Author::Peer(peer("alice")), Timestamp(10))
            .expect("ids");
        assert_eq!(ids, missing.iter().map(|m| m.id).collect::<Vec<_>>());

        assert_eq!(
            store
                .set_attachment_file(ids[0], "a.jpg")
                .expect("set file"),
            Some(alice.clone())
        );
        assert_eq!(store.missing_attachments(&alice).expect("missing").len(), 1);
        let images = &store.messages(&alice).expect("messages")[0].attachments;
        let files: Vec<_> = images.iter().map(|i| i.file.as_deref()).collect();
        assert_eq!(files, vec![Some("a.jpg"), None]);
        assert_eq!(images[0].file_name.as_deref(), Some("1.jpg"));
        assert_eq!((images[0].width, images[0].height), (Some(640), Some(480)));

        // Deleting the message drops its images and hands back the files.
        assert_eq!(
            store
                .delete_message(&alice, &Author::Peer(peer("alice")), Timestamp(10))
                .expect("delete"),
            Some(vec!["a.jpg".to_owned()])
        );
        assert!(
            store
                .missing_attachments(&alice)
                .expect("missing")
                .is_empty()
        );
        assert_eq!(
            store
                .set_attachment_file(ids[1], "b.jpg")
                .expect("set file"),
            None,
            "a late download of a deleted image is reported"
        );
    }

    #[test]
    fn voice_messages_are_unplayed_until_played() {
        let mut store = Store::open_in_memory().expect("in-memory db");
        let alice = ConversationId::Direct(peer("alice"));
        let voice = NewAttachment {
            kind: AttachmentKind::Voice,
            content_type: "audio/aac".into(),
            width: None,
            height: None,
            ..image(1)
        };
        let live = NewMessage {
            attachments: vec![voice.clone()],
            ..incoming(&alice, "alice", 10, "")
        };
        let history = NewMessage {
            attachments: vec![voice],
            ..incoming(&alice, "alice", 5, "")
        };
        store
            .insert_message(&live, ReadState::Unread)
            .expect("insert");
        store
            .insert_message(&history, ReadState::Read)
            .expect("insert");
        assert_eq!(
            store
                .conversation(&alice)
                .expect("query")
                .expect("exists")
                .last_preview
                .as_deref(),
            Some("🎤 Voice message")
        );

        let voices = |store: &Store| -> Vec<StoredAttachment> {
            store
                .messages(&alice)
                .expect("messages")
                .into_iter()
                .flat_map(|m| m.attachments)
                .collect()
        };
        let [old, new] = voices(&store).try_into().expect("two memos");
        assert_eq!((old.kind, old.played), (AttachmentKind::Voice, true));
        assert!(!new.played, "live messages start unplayed");
        assert_eq!(new.duration, None);

        assert!(store.mark_played(new.id).expect("mark played"));
        assert!(
            !store.mark_played(new.id).expect("mark played"),
            "only once"
        );
        store
            .set_duration(new.id, Duration::from_millis(4_250))
            .expect("set duration");
        let [_, new] = voices(&store).try_into().expect("two memos");
        assert_eq!(
            (new.played, new.duration),
            (true, Some(Duration::from_millis(4_250)))
        );
    }

    #[test]
    fn attachments_of_older_databases_gain_the_new_columns() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE attachments (
                 id INTEGER PRIMARY KEY, message_id INTEGER NOT NULL, idx INTEGER NOT NULL,
                 content_type TEXT NOT NULL, file_name TEXT, width INTEGER, height INTEGER,
                 pointer BLOB NOT NULL, file TEXT, UNIQUE (message_id, idx));
             INSERT INTO attachments (message_id, idx, content_type, pointer)
                 VALUES (1, 0, 'image/png', x'00');",
        )
        .expect("old schema");
        let store = Store::init(conn).expect("upgrade");
        let (kind, played): (String, bool) = store
            .conn
            .query_row("SELECT kind, played FROM attachments", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("new columns");
        assert_eq!((kind.as_str(), played), ("image", true));
        drop(store);
    }

    #[test]
    fn avatars_are_replaced_and_hand_back_the_old_file() {
        let mut store = Store::open_in_memory().expect("in-memory db");
        let alice = ConversationId::Direct(peer("alice"));
        let group = ConversationId::Group("ab/c+=".into());
        let avatar = |file: Option<&str>, at| StoredAvatar {
            file: file.map(str::to_owned),
            fetched_at: Timestamp(at),
        };
        assert_eq!(
            store
                .set_avatar(&alice, &avatar(Some("a-1.img"), 1))
                .expect("set"),
            None
        );
        assert_eq!(
            store.set_avatar(&group, &avatar(None, 1)).expect("set"),
            None
        );
        assert_eq!(
            store
                .set_avatar(&alice, &avatar(Some("a-2.img"), 2))
                .expect("set"),
            Some("a-1.img".to_owned())
        );
        assert_eq!(
            store
                .set_avatar(&alice, &avatar(Some("a-2.img"), 3))
                .expect("set"),
            None,
            "the same file isn't handed back"
        );
        let avatars = store.avatars().expect("avatars");
        assert_eq!(avatars[&alice], avatar(Some("a-2.img"), 3));
        assert_eq!(avatars[&group], avatar(None, 1));
    }

    #[test]
    fn purging_returns_image_files() {
        let mut store = Store::open_in_memory().expect("in-memory db");
        let alice = ConversationId::Direct(peer("alice"));
        let msg = NewMessage {
            attachments: vec![image(1)],
            expires_in: Some(Duration::from_secs(1)),
            ..incoming(&alice, "alice", 0, "see this")
        };
        store.insert_message(&msg, ReadState::Read).expect("insert");
        assert_eq!(
            store
                .conversation(&alice)
                .expect("query")
                .expect("exists")
                .last_preview
                .as_deref(),
            Some("🖼 see this")
        );
        let id = store.missing_attachments(&alice).expect("missing")[0].id;
        store.set_attachment_file(id, "1.jpg").expect("set file");
        assert_eq!(
            store.purge_expired(Timestamp(1_000)).expect("purge"),
            Purged {
                conversations: vec![alice],
                files: vec!["1.jpg".to_owned()],
            }
        );
    }

    #[test]
    fn preview_truncates_on_char_boundary() {
        let long = "ä".repeat(100);
        let p = preview(&long);
        assert_eq!(p.chars().count(), 81);
        assert!(p.ends_with('…'));
    }
}
