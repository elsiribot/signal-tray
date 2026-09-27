//! Sample conversations for `--demo`, to try the UI without a Signal
//! account.

use gtk::glib;

use crate::model::{Author, ConversationId, NewMessage, PeerId, Timestamp};
use crate::store::{ReadState, Store};

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

pub fn populate(store: &mut Store) -> anyhow::Result<()> {
    let now = glib::real_time() / 1000;
    let peer = |id: &str| PeerId(id.to_owned());
    let direct = |id: &str| ConversationId::Direct(peer(id));

    let alice = direct("alice");
    let bob = direct("bob");
    let family = ConversationId::Group("family".into());
    let carol = direct("carol");
    let dave = direct("dave");
    store.upsert_names([
        (&alice, "Alice Liddell"),
        (&bob, "Bob"),
        (&family, "Family 🏡"),
        (&carol, "Carol Danvers"),
        (&dave, "Dave (work)"),
        (&direct("erin"), "Erin"),
        (&direct("frank"), "Frank Zappa"),
        (&ConversationId::Group("climbing".into()), "Climbing crew"),
    ])?;

    let messages: &[(&ConversationId, Option<&str>, i64, &str, ReadState)] = &[
        (
            &dave,
            Some("dave"),
            3 * DAY,
            "Can you review the PR before Monday?",
            ReadState::Read,
        ),
        (
            &dave,
            None,
            3 * DAY - 5 * MINUTE,
            "Sure, will do tomorrow morning",
            ReadState::Read,
        ),
        (
            &carol,
            Some("carol"),
            26 * HOUR,
            "📎 IMG_2041.jpg",
            ReadState::Read,
        ),
        (
            &carol,
            Some("carol"),
            26 * HOUR - MINUTE,
            "look what I found in the attic",
            ReadState::Read,
        ),
        (&carol, None, 25 * HOUR, "haha amazing", ReadState::Read),
        (
            &family,
            Some("mum"),
            5 * HOUR,
            "Dinner on Sunday at 7?",
            ReadState::Read,
        ),
        (
            &family,
            Some("dad"),
            5 * HOUR - 10 * MINUTE,
            "👍",
            ReadState::Read,
        ),
        (
            &family,
            None,
            4 * HOUR,
            "I'll bring dessert",
            ReadState::Read,
        ),
        (
            &family,
            Some("mum"),
            40 * MINUTE,
            "Perfect, see you then ❤️",
            ReadState::Unread,
        ),
        (
            &bob,
            Some("bob"),
            2 * HOUR,
            "are we still on for tonight?",
            ReadState::Read,
        ),
        (
            &bob,
            None,
            2 * HOUR - 3 * MINUTE,
            "yes! 8pm at the usual place",
            ReadState::Read,
        ),
        (
            &alice,
            Some("alice"),
            12 * MINUTE,
            "Did you see the new release?",
            ReadState::Read,
        ),
        (
            &alice,
            None,
            10 * MINUTE,
            "Not yet, anything interesting?",
            ReadState::Read,
        ),
        (
            &alice,
            Some("alice"),
            2 * MINUTE,
            "Tons. The keyboard navigation is finally usable, and the whole thing fits in a corner of the screen now.",
            ReadState::Unread,
        ),
        (
            &alice,
            Some("alice"),
            MINUTE,
            "Try Alt+↓ to jump to the next chat",
            ReadState::Unread,
        ),
    ];
    for (conversation, author, age, body, read) in messages {
        let author = match author {
            Some(id) => Author::Peer(peer(id)),
            None => Author::Me,
        };
        store.insert_message(
            &NewMessage {
                conversation: (*conversation).clone(),
                author_name: match &author {
                    Author::Peer(p) => Some(capitalize(&p.0)),
                    Author::Me => None,
                },
                author,
                timestamp: Timestamp(now - age),
                body: (*body).to_owned(),
                expires_in: None,
            },
            *read,
        )?;
    }
    Ok(())
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}
