//! Translation of decrypted Signal [`Content`] into domain events.
//!
//! Pure functions, so the fiddly protobuf handling is unit tested without a
//! network or a store.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use presage::libsignal_service::content::{Content, ContentBody};
use presage::libsignal_service::protocol::ServiceId;
use presage::proto::body_range::AssociatedValue;
use presage::proto::data_message::Flags;
use presage::proto::receipt_message::Type as ReceiptType;
use presage::proto::sync_message::{self, Sent};
use presage::proto::{DataMessage, EditMessage, ReceiptMessage, SyncMessage};

use crate::model::{
    Author, ConversationId, DeliveryStatus, NewMessage, PeerId, SignalEvent, Timestamp,
};

/// Display names used to render @mentions.
pub type Names = HashMap<PeerId, String>;

/// Group master keys are 32 bytes.
pub type MasterKey = [u8; 32];

pub fn peer_id(id: &ServiceId) -> PeerId {
    PeerId(id.service_id_string())
}

pub fn service_id(peer: &PeerId) -> Option<ServiceId> {
    ServiceId::parse_from_service_id_string(&peer.0)
}

pub fn group_conversation(master_key: &[u8]) -> ConversationId {
    ConversationId::Group(BASE64.encode(master_key))
}

pub fn group_master_key(encoded: &str) -> Option<MasterKey> {
    BASE64.decode(encoded).ok()?.try_into().ok()
}

/// Whether a message was just received or read back from storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Live,
    Stored,
}

/// Where a piece of content comes from, which changes how it is read.
#[derive(Clone, Copy, Debug)]
pub enum Source<'a> {
    /// Just received: the content itself says which chat it belongs to.
    Live,
    /// Read back from presage's store, filed under `chat`. History imports
    /// file group messages there without the group context in the message,
    /// and edits have already been folded into the original message.
    Stored { chat: &'a ConversationId },
}

impl Source<'_> {
    pub fn origin(self) -> Origin {
        match self {
            Self::Live => Origin::Live,
            Self::Stored { .. } => Origin::Stored,
        }
    }
}

/// How the chat of a message is determined.
struct Route<'a, F> {
    source: Source<'a>,
    /// For live 1:1 messages: the other party.
    direct_peer: F,
}

impl<F: FnOnce() -> Option<PeerId>> Route<'_, F> {
    fn conversation(self, data: &DataMessage) -> Option<ConversationId> {
        if let Source::Stored { chat } = self.source {
            return Some(chat.clone());
        }
        match data.group_v2.as_ref().and_then(|g| g.master_key.as_deref()) {
            Some(master_key) => Some(group_conversation(master_key)),
            None => (self.direct_peer)().map(ConversationId::Direct),
        }
    }
}

pub fn content_events(content: &Content, source: Source<'_>, names: &Names) -> Vec<SignalEvent> {
    let sender = peer_id(&content.metadata.sender);
    let envelope_ts = Timestamp(content.metadata.client_timestamp.timestamp_millis());
    let route = Route {
        source,
        direct_peer: || Some(sender.clone()),
    };
    let events = match &content.body {
        ContentBody::DataMessage(data) => data_event(
            data,
            Author::Peer(sender.clone()),
            route,
            envelope_ts,
            names,
        ),
        ContentBody::EditMessage(edit) => {
            edit_event(edit, Author::Peer(sender.clone()), route, names)
        }
        ContentBody::SynchronizeMessage(sync) => return sync_events(sync, source, names),
        ContentBody::ReceiptMessage(receipt) => receipt_event(receipt),
        // Typing indicators, calls, stories, decryption errors: not shown.
        _ => None,
    };
    events.into_iter().collect()
}

fn sync_events(sync: &SyncMessage, source: Source<'_>, names: &Names) -> Vec<SignalEvent> {
    let mut events = Vec::new();
    if let Some(sync_message::Content::Sent(sent)) = &sync.content {
        events.extend(sent_event(sent, source, names));
    }
    events.extend(sync.read.iter().filter_map(|read| {
        Some(SignalEvent::ReadElsewhere {
            sender: peer_id(&ServiceId::Aci(read.parse_sender_aci()?)),
            timestamp: Timestamp(i64::try_from(read.timestamp?).ok()?),
        })
    }));
    events
}

/// A message we sent from another of our devices.
fn sent_event(sent: &Sent, source: Source<'_>, names: &Names) -> Option<SignalEvent> {
    let route = Route {
        source,
        direct_peer: || sent.parse_destination_service_id().map(|id| peer_id(&id)),
    };
    if let Some(edit) = &sent.edit_message {
        return edit_event(edit, Author::Me, route, names);
    }
    let data = sent.message.as_ref()?;
    let fallback_ts = Timestamp(i64::try_from(sent.timestamp?).ok()?);
    data_event(data, Author::Me, route, fallback_ts, names)
}

fn data_event(
    data: &DataMessage,
    author: Author,
    route: Route<'_, impl FnOnce() -> Option<PeerId>>,
    fallback_ts: Timestamp,
    names: &Names,
) -> Option<SignalEvent> {
    let conversation = route.conversation(data)?;
    let expires_in = expires_in(data);
    if data.flags.unwrap_or_default() & Flags::ExpirationTimerUpdate as u32 != 0 {
        return Some(SignalEvent::ExpirationTimer {
            conversation,
            expires_in,
        });
    }
    if let Some(target) = data.delete.as_ref().and_then(|d| d.target_sent_timestamp) {
        return Some(SignalEvent::Delete {
            conversation,
            author,
            target: Timestamp(i64::try_from(target).ok()?),
        });
    }
    // Reactions are deliberately not shown to keep the UI compact.
    if data.reaction.is_some() {
        return None;
    }
    let body = render_body(data, names);
    if body.is_empty() {
        // Profile key updates, session resets, group updates, etc.
        return None;
    }
    let timestamp = data
        .timestamp
        .and_then(|t| i64::try_from(t).ok())
        .map_or(fallback_ts, Timestamp);
    Some(SignalEvent::Message(NewMessage {
        conversation,
        author,
        author_name: None,
        timestamp,
        body,
        expires_in,
    }))
}

fn edit_event(
    edit: &EditMessage,
    author: Author,
    route: Route<'_, impl FnOnce() -> Option<PeerId>>,
    names: &Names,
) -> Option<SignalEvent> {
    let data = edit.data_message.as_ref()?;
    let origin = route.source.origin();
    let conversation = route.conversation(data)?;
    let target = Timestamp(i64::try_from(edit.target_sent_timestamp?).ok()?);
    let body = render_body(data, names);
    if body.is_empty() {
        return None;
    }
    Some(match origin {
        Origin::Live => SignalEvent::Edit {
            conversation,
            author,
            target,
            body,
        },
        // presage stores the edit in place of the original message, so this
        // *is* the message, under the original's timestamp.
        Origin::Stored => SignalEvent::Message(NewMessage {
            conversation,
            author,
            author_name: None,
            timestamp: target,
            body,
            expires_in: expires_in(data),
        }),
    })
}

fn receipt_event(receipt: &ReceiptMessage) -> Option<SignalEvent> {
    let status = match receipt.r#type.and_then(|t| ReceiptType::try_from(t).ok())? {
        ReceiptType::Delivery => DeliveryStatus::Delivered,
        ReceiptType::Read | ReceiptType::Viewed => DeliveryStatus::Read,
    };
    Some(SignalEvent::Receipt {
        status,
        timestamps: receipt
            .timestamp
            .iter()
            .filter_map(|t| i64::try_from(*t).ok())
            .map(Timestamp)
            .collect(),
    })
}

fn expires_in(data: &DataMessage) -> Option<Duration> {
    data.expire_timer
        .filter(|s| *s > 0)
        .map(|s| Duration::from_secs(s.into()))
}

/// Flattens text, mentions, quotes and attachments into plain text.
fn render_body(data: &DataMessage, names: &Names) -> String {
    let mut lines = Vec::new();
    if let Some(text) = data.quote.as_ref().and_then(|q| q.text.as_deref()) {
        let first_line = text.lines().next().unwrap_or_default();
        lines.push(format!("› {first_line}"));
    }
    if let Some(text) = data.body.as_deref().filter(|t| !t.is_empty()) {
        lines.push(substitute_mentions(text, data, names));
    }
    if data.sticker.is_some() {
        lines.push("[sticker]".to_owned());
    }
    for attachment in &data.attachments {
        let name = attachment
            .file_name
            .as_deref()
            .or(attachment.content_type.as_deref())
            .unwrap_or("file");
        lines.push(format!("📎 {name}"));
    }
    lines.join("\n")
}

/// Signal encodes mentions as U+FFFC placeholders in the text with the
/// target in a side list of body ranges, ordered by position.
fn substitute_mentions(text: &str, data: &DataMessage, names: &Names) -> String {
    const PLACEHOLDER: char = '\u{FFFC}';
    let mut mentions: Vec<_> = data
        .body_ranges
        .iter()
        .filter_map(|range| {
            let target = match range.associated_value.as_ref()? {
                AssociatedValue::MentionAci(aci) => PeerId(aci.clone()),
                AssociatedValue::MentionAciBinary(bytes) => {
                    let uuid = uuid::Uuid::from_slice(bytes).ok()?;
                    PeerId(uuid.to_string())
                }
                AssociatedValue::Style(_) => return None,
            };
            Some((range.start.unwrap_or_default(), target))
        })
        .collect();
    mentions.sort_by_key(|(start, _)| *start);
    let mut mentions = mentions.into_iter();
    text.chars()
        .map(|c| {
            if c != PLACEHOLDER {
                return c.to_string();
            }
            let name = mentions
                .next()
                .and_then(|(_, peer)| names.get(&peer).cloned())
                .unwrap_or_else(|| "someone".to_owned());
            format!("@{name}")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use presage::libsignal_service::content::Metadata;
    use presage::libsignal_service::protocol::{Aci, DeviceId};
    use presage::proto::data_message::{Delete, Quote};
    use presage::proto::sync_message::Read;
    use presage::proto::{AttachmentPointer, BodyRange, GroupContextV2};

    use super::*;

    const ALICE: &str = "a1a1a1a1-0000-0000-0000-000000000001";
    const ME: &str = "5e1f5e1f-0000-0000-0000-000000000009";

    fn aci(s: &str) -> ServiceId {
        ServiceId::Aci(Aci::from(uuid::Uuid::parse_str(s).expect("test uuid")))
    }

    fn content(sender: &str, body: ContentBody) -> Content {
        let metadata = Metadata {
            sender: aci(sender),
            destination: aci(ME),
            sender_device: DeviceId::new(1).expect("valid device id"),
            pni_verified: None,
            client_timestamp: chrono::DateTime::from_timestamp_millis(1_000)
                .expect("valid timestamp"),
            server_timestamp: chrono::DateTime::from_timestamp_millis(1_001)
                .expect("valid timestamp"),
            needs_receipt: false,
            unidentified_sender: false,
            was_plaintext: false,
            server_guid: None,
            report_spam_token: None,
        };
        Content { metadata, body }
    }

    fn alice() -> PeerId {
        PeerId(ALICE.to_owned())
    }

    fn text(body: &str, ts: u64) -> DataMessage {
        DataMessage {
            body: Some(body.to_owned()),
            timestamp: Some(ts),
            ..Default::default()
        }
    }

    #[test]
    fn incoming_direct_message() {
        let c = content(ALICE, ContentBody::DataMessage(text("hi", 1_000)));
        assert_eq!(
            content_events(&c, Source::Live, &Names::new()),
            vec![SignalEvent::Message(NewMessage {
                conversation: ConversationId::Direct(alice()),
                author: Author::Peer(alice()),
                author_name: None,
                timestamp: Timestamp(1_000),
                body: "hi".into(),
                expires_in: None,
            })]
        );
    }

    #[test]
    fn group_message_with_mention_quote_and_attachment() {
        let key = [7u8; 32];
        let data = DataMessage {
            body: Some("hey \u{FFFC}!".into()),
            timestamp: Some(5),
            expire_timer: Some(60),
            group_v2: Some(GroupContextV2 {
                master_key: Some(key.to_vec()),
                ..Default::default()
            }),
            body_ranges: vec![BodyRange {
                start: Some(4),
                length: Some(1),
                associated_value: Some(AssociatedValue::MentionAci(ALICE.into())),
            }],
            quote: Some(Quote {
                text: Some("earlier\nsecond line".into()),
                ..Default::default()
            }),
            attachments: vec![AttachmentPointer {
                content_type: Some("image/jpeg".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let names = Names::from([(alice(), "Alice".to_owned())]);
        let events = content_events(
            &content(ALICE, ContentBody::DataMessage(data)),
            Source::Live,
            &names,
        );
        let [SignalEvent::Message(msg)] = events.as_slice() else {
            panic!("expected one message, got {events:?}");
        };
        assert_eq!(msg.conversation, group_conversation(&key));
        assert_eq!(msg.body, "› earlier\nhey @Alice!\n📎 image/jpeg");
        assert_eq!(msg.expires_in, Some(Duration::from_secs(60)));
        assert_eq!(group_master_key(&BASE64.encode(key)), Some(key));
    }

    #[test]
    fn sync_sent_and_read() {
        let sync = SyncMessage {
            content: Some(sync_message::Content::Sent(Sent {
                destination_service_id: Some(ALICE.into()),
                timestamp: Some(7),
                message: Some(text("from phone", 7)),
                ..Default::default()
            })),
            read: vec![Read {
                sender_aci: Some(ALICE.into()),
                timestamp: Some(42),
                ..Default::default()
            }],
            ..Default::default()
        };
        let events = content_events(
            &content(ME, ContentBody::SynchronizeMessage(sync)),
            Source::Live,
            &Names::new(),
        );
        assert_eq!(
            events,
            vec![
                SignalEvent::Message(NewMessage {
                    conversation: ConversationId::Direct(alice()),
                    author: Author::Me,
                    author_name: None,
                    timestamp: Timestamp(7),
                    body: "from phone".into(),
                    expires_in: None,
                }),
                SignalEvent::ReadElsewhere {
                    sender: alice(),
                    timestamp: Timestamp(42)
                },
            ]
        );
    }

    #[test]
    fn receipts_deletes_timer_updates_and_reactions() {
        let receipt = ReceiptMessage {
            r#type: Some(ReceiptType::Read as i32),
            timestamp: vec![3, 4],
        };
        assert_eq!(
            content_events(
                &content(ALICE, ContentBody::ReceiptMessage(receipt)),
                Source::Live,
                &Names::new()
            ),
            vec![SignalEvent::Receipt {
                status: DeliveryStatus::Read,
                timestamps: vec![Timestamp(3), Timestamp(4)],
            }]
        );

        let delete = DataMessage {
            delete: Some(Delete {
                target_sent_timestamp: Some(1_000),
            }),
            ..Default::default()
        };
        assert_eq!(
            content_events(
                &content(ALICE, ContentBody::DataMessage(delete)),
                Source::Live,
                &Names::new()
            ),
            vec![SignalEvent::Delete {
                conversation: ConversationId::Direct(alice()),
                author: Author::Peer(alice()),
                target: Timestamp(1_000),
            }]
        );

        let timer = DataMessage {
            flags: Some(Flags::ExpirationTimerUpdate as u32),
            expire_timer: Some(0),
            ..Default::default()
        };
        assert_eq!(
            content_events(
                &content(ALICE, ContentBody::DataMessage(timer)),
                Source::Live,
                &Names::new()
            ),
            vec![SignalEvent::ExpirationTimer {
                conversation: ConversationId::Direct(alice()),
                expires_in: None,
            }]
        );

        let reaction = DataMessage {
            reaction: Some(Default::default()),
            ..Default::default()
        };
        assert!(
            content_events(
                &content(ALICE, ContentBody::DataMessage(reaction)),
                Source::Live,
                &Names::new()
            )
            .is_empty()
        );
    }

    #[test]
    fn edits_are_updates_live_but_messages_when_stored() {
        let edit = EditMessage {
            target_sent_timestamp: Some(1_000),
            data_message: Some(text("fixed", 2_000)),
        };
        let c = content(ALICE, ContentBody::EditMessage(edit));
        assert_eq!(
            content_events(&c, Source::Live, &Names::new()),
            vec![SignalEvent::Edit {
                conversation: ConversationId::Direct(alice()),
                author: Author::Peer(alice()),
                target: Timestamp(1_000),
                body: "fixed".into(),
            }]
        );
        let chat = ConversationId::Direct(alice());
        let stored = content_events(&c, Source::Stored { chat: &chat }, &Names::new());
        let [SignalEvent::Message(msg)] = stored.as_slice() else {
            panic!("stored edit must become a message: {stored:?}");
        };
        assert_eq!(
            (msg.timestamp, msg.body.as_str()),
            (Timestamp(1_000), "fixed")
        );
    }

    /// History imports file group messages under the group thread without a
    /// group context in the message itself.
    #[test]
    fn stored_messages_belong_to_the_thread_they_were_filed_under() {
        let group = group_conversation(&[9u8; 32]);
        let stored = Source::Stored { chat: &group };

        let theirs = content(ALICE, ContentBody::DataMessage(text("in the group", 10)));
        let events = content_events(&theirs, stored, &Names::new());
        let [SignalEvent::Message(msg)] = events.as_slice() else {
            panic!("expected one message, got {events:?}");
        };
        assert_eq!(
            (&msg.conversation, &msg.author),
            (&group, &Author::Peer(alice()))
        );

        // Our own group messages are imported with no destination at all.
        let mine = content(
            ME,
            ContentBody::SynchronizeMessage(SyncMessage {
                content: Some(sync_message::Content::Sent(Sent {
                    timestamp: Some(11),
                    message: Some(text("me too", 11)),
                    ..Default::default()
                })),
                ..Default::default()
            }),
        );
        let events = content_events(&mine, stored, &Names::new());
        let [SignalEvent::Message(msg)] = events.as_slice() else {
            panic!("own group message must not be dropped: {events:?}");
        };
        assert_eq!((&msg.conversation, &msg.author), (&group, &Author::Me));
    }

    #[test]
    fn peer_ids_roundtrip_through_service_ids() {
        let peer = peer_id(&aci(ALICE));
        assert_eq!(peer, alice());
        assert_eq!(service_id(&peer), Some(aci(ALICE)));
    }
}
