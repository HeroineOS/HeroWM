//! `ext-transient-seat-v1` support.
//!
//! Transient seats are additional `wl_seat`s that only live as long as the client that requested
//! them keeps its `ext_transient_seat_v1` object around. They are not backed by any physical
//! device: their input comes from virtual input protocols (`virtual-keyboard-v1` and
//! `wlr-virtual-pointer-v1`), and they have their own keyboard focus, pointer position, and
//! selections, independent of the primary seat (the one of the user).
//!
//! This lets a program (remote desktop, automation, ...) drive applications without stealing the
//! keyboard focus or cursor of the user. See [`crate::input::transient_seat`] for how the input of
//! these seats gets handled.

use smithay::input::keyboard::XkbConfig;
use smithay::input::pointer::MotionEvent;
use smithay::input::Seat;
use smithay::reexports::wayland_protocols::ext::transient_seat::v1::server::ext_transient_seat_manager_v1::{self, ExtTransientSeatManagerV1};
use smithay::reexports::wayland_protocols::ext::transient_seat::v1::server::ext_transient_seat_v1::{self, ExtTransientSeatV1};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
};
use smithay::utils::SERIAL_COUNTER;

use crate::state::{Fht, State};
use crate::utils::get_monotonic_time;

const VERSION: u32 = 1;

/// The maximum amount of transient seats a single client can hold at the same time.
///
/// Every transient seat is a new global advertised to all clients, so don't let a client spam them.
const MAX_SEATS_PER_CLIENT: usize = 8;

pub struct TransientSeatGlobalData {
    filter: Box<dyn Fn(&Client) -> bool + Send + Sync>,
}

/// Data attached to every `ext_transient_seat_v1` resource.
#[derive(Debug)]
pub struct TransientSeatData {
    /// The seat created for this resource, or `None` if the creation got denied.
    seat: Option<Seat<State>>,
}

#[derive(Debug)]
struct TransientSeat {
    seat: Seat<State>,
    /// The client that holds the `ext_transient_seat_v1` object of this seat.
    owner: ClientId,
}

#[derive(Debug)]
pub struct TransientSeatState {
    seats: Vec<TransientSeat>,
    /// Used to give a unique name to each transient seat.
    next_id: u32,
}

impl TransientSeatState {
    /// Create the global and return a new state object.
    ///
    /// `filter` controls which clients may bind the global.
    pub fn new<F>(display: &DisplayHandle, filter: F) -> Self
    where
        F: Fn(&Client) -> bool + Send + Sync + 'static,
    {
        let global_data = TransientSeatGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<State, ExtTransientSeatManagerV1, _>(VERSION, global_data);

        Self {
            seats: Vec::new(),
            next_id: 0,
        }
    }

    /// Iterate over all the transient seats.
    pub fn seats(&self) -> impl Iterator<Item = &Seat<State>> {
        self.seats.iter().map(|transient| &transient.seat)
    }

    /// Whether this seat is a transient seat.
    pub fn contains(&self, seat: &Seat<State>) -> bool {
        self.seats().any(|transient| transient == seat)
    }
}

impl Fht {
    /// Create a new transient seat for this client, and returns it alongside the name of its
    /// `wl_seat` global for the client to bind.
    ///
    /// Returns `None` if the client is not allowed to create more seats.
    fn add_transient_seat(
        &mut self,
        client: &Client,
        manager: &ExtTransientSeatManagerV1,
    ) -> Option<(Seat<State>, u32)> {
        let owner = client.id();
        let held = self
            .transient_seat_state
            .seats
            .iter()
            .filter(|transient| transient.owner == owner)
            .count();
        if held >= MAX_SEATS_PER_CLIENT {
            warn!(?owner, "Client holds too many transient seats");
            return None;
        }

        let id = self.transient_seat_state.next_id;
        self.transient_seat_state.next_id += 1;

        let (mut seat, name) = registry_name::capture(&self.display_handle, manager, || {
            self.seat_state
                .new_wl_seat(&self.display_handle, format!("transient-seat-{id}"))
        });

        // The virtual keyboard protocol expects the seat to have a keyboard, and the virtual
        // keyboard replaces the keymap anyway, so just use the same repeat info as the user.
        let keyboard_config = &self.config.input.keyboard;
        let res = seat.add_keyboard(
            XkbConfig::default(),
            keyboard_config.repeat_delay.get() as i32,
            keyboard_config.repeat_rate.get(),
        );
        if let Err(err) = res {
            warn!(?err, "Failed to add keyboard to transient seat");
            self.destroy_transient_seat_global(&seat);
            return None;
        }
        seat.add_pointer();

        let Some(name) = name else {
            warn!("Client cannot see the wl_seat global of its own transient seat");
            self.destroy_transient_seat_global(&seat);
            return None;
        };

        self.transient_seat_state.seats.push(TransientSeat {
            seat: seat.clone(),
            owner,
        });

        Some((seat, name))
    }

    fn destroy_transient_seat_global(&mut self, seat: &Seat<State>) {
        if let Some(global) = seat.global() {
            self.display_handle.remove_global::<State>(global);
        }
    }
}

impl State {
    /// Remove a transient seat, once its client is done with it.
    fn remove_transient_seat(&mut self, seat: &Seat<State>) {
        let transient_seats = &mut self.fht.transient_seat_state.seats;
        let Some(idx) = transient_seats.iter().position(|t| &t.seat == seat) else {
            return;
        };
        let TransientSeat { mut seat, .. } = transient_seats.swap_remove(idx);

        // Make sure that clients see the seat leave their surfaces, and that no grab (popups,
        // drag and drop, ...) outlives the seat.
        let time = get_monotonic_time().as_millis() as u32;
        if let Some(keyboard) = seat.get_keyboard() {
            keyboard.unset_grab(self);
            keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
        }
        if let Some(pointer) = seat.get_pointer() {
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

        self.fht.destroy_transient_seat_global(&seat);
        // Smithay never forgets about seats created with SeatState::new_seat, at least don't hold
        // the keyboard state for nothing.
        seat.remove_keyboard();
        seat.remove_pointer();
    }
}

impl GlobalDispatch<ExtTransientSeatManagerV1, TransientSeatGlobalData, State>
    for TransientSeatState
{
    fn bind(
        _state: &mut State,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ExtTransientSeatManagerV1>,
        _global_data: &TransientSeatGlobalData,
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(resource, ());
    }

    fn can_view(client: Client, global_data: &TransientSeatGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl Dispatch<ExtTransientSeatManagerV1, (), State> for TransientSeatState {
    fn request(
        state: &mut State,
        client: &Client,
        manager: &ExtTransientSeatManagerV1,
        request: ext_transient_seat_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            ext_transient_seat_manager_v1::Request::Create { seat } => {
                match state.fht.add_transient_seat(client, manager) {
                    Some((new_seat, name)) => {
                        let resource = data_init.init(
                            seat,
                            TransientSeatData {
                                seat: Some(new_seat),
                            },
                        );
                        resource.ready(name);
                    }
                    None => {
                        let resource = data_init.init(seat, TransientSeatData { seat: None });
                        resource.denied();
                    }
                }
            }
            ext_transient_seat_manager_v1::Request::Destroy => {
                // Seats created by the manager remain valid until they are destroyed themselves.
            }
            #[allow(unreachable_patterns)]
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ExtTransientSeatV1, TransientSeatData, State> for TransientSeatState {
    fn request(
        _state: &mut State,
        _client: &Client,
        _resource: &ExtTransientSeatV1,
        request: ext_transient_seat_v1::Request,
        _data: &TransientSeatData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            ext_transient_seat_v1::Request::Destroy => {
                // The seat gets removed in `destroyed`, this also handles clients disconnecting.
            }
            #[allow(unreachable_patterns)]
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut State,
        _client: ClientId,
        _resource: &ExtTransientSeatV1,
        data: &TransientSeatData,
    ) {
        if let Some(seat) = &data.seat {
            debug!(name = %seat.name(), "Removing transient seat");
            state.remove_transient_seat(seat);
        }
    }
}

/// Finding out the name of a new global in the registry of a client.
///
/// The obvious way to do this is `Handle::global_name`, however, when a global filter is installed
/// (which wayland-backend always does), it deadlocks: it holds the lock of the backend state while
/// libwayland calls the filter, which needs that same lock.
///
/// Instead, this uses a libwayland protocol logger to look at the `wl_registry.global` event that
/// gets sent to the client when the global is created.
mod registry_name {
    use std::ffi::{c_char, c_int, c_uint, c_void, CStr};

    use smithay::reexports::wayland_server::{DisplayHandle, Resource};

    /// `WL_PROTOCOL_LOGGER_EVENT` from `enum wl_protocol_logger_type`.
    const LOGGER_EVENT: c_uint = 1;

    /// `struct wl_message`
    #[repr(C)]
    struct WlMessage {
        name: *const c_char,
        signature: *const c_char,
        types: *const *const c_void,
    }

    /// `union wl_argument`
    #[repr(C)]
    union WlArgument {
        u: u32,
        s: *const c_char,
        // Make sure that we are the size of the real thing, we only read the above.
        _padding: u64,
    }

    /// `struct wl_protocol_logger_message`
    #[repr(C)]
    struct LoggerMessage {
        resource: *mut c_void,
        message_opcode: c_int,
        message: *const WlMessage,
        arguments_count: c_int,
        arguments: *const WlArgument,
    }

    type LoggerFunc = unsafe extern "C" fn(*mut c_void, c_uint, *const LoggerMessage);

    /// The functions we need from libwayland-server.
    ///
    /// Those are looked up when needed, since wayland-sys loads libwayland-server dynamically, and
    /// only knows about the functions that wayland-backend uses.
    struct LibWayland {
        add_protocol_logger:
            unsafe extern "C" fn(*mut c_void, LoggerFunc, *mut c_void) -> *mut c_void,
        destroy_protocol_logger: unsafe extern "C" fn(*mut c_void),
        resource_get_client: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
        resource_get_class: unsafe extern "C" fn(*mut c_void) -> *const c_char,
    }

    impl LibWayland {
        fn load() -> Option<Self> {
            // SAFETY: we only look for symbols in a library that is loaded already, and cast them
            // to the types they have in wayland-server.h
            unsafe {
                let lib = libc::dlopen(
                    c"libwayland-server.so.0".as_ptr(),
                    libc::RTLD_LAZY | libc::RTLD_NOLOAD,
                );
                if lib.is_null() {
                    return None;
                }

                let symbol = |name: &CStr| {
                    let symbol = libc::dlsym(lib, name.as_ptr());
                    (!symbol.is_null()).then_some(symbol)
                };

                let libwayland = Some(Self {
                    add_protocol_logger: std::mem::transmute(symbol(
                        c"wl_display_add_protocol_logger",
                    )?),
                    destroy_protocol_logger: std::mem::transmute(symbol(
                        c"wl_protocol_logger_destroy",
                    )?),
                    resource_get_client: std::mem::transmute(symbol(c"wl_resource_get_client")?),
                    resource_get_class: std::mem::transmute(symbol(c"wl_resource_get_class")?),
                });

                // Drop the reference we got from dlopen, the library is still loaded by wayland-sys
                libc::dlclose(lib);
                libwayland
            }
        }
    }

    struct Capture {
        /// The `wl_client` we are interested in.
        client: *mut c_void,
        name: Option<u32>,
        libwayland: LibWayland,
    }

    unsafe extern "C" fn log_message(
        user_data: *mut c_void,
        direction: c_uint,
        message: *const LoggerMessage,
    ) {
        // SAFETY: user_data is the Capture that we passed in `capture`, that outlives the logger.
        // libwayland gives us a valid message, and `wl_registry.global` has the signature `usu`.
        unsafe {
            let capture = &mut *user_data.cast::<Capture>();
            let message = &*message;

            if direction != LOGGER_EVENT
                || CStr::from_ptr((*message.message).name) != c"global"
                || CStr::from_ptr((capture.libwayland.resource_get_class)(message.resource))
                    != c"wl_registry"
                || (capture.libwayland.resource_get_client)(message.resource) != capture.client
            {
                return;
            }

            let arguments = std::slice::from_raw_parts(message.arguments, 3);
            if CStr::from_ptr(arguments[1].s) == c"wl_seat" {
                capture.name = Some(arguments[0].u);
            }
        }
    }

    /// Run `create_seat`, that must create a `wl_seat` global, and returns the name of that global
    /// for the client that owns `resource`.
    ///
    /// The name is `None` if the client did not get to see the global.
    pub fn capture<T, R: Resource>(
        display: &DisplayHandle,
        resource: &R,
        create_seat: impl FnOnce() -> T,
    ) -> (T, Option<u32>) {
        let resource = resource.id().as_ptr();
        let (Some(libwayland), false) = (LibWayland::load(), resource.is_null()) else {
            return (create_seat(), None);
        };

        let display = display.backend_handle().display_ptr().cast::<c_void>();
        // SAFETY: the resource is alive since we are handling one of its requests.
        let client = unsafe { (libwayland.resource_get_client)(resource.cast()) };
        let add_protocol_logger = libwayland.add_protocol_logger;
        let destroy_protocol_logger = libwayland.destroy_protocol_logger;

        let mut capture = Capture {
            client,
            name: None,
            libwayland,
        };
        // SAFETY: the logger only lives for the duration of this function, which is as long as
        // the capture. It does not call back into wayland-backend, so it is fine to be called from
        // within `create_global`.
        let logger = unsafe {
            add_protocol_logger(
                display,
                log_message,
                std::ptr::from_mut(&mut capture).cast(),
            )
        };

        let seat = create_seat();

        // SAFETY: the logger was created above, and gets destroyed only once.
        unsafe { destroy_protocol_logger(logger) };

        (seat, capture.name)
    }
}

#[macro_export]
macro_rules! delegate_transient_seat {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty:ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!(
            $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
                smithay::reexports::wayland_protocols::ext::transient_seat::v1::server::ext_transient_seat_manager_v1::ExtTransientSeatManagerV1:
                    $crate::protocols::transient_seat::TransientSeatGlobalData
            ] => $crate::protocols::transient_seat::TransientSeatState
        );

        smithay::reexports::wayland_server::delegate_dispatch!(
            $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
                smithay::reexports::wayland_protocols::ext::transient_seat::v1::server::ext_transient_seat_manager_v1::ExtTransientSeatManagerV1: ()
            ] => $crate::protocols::transient_seat::TransientSeatState
        );

        smithay::reexports::wayland_server::delegate_dispatch!(
            $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
                smithay::reexports::wayland_protocols::ext::transient_seat::v1::server::ext_transient_seat_v1::ExtTransientSeatV1:
                    $crate::protocols::transient_seat::TransientSeatData
            ] => $crate::protocols::transient_seat::TransientSeatState
        );
    };
}
