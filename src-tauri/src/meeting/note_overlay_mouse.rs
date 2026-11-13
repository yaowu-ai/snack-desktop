//! AppKit routes a transparent NSWindow's entire rectangle to the app. Make only
//! the capsule and its visible close button interactive, including rounded edges.
//! Local + global mouse monitors cover Snack and other apps without a polling loop.

use std::{
    cell::{Cell, RefCell},
    ptr::NonNull,
    rc::Rc,
};

use block2::RcBlock;
use objc2::{rc::Retained, runtime::AnyObject};
use objc2_app_kit::{NSEvent, NSEventMask, NSEventType, NSWindow};
use objc2_foundation::NSPoint;
use tauri::{Manager, WebviewWindow};

use super::geometry;

thread_local! {
    // AppKit monitors and their callbacks belong exclusively to the UI thread.
    static TRACKER: RefCell<Option<Tracker>> = const { RefCell::new(None) };
}

struct Tracker {
    state: Rc<MouseState>,
    local: Retained<AnyObject>,
    global: Retained<AnyObject>,
}

struct MouseState {
    window: Retained<NSWindow>,
    stack_above: Cell<bool>,
    can_dismiss: Cell<bool>,
    pressed_inside: Cell<bool>,
}

impl MouseState {
    fn contains(&self, point: NSPoint) -> bool {
        let height = self
            .window
            .contentView()
            .map_or(geometry::WINDOW_HEIGHT, |view| view.bounds().size.height);
        geometry::contains_control(
            point.x,
            height - point.y,
            self.stack_above.get(),
            self.can_dismiss.get(),
        )
    }

    fn refresh(&self) {
        let inside = self.contains(self.window.convertPointFromScreen(NSEvent::mouseLocation()));
        // Preserve an in-progress button press or native drag until release, even
        // if the pointer crosses a rounded edge or the window changes monitors.
        if NSEvent::pressedMouseButtons() & 1 == 0 {
            self.pressed_inside.set(false);
        }
        let ignore = !inside && !self.pressed_inside.get();
        if self.window.ignoresMouseEvents() != ignore {
            self.window.setIgnoresMouseEvents(ignore);
        }
    }

    fn event(&self, event: &NSEvent) {
        if !self.window.isVisible() {
            self.pressed_inside.set(false);
            return;
        }
        if event.r#type() == NSEventType::LeftMouseDown
            && event.windowNumber() == self.window.windowNumber()
            && !self.window.ignoresMouseEvents()
        {
            // A queued event may follow a layout change or cursor jump. Only an
            // actual control press may keep the window interactive during a drag.
            self.pressed_inside
                .set(self.contains(event.locationInWindow()));
        } else if event.r#type() == NSEventType::LeftMouseUp {
            self.pressed_inside.set(false);
        }
        self.refresh();
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        // These are the exact tokens returned by AppKit's monitor registration.
        unsafe {
            NSEvent::removeMonitor(&self.local);
            NSEvent::removeMonitor(&self.global);
        }
    }
}

pub(super) fn install(window: &WebviewWindow) -> Result<(), String> {
    let handle = window.clone();
    window
        .run_on_main_thread(move || {
            let Ok(pointer) = handle.ns_window() else {
                return;
            };
            // Tauri owns a live NSWindow here; retaining it keeps callbacks valid until uninstall.
            let native = unsafe { Retained::retain(pointer.cast::<NSWindow>()).unwrap() };
            native.setAcceptsMouseMovedEvents(true);
            let state = Rc::new(MouseState {
                window: native,
                stack_above: Cell::new(false),
                can_dismiss: Cell::new(false),
                pressed_inside: Cell::new(false),
            });
            let mask = NSEventMask::MouseMoved
                | NSEventMask::LeftMouseDragged
                | NSEventMask::RightMouseDragged
                | NSEventMask::OtherMouseDragged
                | NSEventMask::LeftMouseDown
                | NSEventMask::LeftMouseUp;
            let local_state = state.clone();
            let local_block = RcBlock::new(move |event: NonNull<NSEvent>| {
                local_state.event(unsafe { event.as_ref() });
                // Observe only: preserve delivery to WKWebView and native dragging.
                event.as_ptr()
            });
            let global_state = state.clone();
            let global_block = RcBlock::new(move |event: NonNull<NSEvent>| {
                global_state.event(unsafe { event.as_ref() });
            });
            let local = unsafe {
                NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &local_block)
            };
            let global =
                NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &global_block);
            match (local, global) {
                (Some(local), Some(global)) => {
                    state.refresh();
                    TRACKER.with(|slot| {
                        *slot.borrow_mut() = Some(Tracker {
                            state,
                            local,
                            global,
                        })
                    });
                }
                (local, global) => {
                    // Do not leave a partial monitor or an unresponsive capsule behind.
                    for token in local.iter().chain(global.iter()) {
                        unsafe {
                            NSEvent::removeMonitor(token);
                        }
                    }
                    crate::logging::write_app_log(
                        handle.app_handle(),
                        "warn",
                        "note-overlay",
                        "unable to install recording overlay mouse monitors",
                        None,
                    );
                }
            }
        })
        .map_err(|error| error.to_string())
}

/// Called on UI during presentation, so a stationary cursor also follows layout changes.
pub(super) fn update(stack_above: bool, can_dismiss: bool) {
    TRACKER.with(|slot| {
        if let Some(tracker) = slot.borrow().as_ref() {
            tracker.state.stack_above.set(stack_above);
            tracker.state.can_dismiss.set(can_dismiss);
            tracker.state.refresh();
        }
    });
}

pub(super) fn uninstall() {
    TRACKER.with(|slot| {
        slot.borrow_mut().take();
    });
}
