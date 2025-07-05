use std::time::Duration;

use smithay::backend::input::ButtonState;
use smithay::desktop::Window;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, GestureHoldBeginEvent,
    GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
    GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
    GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
    RelativeMotionEvent,
};
use smithay::input::touch::{
    self, GrabStartData as TouchGrabStartData, TouchGrab, TouchInnerHandle,
};
use smithay::input::SeatHandler;
use smithay::output::Output;
use smithay::utils::{IsAlive, Logical, Point, Serial, Size, SERIAL_COUNTER};

use crate::input::PointerOrTouchStartData;
use crate::niri::State;

pub struct MoveGrab {
    start_data: PointerOrTouchStartData<State>,
    start_output: Output,
    start_pos_within_output: Point<f64, Logical>,
    last_location: Point<f64, Logical>,
    swipe_location: Point<f64, Logical>,
    window: Window,
    gesture: GestureState,
    enable_view_offset: bool,
    is_swipe_pinch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GestureState {
    Recognizing,
    Move,
    ViewOffset,
}

impl MoveGrab {
    pub fn new(
        state: &mut State,
        start_data: PointerOrTouchStartData<State>,
        window: Window,
        enable_view_offset: bool,
        use_threshold: bool,
        is_swipe_pinch: bool,
    ) -> Option<Self> {
        let (output, pos_within_output) = state.niri.output_under(start_data.location())?;
        let gesture = if use_threshold {
            GestureState::Recognizing
        } else {
            GestureState::Move
        };

        Some(Self {
            last_location: start_data.location(),
            swipe_location: start_data.location(),
            start_data,
            start_output: output.clone(),
            start_pos_within_output: pos_within_output,
            window: window,
            gesture: gesture,
            enable_view_offset: enable_view_offset,
            is_swipe_pinch: is_swipe_pinch,
        })
    }

    pub fn is_move(&self) -> bool {
        self.gesture == GestureState::Move
    }

    fn on_ungrab(&mut self, data: &mut State) {
        let layout = &mut data.niri.layout;
        match self.gesture {
            GestureState::Recognizing => {
                // Activate the window on release. This is most prominent in the overview where
                // windows are not activated on click. In the overview, we also try to do a nice
                // synchronized workspace animation.
                if layout.is_overview_open() {
                    let res = layout.workspaces().find_map(|(mon, ws_idx, ws)| {
                        ws.windows()
                            .any(|w| w.window == self.window)
                            .then(|| (mon.map(|mon| mon.output().clone()), ws_idx))
                    });
                    if let Some((Some(output), ws_idx)) = res {
                        layout.focus_output(&output);
                        layout.toggle_overview_to_workspace(ws_idx);
                    }
                }

                layout.activate_window(&self.window);
            }
            GestureState::Move => layout.interactive_move_end(&self.window),
            GestureState::ViewOffset => {
                layout.view_offset_gesture_end(Some(false));
            }
        }

        if self.start_data.is_pointer() {
            data.niri
                .cursor_manager
                .set_cursor_image(CursorImageStatus::default_named());
        }

        // FIXME: only redraw the window output.

        data.niri.queue_redraw_all();
        if !self.is_swipe_pinch {
            data.niri
                .cursor_manager
                .set_cursor_image(CursorImageStatus::default_named());
        }
    }

    fn begin_move(&mut self, data: &mut State) -> bool {
        if !data.niri.layout.interactive_move_begin(
            self.window.clone(),
            &self.start_output,
            self.start_pos_within_output,
        ) {
            // Can no longer start the move.
            return false;
        }

        self.gesture = GestureState::Move;

        if self.start_data.is_pointer() {
            data.niri
                .cursor_manager
                .set_cursor_image(CursorImageStatus::Named(CursorIcon::Move));
        }

        true
    }

    fn begin_view_offset(&mut self, data: &mut State) -> bool {
        let layout = &mut data.niri.layout;
        let Some((output, ws_idx)) = layout.workspaces().find_map(|(mon, ws_idx, ws)| {
            let ws_idx = ws
                .windows()
                .any(|w| w.window == self.window)
                .then_some(ws_idx)?;
            let output = mon?.output().clone();
            Some((output, ws_idx))
        }) else {
            // Can no longer start the gesture.
            return false;
        };

        layout.view_offset_gesture_begin(&output, Some(ws_idx), false);

        self.gesture = GestureState::ViewOffset;

        if self.start_data.is_pointer() {
            data.niri
                .cursor_manager
                .set_cursor_image(CursorImageStatus::Named(CursorIcon::AllScroll));
        }

        true
    }

    fn on_motion(
        &mut self,
        data: &mut State,
        location: Point<f64, Logical>,
        timestamp: Duration,
    ) -> bool {
        let mut delta = location - self.last_location;
        self.last_location = location;

        // Try to recognize the gesture.
        if self.gesture == GestureState::Recognizing {
            // Check if the window has closed.
            if !self.window.alive() {
                return false;
            }

            // Check if the gesture moved far enough to decide.
            let c = location - self.start_data.location();
            if c.x * c.x + c.y * c.y >= 8. * 8. {
                let is_floating = data
                    .niri
                    .layout
                    .workspaces()
                    .find_map(|(_, _, ws)| {
                        ws.windows()
                            .any(|w| w.window == self.window)
                            .then(|| ws.is_floating(&self.window))
                    })
                    .unwrap_or(false);

                let is_view_offset =
                    self.enable_view_offset && !is_floating && c.x.abs() > c.y.abs();

                let started = if is_view_offset {
                    self.begin_view_offset(data)
                } else {
                    self.begin_move(data)
                };
                if !started {
                    return false;
                }

                // Apply the whole delta that accumulated during recognizing.
                delta = c;
            }
        }

        match self.gesture {
            GestureState::Recognizing => return true,
            GestureState::Move => {
                let Some((output, pos_within_output)) = data.niri.output_under(self.last_location)
                else {
                    return true;
                };
                let output = output.clone();

                let ongoing = data.niri.layout.interactive_move_update(
                    &self.window,
                    delta,
                    output,
                    pos_within_output,
                );
                if ongoing {
                    // FIXME: only redraw the previous and the new output.
                    data.niri.queue_redraw_all();
                    return true;
                }
            }
            GestureState::ViewOffset => {
                let res = data
                    .niri
                    .layout
                    .view_offset_gesture_update(-delta.x, timestamp, false);
                if let Some(output) = res {
                    if let Some(output) = output {
                        data.niri.queue_redraw(&output);
                    }
                    return true;
                }
            }
        }

        false
    }

    fn on_toggle_floating(&mut self, data: &mut State) -> bool {
        if self.gesture == GestureState::ViewOffset {
            return true;
        }

        // Start move if still recognizing.
        if self.gesture == GestureState::Recognizing {
            let Some((output, pos_within_output)) = data.niri.output_under(self.last_location)
            else {
                return false;
            };
            let output = output.clone();

            if !self.begin_move(data) {
                return false;
            }

            // Apply the delta accumulated during recognizing.
            let ongoing = data.niri.layout.interactive_move_update(
                &self.window,
                self.last_location - self.start_data.location(),
                output,
                pos_within_output,
            );
            if !ongoing {
                return false;
            }
        }

        data.niri.layout.toggle_window_floating(Some(&self.window));
        data.niri.queue_redraw_all();

        true
    }
}

impl PointerGrab<State> for MoveGrab {
    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // The pointer should not be moved by swipe and pinch gestures
        if !self.is_swipe_pinch {
            // While the grab is active, no client has pointer focus.
            handle.motion(data, None, event);
        } else if event.serial != Serial::from(0) {
            // Ignore normal pointer motion events if we're in a swipe/pinch
            // gesture
            return;
        }

        if self.window.alive() {
            if let Some((output, pos_within_output)) = data.niri.output_under(event.location) {
                let output = output.clone();
                let event_delta = event.location - self.last_location;
                self.last_location = event.location;

                if self.gesture == GestureState::Recognizing {
                    let c = event.location - self.start_data.location();

                    // Check if the gesture moved far enough to decide.
                    if c.x * c.x + c.y * c.y >= 8. * 8. {
                        self.gesture = GestureState::Move;

                        if !self.is_swipe_pinch {
                            data.niri
                                .cursor_manager
                                .set_cursor_image(CursorImageStatus::Named(CursorIcon::Move));
                        }
                    }
                }

                if self.gesture != GestureState::Move {
                    return;
                }

                let ongoing = data.niri.layout.interactive_move_update(
                    &self.window,
                    event_delta,
                    output,
                    pos_within_output,
                );
                if ongoing {
                    // FIXME: only redraw the previous and the new output.
                    data.niri.queue_redraw_all();
                    return;
                }
            } else {
                return;
            }
        }

        // We asserted `event.serial == Serial::from(0)` above
        // if `is_swipe_pinch` is true. This is not a valid serial.
        let serial = if self.is_swipe_pinch {
            SERIAL_COUNTER.next_serial()
        } else {
            event.serial
        };
        // The move is no longer ongoing.

        let timestamp = Duration::from_millis(u64::from(event.time));
        if !self.on_motion(data, event.location, timestamp) {
            // The gesture is no longer ongoing.
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn relative_motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.relative_motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        let start_data = self.start_data.unwrap_pointer();

        if !handle.current_pressed().contains(&start_data.button) {
            // The button that initiated the grab was released.
            handle.unset_grab(self, data, event.serial, event.time, true);
            return;
        }

        // When moving with the left button, right toggles floating, and vice versa.
        let toggle_floating_button = if start_data.button == 0x110 {
            0x111
        } else {
            0x110
        };
        if event.state != ButtonState::Pressed || event.button != toggle_floating_button {
            return;
        }

        if !self.on_toggle_floating(data) {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        _data: &mut State,
        _handle: &mut PointerInnerHandle<'_, State>,
        _event: &GestureSwipeBeginEvent,
    ) {
        // handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeUpdateEvent,
    ) {
        self.swipe_location += event.delta;
        if let Some(mut global_rect) = data.global_bounding_rectangle() {
            // Shrink by 1 logical pixel, retaining center
            global_rect.loc = global_rect.loc + Point::new(1, 1);
            global_rect.size = global_rect.size - Size::new(2, 2);
            self.swipe_location = self.swipe_location.constrain(global_rect.to_f64());
        }

        PointerGrab::motion(
            self,
            data,
            handle,
            None,
            &MotionEvent {
                location: self.swipe_location,
                serial: Serial::from(0),
                time: event.time,
            },
        )
        // handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeEndEvent,
    ) {
        // handle.gesture_swipe_end(data, event);
        handle.unset_grab(self, data, event.serial, event.time, true);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<State> {
        self.start_data.unwrap_pointer()
    }

    fn unset(&mut self, data: &mut State) {
        self.on_ungrab(data);
    }
}

impl TouchGrab<State> for MoveGrab {
    fn down(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::TouchFocus, Point<f64, Logical>)>,
        event: &touch::DownEvent,
        seq: Serial,
    ) {
        handle.down(data, None, event, seq);

        if event.slot == self.start_data.unwrap_touch().slot {
            return;
        }

        if !self.on_toggle_floating(data) {
            handle.unset_grab(self, data);
        }
    }

    fn up(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        event: &touch::UpEvent,
        seq: Serial,
    ) {
        handle.up(data, event, seq);

        if event.slot == self.start_data.unwrap_touch().slot {
            handle.unset_grab(self, data);
        }
    }

    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::TouchFocus, Point<f64, Logical>)>,
        event: &touch::MotionEvent,
        seq: Serial,
    ) {
        handle.motion(data, None, event, seq);

        if event.slot != self.start_data.unwrap_touch().slot {
            return;
        }

        let timestamp = Duration::from_millis(u64::from(event.time));
        if !self.on_motion(data, event.location, timestamp) {
            // The gesture is no longer ongoing.
            handle.unset_grab(self, data);
        }
    }

    fn frame(&mut self, data: &mut State, handle: &mut TouchInnerHandle<'_, State>, seq: Serial) {
        handle.frame(data, seq);
    }

    fn cancel(&mut self, data: &mut State, handle: &mut TouchInnerHandle<'_, State>, seq: Serial) {
        handle.cancel(data, seq);
        handle.unset_grab(self, data);
    }

    fn shape(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        event: &touch::ShapeEvent,
        seq: Serial,
    ) {
        handle.shape(data, event, seq);
    }

    fn orientation(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        event: &touch::OrientationEvent,
        seq: Serial,
    ) {
        handle.orientation(data, event, seq);
    }

    fn start_data(&self) -> &TouchGrabStartData<State> {
        self.start_data.unwrap_touch()
    }

    fn unset(&mut self, data: &mut State) {
        self.on_ungrab(data);
    }
}
