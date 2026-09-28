//! Round pictures of contacts and groups, or their initials on a colour
//! when they have none.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gtk::gdk;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::prelude::*;

use super::images;

/// Colours of initials; see `.avatar-c<n>` in the stylesheet.
const COLORS: u64 = 8;

/// Who a picture is of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvatarView {
    /// `None` if there's no picture (yet).
    pub path: Option<PathBuf>,
    /// For the initials.
    pub name: String,
    /// Stable id picking the colour, so it doesn't change with the name.
    pub seed: String,
}

/// Decoded pictures, by file and size. Files are never rewritten (a new
/// picture gets a new name), so entries don't go stale.
#[derive(Default)]
pub struct Avatars(RefCell<HashMap<(PathBuf, i32), Option<gdk::Texture>>>);

impl Avatars {
    /// A `size`×`size` round picture, or initials, in a box that takes
    /// the caller's margins: GTK counts margins into a widget's size
    /// request, so a margin on the circle itself would squash it.
    pub fn widget(&self, view: &AvatarView, size: i32) -> gtk::Widget {
        let texture = view
            .path
            .as_deref()
            .and_then(|path| self.texture(path, size));
        let circle = match texture {
            Some(texture) => images::picture(&texture, size, size),
            None => {
                let label = gtk::Label::builder()
                    .label(initials(&view.name))
                    .width_request(size)
                    .height_request(size)
                    .css_classes([
                        "avatar-initials".to_owned(),
                        format!("avatar-c{}", color(&view.seed)),
                    ])
                    .build();
                label.upcast()
            }
        };
        circle.add_css_class("avatar");
        let frame = gtk::Box::builder()
            .valign(gtk::Align::Center)
            .halign(gtk::Align::Center)
            .build();
        frame.append(&circle);
        frame.upcast()
    }

    /// Empty space as wide as [`Avatars::widget`] of `size`.
    pub fn spacer(size: i32) -> gtk::Widget {
        let frame = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        frame.append(&gtk::Box::builder().width_request(size).build());
        frame.upcast()
    }

    /// Decoded at twice the size, for HiDPI. Pictures are small, so this
    /// happens right away rather than in a thread.
    fn texture(&self, path: &Path, size: i32) -> Option<gdk::Texture> {
        let mut cache = self.0.borrow_mut();
        cache
            .entry((path.to_owned(), size))
            .or_insert_with(
                || match Pixbuf::from_file_at_scale(path, size * 2, size * 2, true) {
                    Ok(pixbuf) => Some(gdk::Texture::for_pixbuf(&pixbuf)),
                    Err(err) => {
                        tracing::info!(%err, path = %path.display(), "can't show picture");
                        None
                    }
                },
            )
            .clone()
    }
}

/// Up to two letters: the first of each of the first two words that have
/// one. Numbers get `#`; names without letters their first character.
pub fn initials(name: &str) -> String {
    let letters: String = name
        .split_whitespace()
        .filter_map(|word| word.chars().find(|c| c.is_alphabetic()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    if !letters.is_empty() {
        return letters;
    }
    match name.trim().chars().next() {
        Some(c) if c == '+' || c.is_ascii_digit() => "#".to_owned(),
        Some(c) => c.to_string(),
        None => "?".to_owned(),
    }
}

/// Index of the initials' colour: FNV-1a of the seed, so it's the same on
/// every run.
pub fn color(seed: &str) -> u64 {
    let hash = seed.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    hash % COLORS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_of_names_numbers_and_emoji() {
        assert_eq!(initials("Alice Liddell"), "AL");
        assert_eq!(initials("bob"), "B");
        assert_eq!(initials("Mary Ann Evans"), "MA");
        assert_eq!(initials("Family 🏡"), "F");
        assert_eq!(initials("(old) team"), "OT");
        assert_eq!(initials("+49 170 1234567"), "#");
        assert_eq!(initials("🏡"), "🏡");
        assert_eq!(initials("  "), "?");
        assert_eq!(initials("élodie"), "É");
    }

    #[test]
    fn colors_are_stable_and_spread() {
        assert_eq!(color("d:alice"), color("d:alice"));
        let used: std::collections::HashSet<u64> =
            (0..64).map(|i| color(&format!("d:peer-{i}"))).collect();
        assert!(used.len() > 4, "only {used:?}");
        assert!(used.iter().all(|c| *c < COLORS));
    }
}
