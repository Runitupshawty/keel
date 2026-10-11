//! macOS drag-out: an `NSDraggingSession` begun on Keel's window view with the event that
//! is moving the pointer, one `NSDraggingItem` per file (its `NSURL`, which writes
//! `public.file-url`) with the file's Finder icon as the image.
//!
//! AppKit runs the session on the main thread's run loop, so `start` returns at once and
//! the source's `endedAtPoint` callback reports what the target did. As on Windows, the
//! target copies unless the user holds the platform's move modifier (Command here); Keel's
//! own window is not a target (the in-app drag is over once the pointer left it).

use super::DropEffect;
use anyhow::{anyhow, ensure, Context, Result};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{
    define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEvent, NSEventModifierFlags, NSEventType, NSView, NSWorkspace,
};
use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize, NSString, NSURL};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;

type Done = Box<dyn FnOnce(Result<DropEffect>) + Send>;

/// What the source may let a target do: copy, and move only with Command held (and
/// `allow_move`); nothing inside Keel itself.
pub fn mask(allow_move: bool, outside: bool, command: bool) -> NSDragOperation {
    match (outside, allow_move && command) {
        (false, _) => NSDragOperation::None,
        (true, true) => NSDragOperation::Copy | NSDragOperation::Move,
        (true, false) => NSDragOperation::Copy,
    }
}

/// What the target did, from the session's final operation.
pub fn effect(op: NSDragOperation) -> DropEffect {
    if op.intersects(NSDragOperation::Move | NSDragOperation::Generic) {
        DropEffect::Move
    } else if op.is_empty() {
        DropEffect::None
    } else {
        DropEffect::Copy
    }
}

pub struct Ivars {
    allow_move: bool,
    done: Cell<Option<Done>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and `Source` does not implement
    // `Drop`; AppKit calls the dragging-source methods on the main thread.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KeelDragOutSource"]
    #[ivars = Ivars]
    pub struct Source;

    unsafe impl NSObjectProtocol for Source {}

    unsafe impl NSDraggingSource for Source {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn operation_mask(
            &self,
            _session: &NSDraggingSession,
            context: NSDraggingContext,
        ) -> NSDragOperation {
            let command = NSEvent::modifierFlags_class().contains(NSEventModifierFlags::Command);
            let outside = context == NSDraggingContext::OutsideApplication;
            mask(self.ivars().allow_move, outside, command)
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _at: NSPoint, operation: NSDragOperation) {
            if let Some(done) = self.ivars().done.take() {
                done(Ok(effect(operation)));
            }
        }
    }
);

impl Source {
    fn new(mtm: MainThreadMarker, allow_move: bool, done: Done) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars {
            allow_move,
            done: Cell::new(Some(done)),
        });
        unsafe { msg_send![super(this), init] }
    }
}

thread_local! {
    /// The last drag's source, kept alive while AppKit may still call it (a session does
    /// not retain its source); replaced by the next drag.
    static SOURCE: RefCell<Option<Retained<Source>>> = const { RefCell::new(None) };
}

/// Starts an OS drag of `paths` from Keel's window. Call it on the main thread while the
/// event loop handles the mouse drag (from the UI update); returns at once, and `done`
/// runs on the main thread when the drag ends.
pub fn start(
    paths: Vec<PathBuf>,
    allow_move: bool,
    done: impl FnOnce(Result<DropEffect>) + Send + 'static,
) {
    let Some(mtm) = MainThreadMarker::new() else {
        return done(Err(anyhow!("drag-out must start on the main thread")));
    };
    // The button was released before the UI got here: no drag.
    if NSEvent::pressedMouseButtons() & 1 == 0 {
        return done(Ok(DropEffect::None));
    }
    let (view, event) = match drag_event(mtm) {
        Ok(found) => found,
        Err(e) => return done(Err(e)),
    };
    let at = view.convertPoint_fromView(event.locationInWindow(), None);
    let items: Vec<_> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| item(p, i, at))
        .collect();
    let source = Source::new(mtm, allow_move, Box::new(done));
    view.beginDraggingSessionWithItems_event_source(
        &NSArray::from_retained_slice(&items),
        &event,
        ProtocolObject::from_ref(&*source),
    );
    SOURCE.with(|s| *s.borrow_mut() = Some(source));
}

/// The view the mouse went down in and the event moving the pointer: AppKit only begins a
/// drag with a left-button down or dragged event of the window it starts in.
fn drag_event(mtm: MainThreadMarker) -> Result<(Retained<NSView>, Retained<NSEvent>)> {
    let event = NSApplication::sharedApplication(mtm)
        .currentEvent()
        .context("no mouse event to start the drag with")?;
    ensure!(
        matches!(
            event.r#type(),
            NSEventType::LeftMouseDragged | NSEventType::LeftMouseDown
        ),
        "no mouse drag to start the drag with"
    );
    let view = event
        .window(mtm)
        .and_then(|w| w.contentView())
        .context("no window to drag from")?;
    Ok((view, event))
}

/// One file: its URL on the pasteboard, its icon under the pointer (fanned out a little
/// for the first few files).
fn item(path: &std::path::Path, i: usize, at: NSPoint) -> Retained<NSDraggingItem> {
    let path = NSString::from_str(&path.to_string_lossy());
    let url = NSURL::fileURLWithPath(&path);
    let item = NSDraggingItem::initWithPasteboardWriter(
        NSDraggingItem::alloc(),
        ProtocolObject::from_ref(&*url),
    );
    let icon = NSWorkspace::sharedWorkspace().iconForFile(&path);
    let off = 6.0 * i.min(4) as f64;
    let frame = NSRect::new(
        NSPoint::new(at.x - 16.0 + off, at.y - 16.0 - off),
        NSSize::new(32.0, 32.0),
    );
    unsafe { item.setDraggingFrame_contents(frame, Some(&icon)) };
    item
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_unless_command_and_never_inside_keel() {
        let copy = NSDragOperation::Copy;
        assert_eq!(mask(true, true, false), copy);
        assert_eq!(mask(true, true, true), copy | NSDragOperation::Move);
        assert_eq!(mask(false, true, true), copy);
        assert_eq!(mask(true, false, true), NSDragOperation::None);
    }

    #[test]
    fn final_operation_maps_to_the_effect() {
        assert_eq!(effect(NSDragOperation::None), DropEffect::None);
        assert_eq!(effect(NSDragOperation::Copy), DropEffect::Copy);
        assert_eq!(effect(NSDragOperation::Move), DropEffect::Move);
        assert_eq!(effect(NSDragOperation::Generic), DropEffect::Move);
        assert_eq!(effect(NSDragOperation::Link), DropEffect::Copy);
    }
}
