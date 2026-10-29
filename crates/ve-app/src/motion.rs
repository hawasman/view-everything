//! Functional motion: short ease-out transitions, all of them off under Reduce motion.
//!
//! GPUI's `App::reduce_motion` flag is what every animation consults. `gpui_kit::init` fills it
//! from the operating system (macOS accessibility, Windows animation effects, the GNOME
//! `enable-animations` portal setting); the Settings toggle forces it on, and switching the
//! toggle off returns to what the operating system said at start.

use gpui_kit::*;
use std::time::Duration;

/// How long a transition or fade lasts.
pub const DURATION: Duration = Duration::from_millis(180);
/// How far a panel slides in, in pixels.
const SLIDE: f32 = 14.0;

/// The operating system's Reduce motion preference, as read before the setting was applied.
struct SystemChoice(bool);

impl Global for SystemChoice {}

/// Cubic ease-out: fast at first, settling gently.
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Call once, right after `gpui_kit::init`: remembers the system preference and applies the setting.
pub fn init(reduce: bool, cx: &mut App) {
    let system = cx.reduce_motion();
    cx.set_global(SystemChoice(system));
    apply(reduce, cx);
}

/// Applies the Settings toggle: on forces Reduce motion, off follows the operating system.
pub fn apply(reduce: bool, cx: &mut App) {
    let system = cx.try_global::<SystemChoice>().is_some_and(|s| s.0);
    cx.set_reduce_motion(reduce || system);
}

/// The length of a transition now: zero under Reduce motion, so it never starts.
pub fn duration(cx: &App) -> Duration {
    if cx.reduce_motion() { Duration::ZERO } else { DURATION }
}

/// Fades and slides `el` in over `DURATION` when it first appears, then leaves it alone: no
/// frames are requested once it has settled. Instant under Reduce motion (GPUI renders the end
/// state and schedules nothing).
pub fn slide_in(el: Div, id: impl Into<ElementId>) -> impl IntoElement {
    el.relative()
        .with_animation(id, Animation::new(DURATION).with_easing(ease_out), |el, t| {
            el.opacity(t).top(px(SLIDE * (1.0 - t)))
        })
}
