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

use smithay::backend::input::ButtonState;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerHandle};
use smithay::input::Seat;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER};

use crate::output::OutputExt;
use crate::state::{Fht, State};
use crate::utils::get_monotonic_time;

impl Fht {
    /// Constrain this location to the closest output.
    ///
    /// Returns `None` if there are no outputs.
    fn constrain_to_outputs(&self, location: Point<f64, Logical>) -> Option<Point<f64, Logical>> {
        let constrained = self.space.outputs().map(|output| {
            let rect = output.geometry().to_f64();
            let mut constrained = location.constrain(rect);
            // The right and bottom edges are not part of the output.
            constrained.x = f64::min(constrained.x, rect.loc.x + rect.size.w - 1.0);
            constrained.y = f64::min(constrained.y, rect.loc.y + rect.size.h - 1.0);
            constrained
        });

        constrained.min_by(|a, b| {
            let d1 = f64::hypot(a.x - location.x, a.y - location.y);
            let d2 = f64::hypot(b.x - location.x, b.y - location.y);
            f64::total_cmp(&d1, &d2)
        })
    }

    /// Get the location of the pointer of a transient seat.
    ///
    /// Pointers of transient seats start at (0, 0), which is not necessarily inside an output, so
    /// they get placed in the middle of the active output the first time they get used.
    fn transient_pointer_location(&self, pointer: &PointerHandle<State>) -> Point<f64, Logical> {
        let location = pointer.current_location();
        let inside_space = self
            .space
            .outputs()
            .any(|output| output.geometry().to_f64().contains(location));
        if inside_space {
            return location;
        }

        let rect = self.space.active_output().geometry().to_f64();
        (
            rect.loc.x + rect.size.w / 2.0,
            rect.loc.y + rect.size.h / 2.0,
        )
            .into()
    }
}

impl State {
    /// Move the pointer of a transient seat, to a location in global coordinates.
    fn move_transient_pointer(
        &mut self,
        pointer: &PointerHandle<State>,
        location: Point<f64, Logical>,
        time: u32,
    ) {
        let Some(location) = self.fht.constrain_to_outputs(location) else {
            return; // no outputs
        };

        let focus = self.fht.get_pointer_focus(location);
        pointer.motion(
            self,
            focus.and_then(|focus| focus.surface),
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
    }

    /// Move the pointer of a transient seat by a relative amount.
    pub fn transient_pointer_motion(
        &mut self,
        seat: &Seat<State>,
        delta: Point<f64, Logical>,
        time: u32,
    ) {
        let Some(pointer) = seat.get_pointer().filter(|_| !self.fht.is_locked()) else {
            return;
        };

        let location = self.fht.transient_pointer_location(&pointer) + delta;
        self.move_transient_pointer(&pointer, location, time);
    }

    /// Move the pointer of a transient seat to an absolute location.
    ///
    /// `position` is the position within the area covered by the pointer, from (0, 0) to (1, 1).
    /// That area is `output` if any, otherwise the whole space.
    pub fn transient_pointer_motion_absolute(
        &mut self,
        seat: &Seat<State>,
        output: Option<&Output>,
        position: Point<f64, Logical>,
        time: u32,
    ) {
        let Some(pointer) = seat.get_pointer().filter(|_| !self.fht.is_locked()) else {
            return;
        };

        let area = match output {
            Some(output) => Some(output.geometry()),
            None => self
                .fht
                .space
                .outputs()
                .map(OutputExt::geometry)
                .reduce(Rectangle::merge),
        };
        let Some(area) = area.map(Rectangle::to_f64) else {
            return; // no outputs
        };

        let location = Point::from((
            area.loc.x + position.x * area.size.w,
            area.loc.y + position.y * area.size.h,
        ));
        self.move_transient_pointer(&pointer, location, time);
    }

    /// Send a button event through the pointer of a transient seat.
    ///
    /// Pressing a button also gives the keyboard focus of that seat to what's under the pointer.
    pub fn transient_pointer_button(
        &mut self,
        seat: &Seat<State>,
        button: u32,
        state: ButtonState,
        time: u32,
    ) {
        let Some(pointer) = seat.get_pointer().filter(|_| !self.fht.is_locked()) else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();

        if state == ButtonState::Pressed && !pointer.is_grabbed() {
            let focus = self.fht.get_pointer_focus(pointer.current_location());
            let surface = focus.and_then(|focus| {
                if let Some(window) = focus.window {
                    return Some(window.wl_surface().clone());
                }

                focus
                    .layer_surface
                    .filter(|layer| layer.can_receive_keyboard_focus())
                    .map(|layer| layer.wl_surface().clone())
            });

            if let Some(keyboard) = seat.get_keyboard() {
                keyboard.set_focus(self, surface, serial);
            }
        }

        pointer.button(
            self,
            &ButtonEvent {
                button,
                state,
                serial,
                time,
            },
        );
    }

    /// Send an axis (scroll) frame through the pointer of a transient seat.
    pub fn transient_pointer_axis(&mut self, seat: &Seat<State>, frame: AxisFrame) {
        let Some(pointer) = seat.get_pointer().filter(|_| !self.fht.is_locked()) else {
            return;
        };

        pointer.axis(self, frame);
    }

    /// Terminate a group of pointer events of a transient seat.
    pub fn transient_pointer_frame(&mut self, seat: &Seat<State>) {
        let Some(pointer) = seat.get_pointer().filter(|_| !self.fht.is_locked()) else {
            return;
        };

        pointer.frame(self);
    }

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
