//! Sample conversations for `--demo`, to try the UI without a Signal
//! account: a personal and a work account.

use gtk::glib;

use crate::config::{AccountConfig, Color};
use crate::model::{AccountId, Author, ConversationId, NewMessage, PeerId, Timestamp};
use crate::store::{ReadState, Store};

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

type Sample<'a> = (&'a ConversationId, Option<&'a str>, i64, &'a str, ReadState);

/// The demo accounts with their in-memory stores.
pub fn accounts() -> anyhow::Result<Vec<(AccountConfig, Store)>> {
    let account = |id: &str, name: &str, color: Color| AccountConfig {
        id: AccountId::try_from(id.to_owned()).expect("valid account id"),
        name: name.to_owned(),
        color,
    };
    let mut personal = Store::open_in_memory()?;
    populate_personal(&mut personal)?;
    let mut work = Store::open_in_memory()?;
    populate_work(&mut work)?;
    Ok(vec![
        (account("1", "Personal", Color::PALETTE[0]), personal),
        (account("2", "Work", Color::PALETTE[1]), work),
    ])
}

fn peer(id: &str) -> PeerId {
    PeerId(id.to_owned())
}

fn direct(id: &str) -> ConversationId {
    ConversationId::Direct(peer(id))
}

fn populate_work(store: &mut Store) -> anyhow::Result<()> {
    let dave = direct("dave");
    let team = ConversationId::Group("team".into());
    store.upsert_names([
        (&dave, "Dave"),
        (&team, "Platform team"),
        (&direct("grace"), "Grace Hopper"),
    ])?;
    insert(
        store,
        &[
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
                &team,
                Some("grace"),
                50 * MINUTE,
                "Deploy is green, thanks all",
                ReadState::Read,
            ),
            (
                &team,
                Some("dave"),
                5 * MINUTE,
                "Standup moved to 10:30 tomorrow",
                ReadState::Unread,
            ),
        ],
    )
}

fn populate_personal(store: &mut Store) -> anyhow::Result<()> {
    let alice = direct("alice");
    let bob = direct("bob");
    let family = ConversationId::Group("family".into());
    let carol = direct("carol");
    store.upsert_names([
        (&alice, "Alice Liddell"),
        (&bob, "Bob"),
        (&family, "Family 🏡"),
        (&carol, "Carol Danvers"),
        (&direct("erin"), "Erin"),
        (&direct("frank"), "Frank Zappa"),
        (&ConversationId::Group("climbing".into()), "Climbing crew"),
    ])?;

    let messages: &[Sample<'_>] = &[
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
    insert(store, messages)
}

fn insert(store: &mut Store, messages: &[Sample<'_>]) -> anyhow::Result<()> {
    let now = glib::real_time() / 1000;
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
