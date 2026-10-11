//! Linux drag-out over XDND (version 5) on X11, or XWayland under a Wayland compositor
//! when Keel itself runs on X11.
//!
//! The drag runs on its own thread with its own X connection and a hidden window as the
//! XDND source: it owns `XdndSelection` and serves the files as `text/uri-list`. Keel's
//! window keeps the mouse (the X server's implicit grab from the button press), so the
//! thread reads the pointer and the Escape key itself every few milliseconds, finds the
//! XDND-aware window under the pointer and talks to it with client messages: `XdndEnter`,
//! `XdndPosition` (copy, or move with Shift held), then `XdndDrop` or `XdndLeave`, and reads
//! the target's `XdndStatus` and `XdndFinished`. There is no drag image or drag cursor
//! (only the grab's owner can set one).
//!
//! A Wayland window has no X pointer to follow and Wayland has no drag without the
//! toolkit's input serial, so in a Wayland session `start` reports that at once.

use super::DropEffect;

/// The XDND version Keel speaks.
pub const VERSION: u32 = 5;

/// `XdndEnter`: the source window, the protocol version (the lower of both sides; the
/// "more than three types" bit stays clear) and the one type offered.
pub fn enter(source: u32, their_version: u32, uri_list: u32) -> [u32; 5] {
    [source, VERSION.min(their_version) << 24, uri_list, 0, 0]
}

/// `XdndPosition`: the pointer in root coordinates (x in the high half), the time to ask
/// for the data with, and the action asked for.
pub fn position(source: u32, (x, y): (i16, i16), time: u32, action: u32) -> [u32; 5] {
    let at = (u32::from(x as u16) << 16) | u32::from(y as u16);
    [source, 0, at, time, action]
}

/// `XdndDrop`.
pub fn drop_message(source: u32, time: u32) -> [u32; 5] {
    [source, 0, time, 0, 0]
}

/// `XdndLeave`.
pub fn leave(source: u32) -> [u32; 5] {
    [source, 0, 0, 0, 0]
}

/// `XdndStatus`: whether the target would take a drop here, and with which action.
pub fn status(data: [u32; 5]) -> (bool, u32) {
    (data[1] & 1 != 0, data[4])
}

/// `XdndFinished`: whether the target took the files, and what it did. Before version 5 the
/// message says neither, so the last status's action stands.
pub fn finished(data: [u32; 5], version: u32, last_action: u32) -> (bool, u32) {
    if version >= 5 {
        (data[1] & 1 != 0, data[2])
    } else {
        (true, last_action)
    }
}

/// What the target did: moved with `XdndActionMove`, else (any other action it took) a copy.
pub fn effect(took: bool, action: u32, move_action: u32) -> DropEffect {
    if !took {
        DropEffect::None
    } else if action == move_action {
        DropEffect::Move
    } else {
        DropEffect::Copy
    }
}

/// Whether `code` is down in a `QueryKeymap` bit vector.
pub fn key_down(keys: &[u8; 32], code: u8) -> bool {
    keys[usize::from(code / 8)] & (1 << (code % 8)) != 0
}

#[cfg(target_os = "linux")]
pub use linux::{run, start};

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use anyhow::{anyhow, ensure, Context, Result};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use x11rb::{
        connection::{Connection, RequestConnection},
        protocol::{xproto::*, Event},
        rust_connection::RustConnection,
        wrapper::ConnectionExt as _,
        COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE,
    };

    /// How often the pointer and the keyboard are read (an Escape tap shorter than this
    /// can be missed; a real key press lasts longer).
    const TICK: Duration = Duration::from_millis(10);
    /// How long a status still owed when the button goes up may take.
    const STATUS_WAIT: Duration = Duration::from_secs(1);
    /// How long a target may take to say it is done with a drop.
    const FINISH_WAIT: Duration = Duration::from_secs(30);
    /// `XK_Escape`.
    const ESCAPE: u32 = 0xff1b;

    /// Starts an OS drag of `paths` and returns at once; `done` runs on the drag thread with
    /// what the target did.
    pub fn start(
        paths: Vec<PathBuf>,
        allow_move: bool,
        done: impl FnOnce(Result<DropEffect>) + Send + 'static,
    ) {
        if crate::drag_out::wayland() {
            return done(Err(anyhow!(
                "dragging files to other apps needs X11 (XWayland) on this desktop"
            )));
        }
        let list = match crate::file_uri::uri_list(&paths) {
            Ok(list) => list.into_bytes(),
            Err(e) => return done(Err(e)),
        };
        let spawned = std::thread::Builder::new()
            .name("keel-drag-out".into())
            .spawn(move || done(run(list, allow_move)));
        if let Err(e) = spawned {
            tracing::error!("spawn keel-drag-out: {e}");
        }
    }

    struct Atoms {
        aware: Atom,
        selection: Atom,
        enter: Atom,
        position: Atom,
        status: Atom,
        leave: Atom,
        drop: Atom,
        finished: Atom,
        copy: Atom,
        move_: Atom,
        uri_list: Atom,
        targets: Atom,
    }

    impl Atoms {
        fn new(conn: &RustConnection) -> Result<Self> {
            let names = [
                "XdndAware",
                "XdndSelection",
                "XdndEnter",
                "XdndPosition",
                "XdndStatus",
                "XdndLeave",
                "XdndDrop",
                "XdndFinished",
                "XdndActionCopy",
                "XdndActionMove",
                "text/uri-list",
                "TARGETS",
            ];
            let cookies = names
                .iter()
                .map(|n| conn.intern_atom(false, n.as_bytes()))
                .collect::<Result<Vec<_>, _>>()?;
            let mut a = Vec::with_capacity(names.len());
            for c in cookies {
                a.push(c.reply()?.atom);
            }
            Ok(Self {
                aware: a[0],
                selection: a[1],
                enter: a[2],
                position: a[3],
                status: a[4],
                leave: a[5],
                drop: a[6],
                finished: a[7],
                copy: a[8],
                move_: a[9],
                uri_list: a[10],
                targets: a[11],
            })
        }
    }

    /// The window under the pointer that takes XDND drops.
    struct Target {
        win: Window,
        version: u32,
        accepted: bool,
        action: Atom,
        /// A position was sent and its status has not come yet (XDND sends one at a time).
        waiting: bool,
    }

    struct Drag {
        conn: RustConnection,
        root: Window,
        win: Window,
        atoms: Atoms,
        list: Vec<u8>,
        /// The server time Keel took the selection at.
        time: u32,
    }

    /// Runs the drag of a `text/uri-list` on the calling thread until the button goes up or
    /// Escape is pressed. A button already up when this starts means no drag.
    pub fn run(list: Vec<u8>, allow_move: bool) -> Result<DropEffect> {
        let (conn, screen) = RustConnection::connect(None).context("connect to the X display")?;
        let root = conn.setup().roots[screen].root;
        let pointer = conn.query_pointer(root)?.reply()?;
        if !pointer.mask.contains(KeyButMask::BUTTON1) {
            return Ok(DropEffect::None);
        }
        let win = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            win,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        let atoms = Atoms::new(&conn)?;
        let time = timestamp(&conn, win, atoms.selection)?;
        conn.set_selection_owner(win, atoms.selection, time)?;
        let owner = conn.get_selection_owner(atoms.selection)?.reply()?.owner;
        ensure!(owner == win, "could not take the drag selection");
        let drag = Drag {
            conn,
            root,
            win,
            atoms,
            list,
            time,
        };
        drag.track(allow_move)
    }

    /// A server timestamp: the time of a property change on our own window.
    fn timestamp(conn: &RustConnection, win: Window, atom: Atom) -> Result<u32> {
        conn.change_property8(PropMode::APPEND, win, atom, AtomEnum::STRING, &[])?;
        conn.flush()?;
        loop {
            if let Event::PropertyNotify(e) = conn.wait_for_event()? {
                if e.window == win {
                    return Ok(e.time);
                }
            }
        }
    }

    impl Drag {
        fn track(&self, allow_move: bool) -> Result<DropEffect> {
            let mut target: Option<Target> = None;
            match self.follow(allow_move, &mut target) {
                Ok(true) => match target {
                    Some(t) => self.drop_on(t),
                    None => Ok(DropEffect::None),
                },
                Ok(false) => Ok(DropEffect::None),
                Err(e) => {
                    // An X error mid-drag: the window under the pointer still follows this
                    // drag; tell it it is over.
                    if let Some(t) = &target {
                        let _ = self.send_leave(t);
                    }
                    Err(e)
                }
            }
        }

        /// Follows the pointer over the windows under it until the button goes up (true)
        /// or Esc cancels (false; the window under it was told).
        fn follow(&self, allow_move: bool, target: &mut Option<Target>) -> Result<bool> {
            let escape = self.escape_keycode()?;
            let mut sent = None;
            loop {
                self.pump(target.as_mut())?;
                let p = self.conn.query_pointer(self.root)?.reply()?;
                let cancel = match escape {
                    Some(code) => key_down(&self.conn.query_keymap()?.reply()?.keys, code),
                    None => false,
                };
                if cancel {
                    if let Some(t) = target.take() {
                        self.send_leave(&t)?;
                    }
                    return Ok(false);
                }
                if !p.mask.contains(KeyButMask::BUTTON1) {
                    return Ok(true);
                }
                let under = self.target_at()?;
                if under.map(|u| u.0) != target.as_ref().map(|t| t.win) {
                    if let Some(t) = target.take() {
                        self.send_leave(&t)?;
                    }
                    if let Some((win, version)) = under {
                        let msg = enter(self.win, version, self.atoms.uri_list);
                        self.send(win, self.atoms.enter, msg)?;
                        *target = Some(Target {
                            win,
                            version: version.min(VERSION),
                            accepted: false,
                            action: NONE,
                            waiting: false,
                        });
                        sent = None;
                    }
                }
                let action = if allow_move && p.mask.contains(KeyButMask::SHIFT) {
                    self.atoms.move_
                } else {
                    self.atoms.copy
                };
                let now = ((p.root_x, p.root_y), action);
                if let Some(t) = target.as_mut().filter(|t| !t.waiting) {
                    if sent != Some(now) {
                        let msg = position(self.win, now.0, self.time, action);
                        self.send(t.win, self.atoms.position, msg)?;
                        t.waiting = true;
                        sent = Some(now);
                    }
                }
                std::thread::sleep(TICK);
            }
        }

        /// The button went up over `t`: drop when it takes the files, else leave it.
        fn drop_on(&self, mut t: Target) -> Result<DropEffect> {
            let until = Instant::now() + STATUS_WAIT;
            while t.waiting && Instant::now() < until {
                self.pump(Some(&mut t))?;
                std::thread::sleep(TICK);
            }
            if !t.accepted {
                self.send_leave(&t)?;
                return Ok(DropEffect::None);
            }
            self.send(t.win, self.atoms.drop, drop_message(self.win, self.time))?;
            let until = Instant::now() + FINISH_WAIT;
            loop {
                match self.conn.poll_for_event()? {
                    Some(Event::ClientMessage(e))
                        if e.type_ == self.atoms.finished && e.data.as_data32()[0] == t.win =>
                    {
                        let (took, action) = finished(e.data.as_data32(), t.version, t.action);
                        return Ok(effect(took, action, self.atoms.move_));
                    }
                    Some(e) => self.handle(e, Some(&mut t))?,
                    None if Instant::now() >= until => {
                        tracing::warn!("drag out: the target never said it was done");
                        return Ok(DropEffect::None);
                    }
                    None => std::thread::sleep(TICK),
                }
            }
        }

        /// Handles the events that came in.
        fn pump(&self, mut target: Option<&mut Target>) -> Result<()> {
            while let Some(e) = self.conn.poll_for_event()? {
                self.handle(e, target.as_deref_mut())?;
            }
            Ok(())
        }

        fn handle(&self, e: Event, target: Option<&mut Target>) -> Result<()> {
            match e {
                Event::SelectionRequest(e) => self.answer(&e),
                Event::ClientMessage(e) if e.type_ == self.atoms.status => {
                    let data = e.data.as_data32();
                    if let Some(t) = target.filter(|t| t.win == data[0]) {
                        (t.accepted, t.action) = status(data);
                        t.waiting = false;
                    }
                    Ok(())
                }
                _ => Ok(()),
            }
        }

        /// Serves the selection: the URI list, and the list of targets.
        fn answer(&self, e: &SelectionRequestEvent) -> Result<()> {
            let property = if e.property == NONE {
                e.target
            } else {
                e.property
            };
            // ponytail: no INCR transfers, so a list longer than one X request (usually
            // 16 MiB, at least 256 KiB) is refused, as on the clipboard.
            let fits = self.list.len() + 64 <= self.conn.maximum_request_bytes();
            let ok = if e.selection != self.atoms.selection {
                false
            } else if e.target == self.atoms.targets {
                let list = [self.atoms.targets, self.atoms.uri_list];
                self.conn.change_property32(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    AtomEnum::ATOM,
                    &list,
                )?;
                true
            } else if e.target == self.atoms.uri_list && fits {
                self.conn.change_property8(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    self.atoms.uri_list,
                    &self.list,
                )?;
                true
            } else {
                false
            };
            let notify = SelectionNotifyEvent {
                response_type: SELECTION_NOTIFY_EVENT,
                sequence: 0,
                time: e.time,
                requestor: e.requestor,
                selection: e.selection,
                target: e.target,
                property: if ok { property } else { NONE },
            };
            self.conn
                .send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
            self.conn.flush()?;
            Ok(())
        }

        /// The top-most window under the pointer with `XdndAware` (version 3 or later),
        /// looked for from the root down (a window manager's frame holds the app's window).
        // ponytail: `XdndProxy` is not followed; a desktop that proxies its root window
        // (icons on the desktop) is not a target.
        fn target_at(&self) -> Result<Option<(Window, u32)>> {
            let mut win = self.root;
            loop {
                let child = self.conn.query_pointer(win)?.reply()?.child;
                if child == NONE || child == self.win {
                    return Ok(None);
                }
                win = child;
                let aware = self
                    .conn
                    .get_property(false, win, self.atoms.aware, AtomEnum::ATOM, 0, 1)?
                    .reply()?;
                if let Some(version) = aware.value32().and_then(|mut v| v.next()) {
                    return Ok((version >= 3).then_some((win, version)));
                }
            }
        }

        fn send(&self, to: Window, kind: Atom, data: [u32; 5]) -> Result<()> {
            let msg = ClientMessageEvent::new(32, to, kind, data);
            self.conn.send_event(false, to, EventMask::NO_EVENT, msg)?;
            self.conn.flush()?;
            Ok(())
        }

        fn send_leave(&self, t: &Target) -> Result<()> {
            self.send(t.win, self.atoms.leave, leave(self.win))
        }

        /// The keycode of Escape on this keyboard.
        fn escape_keycode(&self) -> Result<Option<u8>> {
            let setup = self.conn.setup();
            let (min, max) = (setup.min_keycode, setup.max_keycode);
            let map = self
                .conn
                .get_keyboard_mapping(min, max - min + 1)?
                .reply()?;
            let per = usize::from(map.keysyms_per_keycode).max(1);
            Ok(map
                .keysyms
                .chunks(per)
                .position(|k| k.contains(&ESCAPE))
                .map(|i| min + i as u8))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_messages() {
        let (src, uri, time, copy) = (0x0040_0001, 300, 123_456, 301);
        assert_eq!(enter(src, 5, uri), [src, 5 << 24, uri, 0, 0]);
        assert_eq!(enter(src, 7, uri)[1], 5 << 24, "the lower version");
        assert_eq!(enter(src, 3, uri)[1], 3 << 24);
        assert_eq!(
            position(src, (1920, 35), time, copy),
            [src, 0, (1920 << 16) | 35, time, copy]
        );
        // Negative root coordinates (a monitor left of the primary one) keep their bits.
        assert_eq!(position(src, (-2, -1), time, copy)[2], 0xFFFE_FFFF);
        assert_eq!(drop_message(src, time), [src, 0, time, 0, 0]);
        assert_eq!(leave(src), [src, 0, 0, 0, 0]);
    }

    #[test]
    fn target_answers() {
        let (target, copy, move_) = (77, 301, 302);
        assert_eq!(status([target, 3, 0, 0, copy]), (true, copy));
        assert_eq!(status([target, 2, 0, 0, 0]), (false, 0));
        assert_eq!(finished([target, 1, move_, 0, 0], 5, copy), (true, move_));
        assert_eq!(finished([target, 0, 0, 0, 0], 5, copy), (false, 0));
        assert_eq!(finished([target, 0, 0, 0, 0], 4, copy), (true, copy));
        assert_eq!(effect(true, move_, move_), DropEffect::Move);
        assert_eq!(effect(true, copy, move_), DropEffect::Copy);
        assert_eq!(effect(false, move_, move_), DropEffect::None);
    }

    #[test]
    fn keymap_bits() {
        let mut keys = [0u8; 32];
        keys[1] = 0b0000_0100; // keycode 10
        keys[31] = 0x80; // keycode 255
        assert!(key_down(&keys, 10) && key_down(&keys, 255));
        assert!(!key_down(&keys, 9) && !key_down(&keys, 11));
    }
}
