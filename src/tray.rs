//! StatusNotifierItem tray icon (waybar, KDE, GNOME with AppIndicator, ...).

use ksni::blocking::TrayMethods as _;

#[derive(Clone, Copy, Debug)]
pub enum TrayAction {
    Toggle,
    Settings,
    Quit,
}

pub struct Tray {
    actions: async_channel::Sender<TrayAction>,
    unread: u32,
}

/// Owned handle to the running tray service.
pub struct TrayHandle(ksni::blocking::Handle<Tray>);

impl TrayHandle {
    pub fn set_unread(&self, unread: u32) {
        self.0.update(|tray| tray.unread = unread);
    }
}

/// Returns `None` if no tray host is available; the app still works via the
/// toggle command then.
pub fn spawn(actions: async_channel::Sender<TrayAction>) -> Option<TrayHandle> {
    let tray = Tray { actions, unread: 0 };
    match tray.assume_sni_available(true).spawn() {
        Ok(handle) => Some(TrayHandle(handle)),
        Err(err) => {
            tracing::warn!(%err, "tray icon unavailable");
            None
        }
    }
}

impl Tray {
    fn send(&self, action: TrayAction) {
        // Fails only once the app is shutting down.
        let _ = self.actions.try_send(action);
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "signal-tray".into()
    }

    fn title(&self) -> String {
        "Signal".into()
    }

    fn status(&self) -> ksni::Status {
        if self.unread > 0 {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        [22, 32, 48, 64]
            .into_iter()
            .map(|size| render_icon(size, self.unread > 0))
            .collect()
    }

    fn attention_icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon_pixmap()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: match self.unread {
                0 => "Signal".into(),
                n => format!("Signal – {n} unread"),
            },
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayAction::Toggle);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Show / Hide".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayAction::Toggle)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Settings…".into(),
                icon_name: "preferences-system".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayAction::Settings)),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayAction::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

const SIGNAL_BLUE: [u8; 3] = [0x3a, 0x76, 0xf0];
const BADGE_RED: [u8; 3] = [0xe5, 0x39, 0x35];

/// Draws a speech bubble on a blue disc, plus a red badge for unread
/// messages. Supersampled for cheap anti-aliasing.
fn render_icon(size: i32, badge: bool) -> ksni::Icon {
    const SAMPLES: i32 = 4;
    let s = f64::from(size);
    let mut data =
        Vec::with_capacity(usize::try_from(size * size * 4).expect("icon size is small"));
    for y in 0..size {
        for x in 0..size {
            let mut acc = [0.0f64; 4]; // premultiplied a, r, g, b
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = (f64::from(x) + (f64::from(sx) + 0.5) / f64::from(SAMPLES)) / s;
                    let py = (f64::from(y) + (f64::from(sy) + 0.5) / f64::from(SAMPLES)) / s;
                    if let Some(rgb) = sample(px, py, badge) {
                        acc[0] += 1.0;
                        for c in 0..3 {
                            acc[c + 1] += f64::from(rgb[c]);
                        }
                    }
                }
            }
            let n = f64::from(SAMPLES * SAMPLES);
            let alpha = acc[0] / n;
            let channel = |v: f64| {
                if acc[0] > 0.0 {
                    (v / acc[0]).round() as u8
                } else {
                    0
                }
            };
            data.extend([
                (alpha * 255.0).round() as u8,
                channel(acc[1]),
                channel(acc[2]),
                channel(acc[3]),
            ]);
        }
    }
    ksni::Icon {
        width: size,
        height: size,
        data,
    }
}

/// Colour at normalized coordinates, `None` for transparent.
fn sample(x: f64, y: f64, badge: bool) -> Option<[u8; 3]> {
    let inside = |cx: f64, cy: f64, r: f64| (x - cx).powi(2) + (y - cy).powi(2) <= r * r;
    if badge && inside(0.80, 0.20, 0.20) {
        return Some(BADGE_RED);
    }
    if !inside(0.5, 0.5, 0.48) {
        return None;
    }
    let bubble = inside(0.52, 0.46, 0.25);
    // Tail pointing to the lower left.
    let tail = in_triangle((x, y), (0.30, 0.56), (0.46, 0.68), (0.22, 0.80));
    Some(if bubble || tail {
        [0xff; 3]
    } else {
        SIGNAL_BLUE
    })
}

fn in_triangle(p: (f64, f64), a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> bool {
    let side = |u: (f64, f64), v: (f64, f64)| (v.0 - u.0) * (p.1 - u.1) - (v.1 - u.1) * (p.0 - u.0);
    let (ab, bc, ca) = (side(a, b), side(b, c), side(c, a));
    (ab >= 0.0 && bc >= 0.0 && ca >= 0.0) || (ab <= 0.0 && bc <= 0.0 && ca <= 0.0)
}
