//! User configuration from `$XDG_CONFIG_HOME/signal-tray/config.toml`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

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
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
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
    }
}
