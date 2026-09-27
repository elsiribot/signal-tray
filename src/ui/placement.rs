//! Where the layer-shell window sits, and moving it with Super+drag.
//!
//! Compositors don't move layer surfaces (sway's mod+drag only applies to
//! regular windows), so the window moves itself by changing its margins.
//! Only enabled on sway: how pointer positions relate to a surface that
//! moves mid-press differs between compositors, and this follows sway.
//! While dragging it is anchored top-left so the margins are plain screen
//! coordinates; on release it snaps its anchor to the nearest corner, which
//! keeps it in place relative to that corner when the resolution changes.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};
use gtk4_layer_shell::{Edge, LayerShell as _};

use crate::config::{Corner, Margin};

/// Holding any of these starts a move. Super matches sway's usual `$mod`; Alt
/// covers setups that use Mod1 instead.
const MOVE_MODIFIERS: gdk::ModifierType = gdk::ModifierType::SUPER_MASK
    .union(gdk::ModifierType::META_MASK)
    .union(gdk::ModifierType::ALT_MASK);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub corner: Corner,
    pub margin: Margin,
}

pub fn apply(window: &gtk::ApplicationWindow, placement: Placement) {
    let (left, top) = corner_edges(placement.corner);
    set_anchor(window, left, top, placement.margin);
}

/// Lets Super/Alt+drag move the window on sway; `on_moved` gets the
/// placement it was dropped at. Clicks without the modifier are left alone.
pub fn enable_dragging(
    window: &gtk::ApplicationWindow,
    placement: Placement,
    on_moved: impl Fn(Placement) + 'static,
) {
    if std::env::var_os("SWAYSOCK").is_none() {
        tracing::info!("not running on sway, so the window can't be dragged");
        return;
    }
    let placement = Rc::new(Cell::new(placement));
    let drag: Rc<RefCell<Option<Drag>>> = Rc::default();
    let gesture = gtk::GestureDrag::builder()
        .button(gdk::BUTTON_PRIMARY)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();

    gesture.connect_drag_begin(glib::clone!(
        #[weak]
        window,
        #[strong]
        placement,
        #[strong]
        drag,
        move |gesture, _, _| {
            let screen = gesture
                .current_event_state()
                .intersects(MOVE_MODIFIERS)
                .then(|| Screen::of(&window))
                .flatten();
            let Some(screen) = screen else {
                gesture.set_state(gtk::EventSequenceState::Denied);
                return;
            };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let start = screen.top_left(placement.get());
            set_anchor(&window, true, true, margin(start));
            *drag.borrow_mut() = Some(Drag {
                screen,
                start,
                target: start,
            });
        }
    ));

    gesture.connect_drag_update(glib::clone!(
        #[weak]
        window,
        #[strong]
        drag,
        move |_, dx, dy| {
            let mut guard = drag.borrow_mut();
            let Some(drag) = guard.as_mut() else { return };
            // Sway reports pointer positions during a press relative to
            // where the surface was when the button went down, so the
            // offset is the whole way the pointer has travelled.
            let target = drag.screen.clamp((
                drag.start.0 + dx.round() as i32,
                drag.start.1 + dy.round() as i32,
            ));
            if target != drag.target {
                drag.target = target;
                set_anchor(&window, true, true, margin(target));
            }
        }
    ));

    gesture.connect_drag_end(glib::clone!(
        #[weak]
        window,
        #[strong]
        placement,
        #[strong]
        drag,
        move |_, _, _| {
            let Some(drag) = drag.borrow_mut().take() else {
                return;
            };
            let dropped = drag.screen.nearest_corner(drag.target);
            apply(&window, dropped);
            if dropped != placement.replace(dropped) {
                on_moved(dropped);
            }
        }
    ));

    window.add_controller(gesture);
}

/// Window position on the monitor, left and top, in logical pixels.
type Position = (i32, i32);

struct Drag {
    screen: Screen,
    /// Where the window was when the drag started.
    start: Position,
    /// Where the window was last moved to.
    target: Position,
}

/// The monitor and window sizes a move is confined to.
#[derive(Clone, Copy, Debug)]
struct Screen {
    width: i32,
    height: i32,
    window_width: i32,
    window_height: i32,
}

impl Screen {
    fn of(window: &gtk::ApplicationWindow) -> Option<Self> {
        let surface = window.surface()?;
        let geometry = surface.display().monitor_at_surface(&surface)?.geometry();
        Some(Self {
            width: geometry.width(),
            height: geometry.height(),
            // The surface, not the widget: the widget size leaves out the
            // window's CSS border.
            window_width: surface.width(),
            window_height: surface.height(),
        })
    }

    fn top_left(self, placement: Placement) -> Position {
        let (left_edge, top_edge) = corner_edges(placement.corner);
        let Margin {
            horizontal,
            vertical,
        } = placement.margin;
        (
            if left_edge {
                horizontal
            } else {
                self.width - self.window_width - horizontal
            },
            if top_edge {
                vertical
            } else {
                self.height - self.window_height - vertical
            },
        )
    }

    /// Keeps the window fully on screen, or at the top-left if it's larger.
    fn clamp(self, (left, top): Position) -> Position {
        (
            left.clamp(0, (self.width - self.window_width).max(0)),
            top.clamp(0, (self.height - self.window_height).max(0)),
        )
    }

    fn nearest_corner(self, (left, top): Position) -> Placement {
        let right = self.width - self.window_width - left;
        let bottom = self.height - self.window_height - top;
        let (left_edge, top_edge) = (left <= right, top <= bottom);
        Placement {
            corner: match (left_edge, top_edge) {
                (true, true) => Corner::TopLeft,
                (false, true) => Corner::TopRight,
                (true, false) => Corner::BottomLeft,
                (false, false) => Corner::BottomRight,
            },
            margin: Margin {
                horizontal: if left_edge { left } else { right },
                vertical: if top_edge { top } else { bottom },
            },
        }
    }
}

/// Whether a corner touches the left and the top edge.
fn corner_edges(corner: Corner) -> (bool, bool) {
    match corner {
        Corner::TopLeft => (true, true),
        Corner::TopRight => (false, true),
        Corner::BottomLeft => (true, false),
        Corner::BottomRight => (false, false),
    }
}

fn margin((horizontal, vertical): Position) -> Margin {
    Margin {
        horizontal,
        vertical,
    }
}

fn set_anchor(window: &gtk::ApplicationWindow, left: bool, top: bool, margin: Margin) {
    let (horizontal, vertical) = (
        if left { Edge::Left } else { Edge::Right },
        if top { Edge::Top } else { Edge::Bottom },
    );
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        window.set_anchor(edge, edge == horizontal || edge == vertical);
    }
    window.set_margin(horizontal, margin.horizontal);
    window.set_margin(vertical, margin.vertical);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Screen = Screen {
        width: 1000,
        height: 800,
        window_width: 300,
        window_height: 500,
    };

    #[test]
    fn position_survives_a_round_trip_through_the_nearest_corner() {
        for (left, top) in [(0, 0), (10, 290), (690, 5), (400, 150), (700, 300)] {
            let placement = SCREEN.nearest_corner((left, top));
            assert_eq!(
                SCREEN.top_left(placement),
                (left, top),
                "dropping at {left},{top} gave {placement:?}"
            );
        }
    }

    #[test]
    fn dropping_snaps_to_the_closest_corner() {
        assert_eq!(
            SCREEN.nearest_corner((650, 280)),
            Placement {
                corner: Corner::BottomRight,
                margin: margin((50, 20)),
            }
        );
        assert_eq!(SCREEN.nearest_corner((40, 10)).corner, Corner::TopLeft);
    }

    #[test]
    fn the_window_stays_on_screen() {
        assert_eq!(SCREEN.clamp((-50, 900)), (0, 300));
        assert_eq!(SCREEN.clamp((800, -1)), (700, 0));
    }
}
