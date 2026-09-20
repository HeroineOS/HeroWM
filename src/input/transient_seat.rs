//! Input handling for transient seats.
//!
//! Transient seats (see [`crate::protocols::transient_seat`]) are not driven by the physical
//! devices of the user, but by virtual input protocols, like `wlr-virtual-pointer-v1`. Their input
//! goes through a much simpler path than the input of the primary seat: they only ever deliver
//! events to clients, and must never change what the user sees or is doing.
//!
//! In contrast to the primary seat, transient seats:
//!
//! - Do not activate or raise windows, and do not change the active output or workspace. Clicking
//!   on a window only gives it keyboard focus **for that seat**.
//! - Do not trigger keybinds, mousebinds, or gestures, and do not start interactive grabs.
//! - Do not move (or draw their own) cursor.
//! - Are inert while the session is locked, so they can't type into the lock screen.

use smithay::input::pointer::MotionEvent;
use smithay::utils::SERIAL_COUNTER;

use crate::state::State;
use crate::utils::get_monotonic_time;

impl State {
    /// Refresh the transient seats.
    ///
    /// If the session is locked, take away everything the transient seats are focusing.
    pub fn refresh_transient_seats(&mut self) {
        if !self.fht.is_locked() {
            return;
        }

        let seats: Vec<_> = self.fht.transient_seat_state.seats().cloned().collect();
        let time = get_monotonic_time().as_millis() as u32;
        for seat in seats {
            if let Some(keyboard) = seat.get_keyboard() {
                if keyboard.current_focus().is_some() {
                    keyboard.unset_grab(self);
                    keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
                }
            }

            if let Some(pointer) = seat.get_pointer() {
                if pointer.current_focus().is_some() {
                    pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), time);
                    pointer.motion(
                        self,
                        None,
                        &MotionEvent {
                            location: pointer.current_location(),
                            serial: SERIAL_COUNTER.next_serial(),
                            time,
                        },
                    );
                    pointer.frame(self);
                }
            }
        }
    }
}
