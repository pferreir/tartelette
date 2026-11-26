//! The pointer captured by a mouse pad, so that the emulated mouse moves
//! as far as the real one does instead of only across the pad.
//!
//! Natively the cursor is hidden and locked, or confined to the window
//! where locking isn't supported (X11, Windows), and the motion comes from
//! the raw mouse motion egui forwards ([`egui::Event::MouseMoved`]), which
//! the window's edges don't stop. In a browser it is the Pointer Lock API,
//! with the motion from `mousemove`'s `movementX` and `movementY`; the
//! browser takes Escape back itself.
//!
//! A pad that holds the capture says so every frame ([`keep`]); one that
//! is no longer drawn (another board, another run) lets it go at the end of
//! the frame ([`end_frame`]).
use egui::{Context, Id, Vec2};

#[derive(Clone, Copy)]
struct Held {
    part: usize,
    kept: bool,
    /// Web: the lock has come into force at least once, and when it was
    /// asked for (UI time).
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    locked: bool,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    asked: f64,
}

fn id() -> Id {
    Id::unique("panel-pointer-grab")
}

fn held(ctx: &Context) -> Option<Held> {
    ctx.data(|d| d.get_temp::<Held>(id()))
}

fn set(ctx: &Context, h: Option<Held>) {
    ctx.data_mut(|d| match h {
        Some(h) => {
            d.insert_temp(id(), h);
        }
        None => d.remove::<Held>(id()),
    });
}

/// The part whose pad holds the pointer, if any.
pub fn holder(ctx: &Context) -> Option<usize> {
    #[cfg_attr(not(target_arch = "wasm32"), allow(unused_mut))]
    let mut h = held(ctx)?;
    #[cfg(target_arch = "wasm32")]
    {
        if web::locked() {
            h.locked = true;
            set(ctx, Some(h));
        } else if h.locked || ctx.input(|i| i.time) - h.asked > 2.0 {
            // Escape, or the browser refused.
            set(ctx, None);
            return None;
        }
    }
    Some(h.part)
}

/// Capture the pointer for `part`'s pad.
pub fn take(ctx: &Context, part: usize) {
    let asked = ctx.input(|i| i.time);
    set(
        ctx,
        Some(Held {
            part,
            kept: true,
            locked: false,
            asked,
        }),
    );
    #[cfg(target_arch = "wasm32")]
    web::lock();
    #[cfg(not(target_arch = "wasm32"))]
    {
        use egui::{ViewportCommand, viewport::CursorGrab};
        // Locked where the platform can, else confined: a grab mode the
        // platform lacks is refused and leaves the one before.
        ctx.send_viewport_cmd(ViewportCommand::CursorGrab(CursorGrab::Confined));
        ctx.send_viewport_cmd(ViewportCommand::CursorGrab(CursorGrab::Locked));
        ctx.send_viewport_cmd(ViewportCommand::CursorVisible(false));
    }
}

/// Give the pointer back.
pub fn release(ctx: &Context) {
    if held(ctx).is_none() {
        return;
    }
    set(ctx, None);
    #[cfg(target_arch = "wasm32")]
    web::unlock();
    #[cfg(not(target_arch = "wasm32"))]
    {
        use egui::{ViewportCommand, viewport::CursorGrab};
        ctx.send_viewport_cmd(ViewportCommand::CursorGrab(CursorGrab::None));
        ctx.send_viewport_cmd(ViewportCommand::CursorVisible(true));
    }
}

/// The holder is still there this frame, at `home`: a confined cursor that
/// has wandered off is put back there, hidden, so that a click can't land
/// on something else in the window. (A locked one doesn't move.)
pub fn keep(ctx: &Context, home: egui::Pos2) {
    if let Some(mut h) = held(ctx) {
        h.kept = true;
        set(ctx, Some(h));
    }
    #[cfg(not(target_arch = "wasm32"))]
    if ctx
        .input(|i| i.pointer.latest_pos())
        .is_some_and(|p| p.distance(home) > 8.0)
    {
        ctx.send_viewport_cmd(egui::ViewportCommand::CursorPosition(home));
    }
    #[cfg(target_arch = "wasm32")]
    let _ = home;
}

/// After the panel is drawn: let go of a capture no pad kept.
pub fn end_frame(ctx: &Context) {
    match held(ctx) {
        Some(h) if !h.kept => release(ctx),
        Some(mut h) => {
            h.kept = false;
            set(ctx, Some(h));
        }
        None => {}
    }
}

/// How far the mouse moved since the last call, in its own units (about a
/// pixel each).
pub fn motion(ctx: &Context) -> Vec2 {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = ctx;
        web::take_motion()
    }
    #[cfg(not(target_arch = "wasm32"))]
    ctx.input(|i| {
        i.events
            .iter()
            .filter_map(|e| match e {
                egui::Event::MouseMoved(d) => Some(*d),
                _ => None,
            })
            .fold(Vec2::ZERO, |a, d| a + d)
    })
}

#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::Cell;

    use eframe::{
        wasm_bindgen::{JsCast as _, closure::Closure},
        web_sys::{self, MouseEvent},
    };
    use egui::{Vec2, vec2};

    thread_local! {
        static MOTION: Cell<Vec2> = const { Cell::new(Vec2::ZERO) };
        static LISTENING: Cell<bool> = const { Cell::new(false) };
    }

    fn document() -> Option<web_sys::Document> {
        web_sys::window()?.document()
    }

    pub fn locked() -> bool {
        document().is_some_and(|d| d.pointer_lock_element().is_some())
    }

    pub fn lock() {
        let Some(doc) = document() else {
            return;
        };
        if !LISTENING.get() {
            LISTENING.set(true);
            let on_move = Closure::<dyn FnMut(MouseEvent)>::new(|e: MouseEvent| {
                if locked() {
                    let d = vec2(e.movement_x() as f32, e.movement_y() as f32);
                    MOTION.set(MOTION.get() + d);
                }
            });
            let _ =
                doc.add_event_listener_with_callback("mousemove", on_move.as_ref().unchecked_ref());
            // For as long as the page is open.
            on_move.forget();
        }
        MOTION.set(Vec2::ZERO);
        // The click that asked for it is recent enough to count as the
        // user's gesture the browser wants.
        if let Some(canvas) = doc.get_element_by_id("app") {
            canvas.request_pointer_lock();
        }
    }

    pub fn unlock() {
        if let Some(doc) = document() {
            doc.exit_pointer_lock();
        }
    }

    pub fn take_motion() -> Vec2 {
        MOTION.replace(Vec2::ZERO)
    }
}
