//! Domain types shared by the Signal backend, the store and the UI.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

/// Milliseconds since the Unix epoch, as used by Signal for message ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i64);

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A Signal user, as a service id string: the bare ACI uuid, or
/// `PNI:<uuid>` for accounts only known by phone number identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PeerId(pub String);

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ConversationId {
    Direct(PeerId),
    /// Base64 group master key.
    Group(String),
}

impl fmt::Display for ConversationId {
    /// Stable string form used as database key and GAction target.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct(peer) => write!(f, "d:{peer}"),
            Self::Group(id) => write!(f, "g:{id}"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid conversation id {0:?}")]
pub struct InvalidConversationId(String);

impl FromStr for ConversationId {
    type Err = InvalidConversationId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once(':') {
            Some(("d", peer)) if !peer.is_empty() => Ok(Self::Direct(PeerId(peer.to_owned()))),
            Some(("g", group)) if !group.is_empty() => Ok(Self::Group(group.to_owned())),
            _ => Err(InvalidConversationId(s.to_owned())),
        }
    }
}

/// One of the linked Signal accounts. Local only: it names the account's
/// data directory and never leaves the machine.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Deserialize, serde::Serialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct AccountId(String);

#[derive(Debug, thiserror::Error)]
#[error("invalid account id {0:?}: use letters, digits, '-' and '_'")]
pub struct InvalidAccountId(String);

impl AccountId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Restricted so the id is safe as a directory name and CSS class, and
/// can't contain the separator of [`ChatRef`]'s string form.
impl TryFrom<String> for AccountId {
    type Error = InvalidAccountId;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        let valid = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if valid {
            Ok(Self(id))
        } else {
            Err(InvalidAccountId(id))
        }
    }
}

impl From<AccountId> for String {
    fn from(id: AccountId) -> Self {
        id.0
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A conversation of a particular account. The same person or group in two
/// accounts is two chats.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChatRef {
    pub account: AccountId,
    pub conversation: ConversationId,
}

impl fmt::Display for ChatRef {
    /// Stable string form used as notification id and GAction target.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.account, self.conversation)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid chat {0:?}")]
pub struct InvalidChatRef(String);

impl FromStr for ChatRef {
    type Err = InvalidChatRef;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidChatRef(s.to_owned());
        // Account ids never contain '/', group ids may.
        let (account, conversation) = s.split_once('/').ok_or_else(invalid)?;
        Ok(Self {
            account: AccountId::try_from(account.to_owned()).map_err(|_| invalid())?,
            conversation: conversation.parse().map_err(|_| invalid())?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Author {
    Me,
    Peer(PeerId),
}

/// Delivery state of an outgoing message. Ordered so that a receipt can
/// never move a message backwards (e.g. a late delivery receipt after a read
/// receipt).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeliveryStatus {
    Sent = 1,
    Delivered = 2,
    Read = 3,
}

impl DeliveryStatus {
    pub fn from_db(v: i64) -> Option<Self> {
        match v {
            1 => Some(Self::Sent),
            2 => Some(Self::Delivered),
            3 => Some(Self::Read),
            _ => None,
        }
    }
}

/// A message to be persisted, either received or sent (from here or another
/// linked device).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMessage {
    pub conversation: ConversationId,
    pub author: Author,
    /// Display name the sender advertised in the envelope, if any.
    pub author_name: Option<String>,
    pub timestamp: Timestamp,
    pub body: String,
    /// Disappearing-message timer; `None` when off.
    pub expires_in: Option<Duration>,
}

/// Something that happened on the Signal side and needs to be reflected
/// locally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalEvent {
    Message(NewMessage),
    Edit {
        conversation: ConversationId,
        author: Author,
        target: Timestamp,
        body: String,
    },
    Delete {
        conversation: ConversationId,
        author: Author,
        target: Timestamp,
    },
    /// Messages were read on another of our devices.
    ReadElsewhere {
        sender: PeerId,
        timestamp: Timestamp,
    },
    /// Receipt for messages we sent.
    Receipt {
        status: DeliveryStatus,
        timestamps: Vec<Timestamp>,
    },
    /// The disappearing-message timer of a conversation changed.
    ExpirationTimer {
        conversation: ConversationId,
        /// `None` when turned off.
        expires_in: Option<Duration>,
    },
}

impl SignalEvent {
    /// For logging without leaking message contents.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Message(_) => "message",
            Self::Edit { .. } => "edit",
            Self::Delete { .. } => "delete",
            Self::ReadElsewhere { .. } => "read-elsewhere",
            Self::Receipt { .. } => "receipt",
            Self::ExpirationTimer { .. } => "expiration-timer",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_id_roundtrip() {
        for id in [
            ConversationId::Direct(PeerId("a1a1a1a1-0000-0000-0000-000000000001".into())),
            ConversationId::Direct(PeerId("PNI:a1a1a1a1-0000-0000-0000-000000000001".into())),
            ConversationId::Group("abc/def+==".into()),
        ] {
            let parsed: ConversationId = id.to_string().parse().expect("valid id must parse");
            assert_eq!(parsed, id);
        }
        assert!("x:foo".parse::<ConversationId>().is_err());
        assert!("d:".parse::<ConversationId>().is_err());
    }

    #[test]
    fn chat_ref_roundtrip() {
        let account = |id: &str| AccountId::try_from(id.to_owned()).expect("valid account id");
        for chat in [
            ChatRef {
                account: account("1"),
                conversation: ConversationId::Direct(PeerId(
                    "a1a1a1a1-0000-0000-0000-000000000001".into(),
                )),
            },
            ChatRef {
                account: account("work_2"),
                conversation: ConversationId::Group("ab/c/d+==".into()),
            },
        ] {
            let parsed: ChatRef = chat.to_string().parse().expect("valid chat must parse");
            assert_eq!(parsed, chat);
        }
        assert!("d:peer".parse::<ChatRef>().is_err(), "account is required");
        assert!("/d:peer".parse::<ChatRef>().is_err());
        assert!("1/x:peer".parse::<ChatRef>().is_err());
    }

    #[test]
    fn account_ids_are_path_and_css_safe() {
        assert!(AccountId::try_from("12".to_owned()).is_ok());
        for bad in ["", "a/b", "..", "a b", "ü"] {
            assert!(AccountId::try_from(bad.to_owned()).is_err(), "{bad:?}");
        }
    }
}
