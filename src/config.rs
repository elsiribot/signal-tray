//! User configuration from `$XDG_CONFIG_HOME/signal-tray/config.toml`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::model::AccountId;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    #[default]
    BottomRight,
}

/// Distance of the window from the two screen edges of its corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(from = "MarginRepr", into = "MarginRepr")]
pub struct Margin {
    pub horizontal: i32,
    pub vertical: i32,
}

/// Written as one number for both edges, or `[horizontal, vertical]`.
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum MarginRepr {
    Both(i32),
    Each([i32; 2]),
}

impl From<MarginRepr> for Margin {
    fn from(repr: MarginRepr) -> Self {
        let [horizontal, vertical] = match repr {
            MarginRepr::Both(both) => [both; 2],
            MarginRepr::Each(each) => each,
        };
        Self {
            horizontal,
            vertical,
        }
    }
}

impl From<Margin> for MarginRepr {
    fn from(margin: Margin) -> Self {
        if margin.horizontal == margin.vertical {
            Self::Both(margin.horizontal)
        } else {
            Self::Each([margin.horizontal, margin.vertical])
        }
    }
}

/// How opaque the window background is. Text and message bubbles stay
/// fully opaque, so even a faint background keeps messages readable.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct Opacity(f64);

#[derive(Debug, thiserror::Error)]
#[error("opacity must be between {min} and {max}, got {value}", min = Opacity::MIN.0, max = Opacity::MAX.0)]
pub struct OpacityOutOfRange {
    value: f64,
}

impl Opacity {
    /// Below this the window is hard to make out against the desktop.
    pub const MIN: Self = Self(0.2);
    pub const MAX: Self = Self(1.0);

    pub fn get(self) -> f64 {
        self.0
    }
}

impl Default for Opacity {
    fn default() -> Self {
        Self(0.9)
    }
}

impl TryFrom<f64> for Opacity {
    type Error = OpacityOutOfRange;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if (Self::MIN.0..=Self::MAX.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(OpacityOutOfRange { value })
        }
    }
}

impl From<Opacity> for f64 {
    fn from(opacity: Opacity) -> Self {
        opacity.0
    }
}

/// An opaque colour, written as `#rrggbb`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, thiserror::Error)]
#[error("colour must be written as #rrggbb, got {0:?}")]
pub struct InvalidColor(String);

impl Color {
    const BLACK: Self = Self::rgb(0, 0, 0);
    const WHITE: Self = Self::rgb(0xff, 0xff, 0xff);

    /// Handed out to new accounts in order, skipping colours in use. The
    /// first one is the outgoing bubble colour of the single-account days.
    pub const PALETTE: [Self; 8] = [
        Self::rgb(0x2c, 0x6b, 0xed), // blue
        Self::rgb(0xe8, 0x71, 0x0a), // orange
        Self::rgb(0x2e, 0x9e, 0x5b), // green
        Self::rgb(0x8e, 0x44, 0xad), // purple
        Self::rgb(0xd9, 0x3b, 0x3b), // red
        Self::rgb(0x0f, 0x9d, 0x9a), // teal
        Self::rgb(0xd6, 0x42, 0x8f), // pink
        Self::rgb(0x8d, 0x6e, 0x3f), // brown
    ];

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// The first palette colour not in `used`, cycling once all are taken.
    pub fn next_unused(used: &[Self]) -> Self {
        Self::PALETTE
            .into_iter()
            .find(|color| !used.contains(color))
            .unwrap_or(Self::PALETTE[used.len() % Self::PALETTE.len()])
    }

    /// WCAG relative luminance, 0 for black to 1 for white.
    fn luminance(self) -> f64 {
        let linear = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    /// Black or white, whichever reads on this background. Leans towards
    /// white, which looks right on the saturated colours of the palette.
    pub fn text_color(self) -> Self {
        if self.luminance() > 0.5 {
            Self::BLACK
        } else {
            Self::WHITE
        }
    }
}

impl std::fmt::Display for Color {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

impl std::str::FromStr for Color {
    type Err = InvalidColor;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidColor(s.to_owned());
        let hex = s.strip_prefix('#').ok_or_else(invalid)?;
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(invalid());
        }
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| invalid());
        Ok(Self::rgb(channel(0)?, channel(2)?, channel(4)?))
    }
}

impl TryFrom<String> for Color {
    type Error = InvalidColor;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Color> for String {
    fn from(color: Color) -> Self {
        color.to_string()
    }
}

/// A linked Signal account. Only accounts that finished linking are listed.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    /// Names the account's data directory.
    pub id: AccountId,
    /// Shown next to the chat title; empty until linked, then the phone
    /// number unless changed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub color: Color,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Window size in logical pixels.
    pub width: i32,
    pub height: i32,
    /// The corner the window sits in and its distance from the edges;
    /// Super+drag updates both.
    pub corner: Corner,
    pub margin: Margin,
    /// Opacity of the window background.
    pub opacity: Opacity,
    /// Start minimized to the tray.
    pub start_hidden: bool,
    /// Hide the window when it loses keyboard focus.
    pub hide_on_focus_loss: bool,
    /// Desktop notifications for messages arriving while not looking.
    pub notifications: bool,
    /// Name shown in the phone's "Linked devices" list.
    pub device_name: String,
    /// Last: TOML wants tables after plain keys.
    pub accounts: Vec<AccountConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            width: 340,
            height: 520,
            corner: Corner::default(),
            margin: Margin {
                horizontal: 8,
                vertical: 8,
            },
            opacity: Opacity::default(),
            start_hidden: false,
            hide_on_focus_loss: false,
            notifications: true,
            device_name: "signal-tray".to_owned(),
            accounts: Vec::new(),
        }
    }
}

impl Config {
    pub fn default_path() -> PathBuf {
        gtk::glib::user_config_dir()
            .join("signal-tray")
            .join("config.toml")
    }

    /// A missing file yields the defaults; a malformed one is an error so
    /// typos don't get silently ignored.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let config = match std::fs::read_to_string(path) {
            Ok(text) => {
                Self::parse(&text).with_context(|| format!("parsing {}", path.display()))?
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        Ok(config)
    }

    fn parse(text: &str) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(text)?;
        for (i, account) in config.accounts.iter().enumerate() {
            if config.accounts[..i].iter().any(|a| a.id == account.id) {
                anyhow::bail!("account id {:?} is listed twice", account.id.as_str());
            }
        }
        Ok(config)
    }

    pub fn account(&self, id: &AccountId) -> Option<&AccountConfig> {
        self.accounts.iter().find(|a| &a.id == id)
    }

    pub fn account_mut(&mut self, id: &AccountId) -> Option<&mut AccountConfig> {
        self.accounts.iter_mut().find(|a| &a.id == id)
    }

    /// Adds a linked account with the next free palette colour.
    pub fn add_account(&mut self, id: AccountId) {
        let used: Vec<Color> = self.accounts.iter().map(|a| a.color).collect();
        self.accounts.push(AccountConfig {
            id,
            name: String::new(),
            color: Color::next_unused(&used),
        });
    }

    /// Writes the whole config, replacing the file atomically so a crash
    /// can't leave a truncated config that fails to load. Comments in a
    /// hand-written file are not preserved.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let dir = path
            .parent()
            .context("config path has a parent directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
}

pub fn data_dir() -> PathBuf {
    gtk::glib::user_data_dir().join("signal-tray")
}

/// Creates the data directory, private to the user: it holds Signal keys
/// and messages.
pub fn ensure_private_data_dir() -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = data_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_uses_defaults() {
        let config: Config = toml::from_str("corner = \"top-left\"\nwidth = 400\n")
            .expect("valid partial config must parse");
        assert_eq!(config.corner, Corner::TopLeft);
        assert_eq!(config.width, 400);
        assert_eq!(config.height, Config::default().height);
    }

    #[test]
    fn margin_is_one_number_or_a_pair() {
        let margin = |text: &str| {
            toml::from_str::<Config>(text)
                .expect("valid margin must parse")
                .margin
        };
        assert_eq!(
            margin("margin = 4\n"),
            Margin {
                horizontal: 4,
                vertical: 4
            }
        );
        assert_eq!(
            margin("margin = [4, 30]\n"),
            Margin {
                horizontal: 4,
                vertical: 30
            }
        );
    }

    #[test]
    fn opacity_out_of_range_is_rejected() {
        assert!(toml::from_str::<Config>("opacity = 0.5\n").is_ok());
        assert!(toml::from_str::<Config>("opacity = 1.5\n").is_err());
        assert!(toml::from_str::<Config>("opacity = 0.0\n").is_err());
    }

    #[test]
    fn saved_config_loads_back() {
        let dir = std::env::temp_dir().join(format!("signal-tray-config-{}", std::process::id()));
        let path = dir.join("config.toml");
        let config = Config {
            corner: Corner::TopLeft,
            margin: Margin {
                horizontal: 120,
                vertical: 8,
            },
            opacity: Opacity::try_from(0.55).expect("in range"),
            ..Config::default()
        };
        config.save(&path).expect("config saves");
        let loaded = Config::load(&path).expect("saved config loads");
        assert_eq!(
            (loaded.corner, loaded.margin, loaded.opacity),
            (config.corner, config.margin, config.opacity)
        );
        std::fs::remove_dir_all(&dir).expect("temp dir is removable");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>("widht = 400\n").is_err());
        assert!(
            Config::parse("[[accounts]]\nid = \"1\"\ncolor = \"#000000\"\ncolour = 1\n").is_err()
        );
    }

    #[test]
    fn accounts_round_trip() {
        let dir = std::env::temp_dir().join(format!("signal-tray-accounts-{}", std::process::id()));
        let path = dir.join("config.toml");
        let mut config = Config::default();
        config.add_account(AccountId::try_from("1".to_owned()).expect("valid id"));
        config.add_account(AccountId::try_from("2".to_owned()).expect("valid id"));
        config.accounts[1].name = "Work".to_owned();
        config.save(&path).expect("config saves");
        let loaded = Config::load(&path).expect("saved config loads");
        assert_eq!(loaded.accounts, config.accounts);
        assert_eq!(
            loaded.accounts.iter().map(|a| a.color).collect::<Vec<_>>(),
            Color::PALETTE[..2],
            "new accounts get distinct palette colours"
        );
        std::fs::remove_dir_all(&dir).expect("temp dir is removable");
    }

    #[test]
    fn duplicate_or_malformed_accounts_are_rejected() {
        let account =
            |id: &str, color: &str| format!("[[accounts]]\nid = {id:?}\ncolor = {color:?}\n");
        assert!(Config::parse(&account("1", "#3A76f0")).is_ok());
        assert!(Config::parse(&(account("1", "#000000") + &account("1", "#ffffff"))).is_err());
        for color in ["3a76f0", "#3a76f", "#3a76f0ff", "#gggggg", "blue"] {
            assert!(Config::parse(&account("1", color)).is_err(), "{color:?}");
        }
        assert!(Config::parse(&account("a/b", "#000000")).is_err());
    }

    #[test]
    fn palette_skips_used_colours_and_cycles() {
        assert_eq!(Color::next_unused(&[]), Color::PALETTE[0]);
        assert_eq!(
            Color::next_unused(&[Color::PALETTE[0], Color::PALETTE[2]]),
            Color::PALETTE[1]
        );
        assert_eq!(Color::next_unused(&Color::PALETTE), Color::PALETTE[0]);
    }

    #[test]
    fn text_colour_contrasts_with_background() {
        assert_eq!(Color::PALETTE[0].text_color(), Color::WHITE);
        assert_eq!(Color::rgb(0x11, 0x11, 0x11).text_color(), Color::WHITE);
        assert_eq!(Color::rgb(0xf5, 0xc4, 0x00).text_color(), Color::BLACK);
        assert_eq!(Color::WHITE.text_color(), Color::BLACK);
    }
}
