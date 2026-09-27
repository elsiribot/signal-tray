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
}
