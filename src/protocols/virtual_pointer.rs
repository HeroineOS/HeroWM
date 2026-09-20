//! `wlr-virtual-pointer-unstable-v1` support.
//!
//! Virtual pointers only drive [transient seats](crate::protocols::transient_seat). The primary
//! seat is the seat of the user, virtual pointers created on it are inert, so that programs can
//! never steal the cursor of the user.

use std::sync::Mutex;

use smithay::backend::input::{Axis, AxisSource, ButtonState};
use smithay::input::pointer::AxisFrame;
use smithay::input::Seat;
use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::zwlr_virtual_pointer_manager_v1::{self, ZwlrVirtualPointerManagerV1};
use smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::zwlr_virtual_pointer_v1::{self, ZwlrVirtualPointerV1};
use smithay::reexports::wayland_server::protocol::wl_pointer;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::state::State;

const VERSION: u32 = 2;

pub struct VirtualPointerGlobalData {
    filter: Box<dyn Fn(&Client) -> bool + Send + Sync>,
}

#[derive(Debug)]
pub struct VirtualPointerManagerState;

impl VirtualPointerManagerState {
    /// Create the global and return a new state object.
    ///
    /// `filter` controls which clients may bind the global.
    pub fn new<F>(display: &DisplayHandle, filter: F) -> Self
    where
        F: Fn(&Client) -> bool + Send + Sync + 'static,
    {
        let global_data = VirtualPointerGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<State, ZwlrVirtualPointerManagerV1, _>(VERSION, global_data);
        Self
    }
}

/// The state of a single axis, accumulated until the next `frame` request.
#[derive(Debug, Default, Clone, Copy)]
struct PendingAxis {
    value: f64,
    v120: i32,
    stop: bool,
    /// Whether any request touched this axis since the last frame.
    touched: bool,
}

/// The axis events, accumulated until the next `frame` request.
#[derive(Debug, Default)]
struct PendingFrame {
    time: u32,
    source: Option<AxisSource>,
    horizontal: PendingAxis,
    vertical: PendingAxis,
}

impl PendingFrame {
    fn axis_mut(&mut self, axis: Axis) -> &mut PendingAxis {
        match axis {
            Axis::Horizontal => &mut self.horizontal,
            Axis::Vertical => &mut self.vertical,
        }
    }

    /// Take the pending events as an [`AxisFrame`], if there are any.
    fn take(&mut self) -> Option<AxisFrame> {
        let pending = std::mem::take(self);
        if pending.source.is_none() && !pending.horizontal.touched && !pending.vertical.touched {
            return None;
        }

        let mut frame = AxisFrame::new(pending.time);
        if let Some(source) = pending.source {
            frame = frame.source(source);
        }
        for (axis, pending) in [
            (Axis::Horizontal, pending.horizontal),
            (Axis::Vertical, pending.vertical),
        ] {
            if !pending.touched {
                continue;
            }

            frame = frame.value(axis, pending.value);
            if pending.v120 != 0 {
                frame = frame.v120(axis, pending.v120);
            }
            if pending.stop {
                frame = frame.stop(axis);
            }
        }

        Some(frame)
    }
}

/// Data attached to every `zwlr_virtual_pointer_v1` resource.
#[derive(Debug)]
pub struct VirtualPointerData {
    seat: Seat<State>,
    /// The output the absolute motion events are relative to, if any.
    output: Option<Output>,
    pending: Mutex<PendingFrame>,
}

impl GlobalDispatch<ZwlrVirtualPointerManagerV1, VirtualPointerGlobalData, State>
    for VirtualPointerManagerState
{
    fn bind(
        _state: &mut State,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrVirtualPointerManagerV1>,
        _global_data: &VirtualPointerGlobalData,
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(resource, ());
    }

    fn can_view(client: Client, global_data: &VirtualPointerGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, (), State> for VirtualPointerManagerState {
    fn request(
        state: &mut State,
        _client: &Client,
        _manager: &ZwlrVirtualPointerManagerV1,
        request: zwlr_virtual_pointer_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        let (seat, output, id) = match request {
            zwlr_virtual_pointer_manager_v1::Request::CreateVirtualPointer { seat, id } => {
                (seat, None, id)
            }
            zwlr_virtual_pointer_manager_v1::Request::CreateVirtualPointerWithOutput {
                seat,
                output,
                id,
            } => (seat, output, id),
            zwlr_virtual_pointer_manager_v1::Request::Destroy => {
                // Virtual pointers created by the manager remain valid until they are destroyed.
                return;
            }
            #[allow(unreachable_patterns)]
            _ => unreachable!(),
        };

        // When the client doesn't say which seat it wants, this is the primary one.
        let seat = seat
            .as_ref()
            .and_then(Seat::from_resource)
            .unwrap_or_else(|| state.fht.seat.clone());
        if !state.fht.transient_seat_state.contains(&seat) {
            debug!("Virtual pointer created on the primary seat, its events will be ignored");
        }

        let output = output.as_ref().and_then(Output::from_resource);
        data_init.init(
            id,
            VirtualPointerData {
                seat,
                output,
                pending: Mutex::new(PendingFrame::default()),
            },
        );
    }
}

impl Dispatch<ZwlrVirtualPointerV1, VirtualPointerData, State> for VirtualPointerManagerState {
    fn request(
        state: &mut State,
        _client: &Client,
        resource: &ZwlrVirtualPointerV1,
        request: zwlr_virtual_pointer_v1::Request,
        data: &VirtualPointerData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        use zwlr_virtual_pointer_v1::Request;

        if matches!(request, Request::Destroy) {
            return;
        }

        // Virtual pointers only work on transient seats, this also makes them inert once their
        // seat got removed.
        if !state.fht.transient_seat_state.contains(&data.seat) {
            return;
        }
        let seat = &data.seat;
        let mut pending = data.pending.lock().unwrap();

        match request {
            Request::Motion { time, dx, dy } => {
                state.transient_pointer_motion(seat, (dx, dy).into(), time);
            }
            Request::MotionAbsolute {
                time,
                x,
                y,
                x_extent,
                y_extent,
            } => {
                if x_extent == 0 || y_extent == 0 {
                    return;
                }

                let position = (
                    f64::from(x) / f64::from(x_extent),
                    f64::from(y) / f64::from(y_extent),
                );
                state.transient_pointer_motion_absolute(
                    seat,
                    data.output.as_ref(),
                    position.into(),
                    time,
                );
            }
            Request::Button {
                time,
                button,
                state: button_state,
            } => {
                let button_state = match button_state.into_result() {
                    Ok(wl_pointer::ButtonState::Pressed) => ButtonState::Pressed,
                    Ok(wl_pointer::ButtonState::Released) => ButtonState::Released,
                    _ => return,
                };
                state.transient_pointer_button(seat, button, button_state, time);
            }
            Request::Axis { time, axis, value } => {
                let Some(axis) = convert_axis(resource, axis.into_result().ok()) else {
                    return;
                };

                let pending_axis = pending.axis_mut(axis);
                pending_axis.value += value;
                pending_axis.touched = true;
                pending.time = time;
            }
            Request::AxisDiscrete {
                time,
                axis,
                value,
                discrete,
            } => {
                let Some(axis) = convert_axis(resource, axis.into_result().ok()) else {
                    return;
                };

                let pending_axis = pending.axis_mut(axis);
                pending_axis.value += value;
                pending_axis.v120 += discrete * 120;
                pending_axis.touched = true;
                pending.time = time;
            }
            Request::AxisStop { time, axis } => {
                let Some(axis) = convert_axis(resource, axis.into_result().ok()) else {
                    return;
                };

                let pending_axis = pending.axis_mut(axis);
                pending_axis.stop = true;
                pending_axis.touched = true;
                pending.time = time;
            }
            Request::AxisSource { axis_source } => {
                pending.source = match axis_source.into_result() {
                    Ok(wl_pointer::AxisSource::Wheel) => Some(AxisSource::Wheel),
                    Ok(wl_pointer::AxisSource::Finger) => Some(AxisSource::Finger),
                    Ok(wl_pointer::AxisSource::Continuous) => Some(AxisSource::Continuous),
                    Ok(wl_pointer::AxisSource::WheelTilt) => Some(AxisSource::WheelTilt),
                    _ => {
                        resource.post_error(
                            zwlr_virtual_pointer_v1::Error::InvalidAxisSource,
                            "Invalid axis source",
                        );
                        return;
                    }
                };
            }
            Request::Frame => {
                let frame = pending.take();
                drop(pending);

                if let Some(frame) = frame {
                    state.transient_pointer_axis(seat, frame);
                }
                state.transient_pointer_frame(seat);
            }
            Request::Destroy => unreachable!(),
            #[allow(unreachable_patterns)]
            _ => unreachable!(),
        }
    }
}

fn convert_axis(resource: &ZwlrVirtualPointerV1, axis: Option<wl_pointer::Axis>) -> Option<Axis> {
    match axis {
        Some(wl_pointer::Axis::HorizontalScroll) => Some(Axis::Horizontal),
        Some(wl_pointer::Axis::VerticalScroll) => Some(Axis::Vertical),
        _ => {
            resource.post_error(zwlr_virtual_pointer_v1::Error::InvalidAxis, "Invalid axis");
            None
        }
    }
}

#[macro_export]
macro_rules! delegate_virtual_pointer {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty:ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!(
            $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
                smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1:
                    $crate::protocols::virtual_pointer::VirtualPointerGlobalData
            ] => $crate::protocols::virtual_pointer::VirtualPointerManagerState
        );

        smithay::reexports::wayland_server::delegate_dispatch!(
            $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
                smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1: ()
            ] => $crate::protocols::virtual_pointer::VirtualPointerManagerState
        );

        smithay::reexports::wayland_server::delegate_dispatch!(
            $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
                smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1:
                    $crate::protocols::virtual_pointer::VirtualPointerData
            ] => $crate::protocols::virtual_pointer::VirtualPointerManagerState
        );
    };
}
