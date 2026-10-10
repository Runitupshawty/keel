//! The Linux file clipboard with the cut flag. arboard offers only `text/uri-list`, so on
//! X11 (and on Wayland through XWayland, as arboard does here) Keel owns the CLIPBOARD
//! selection itself and offers the file list in the targets file managers read:
//! `x-special/gnome-copied-files` (`cut` or `copy`, then `file://` URIs: GNOME Files, Nemo,
//! Caja, Thunar, PCManFM), `text/uri-list`, and `application/x-kde-cutselection` (Dolphin).
//! Reading takes the same targets from whichever app owns the clipboard.

use std::path::{Path, PathBuf};

pub const GNOME: &str = "x-special/gnome-copied-files";
pub const URI_LIST: &str = "text/uri-list";
pub const KDE_CUT: &str = "application/x-kde-cutselection";

/// What Keel offers for `paths`, as (target, bytes).
pub fn payload(paths: &[PathBuf], cut: bool) -> Vec<(&'static str, Vec<u8>)> {
    let uris: Vec<String> = paths.iter().map(|p| file_uri(p)).collect();
    let verb = if cut { "cut" } else { "copy" };
    vec![
        (GNOME, format!("{verb}\n{}", uris.join("\n")).into_bytes()),
        (URI_LIST, format!("{}\r\n", uris.join("\r\n")).into_bytes()),
        (KDE_CUT, if cut { b"1" } else { b"0" }.to_vec()),
    ]
}

/// `file://` URI of an absolute path, every byte but `A-Z a-z 0-9 - . _ ~ /` escaped.
pub fn file_uri(p: &Path) -> String {
    let mut out = String::from("file://");
    for b in path_bytes(p) {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The local path of a `file://` URI (`file:///p` or `file://localhost/p`).
pub fn uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.trim().strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let (mut bytes, mut it) = (Vec::new(), rest.bytes());
    while let Some(b) = it.next() {
        if b == b'%' {
            let hex = [it.next()?, it.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    Some(bytes_path(bytes))
}

/// The local paths in URI lines; comments, other schemes and other hosts are skipped.
pub fn uri_paths<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<PathBuf> {
    lines
        .filter(|l| !l.starts_with('#'))
        .filter_map(uri_path)
        .collect()
}

/// `x-special/gnome-copied-files`: the verb line, then one URI per line.
pub fn parse_gnome(bytes: &[u8]) -> Option<(Vec<PathBuf>, bool)> {
    let mut lines = std::str::from_utf8(bytes).ok()?.lines();
    let cut = match lines.next()?.trim() {
        "cut" => true,
        "copy" => false,
        _ => return None,
    };
    let paths = uri_paths(lines);
    (!paths.is_empty()).then_some((paths, cut))
}

#[cfg(unix)]
fn path_bytes(p: &Path) -> Vec<u8> {
    std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str()).to_vec()
}
#[cfg(unix)]
fn bytes_path(b: Vec<u8>) -> PathBuf {
    <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(b).into()
}
// Tests on other systems: Unix-style paths as text.
#[cfg(not(unix))]
fn path_bytes(p: &Path) -> Vec<u8> {
    p.to_string_lossy().replace('\\', "/").into_bytes()
}
#[cfg(not(unix))]
fn bytes_path(b: Vec<u8>) -> PathBuf {
    String::from_utf8_lossy(&b).into_owned().into()
}

#[cfg(all(target_os = "linux", test))]
use x11::targets;
#[cfg(target_os = "linux")]
pub use x11::{available, read, write};

#[cfg(target_os = "linux")]
mod x11 {
    use super::*;
    use anyhow::{ensure, Context, Result};
    use parking_lot::Mutex;
    use std::{
        sync::{Arc, OnceLock},
        time::{Duration, Instant},
    };
    use x11rb::{
        connection::{Connection, RequestConnection},
        protocol::{xproto::*, Event},
        rust_connection::RustConnection,
        wrapper::ConnectionExt as _,
        COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE,
    };

    /// How long a read waits for the clipboard's owner to answer, per target.
    const ANSWER_WAIT: Duration = Duration::from_secs(1);

    /// An X display to talk to (also set in a Wayland session with XWayland).
    pub fn available() -> bool {
        std::env::var_os("DISPLAY").is_some()
    }

    struct Atoms {
        clipboard: Atom,
        targets: Atom,
        incr: Atom,
        property: Atom,
        gnome: Atom,
        uri_list: Atom,
        kde_cut: Atom,
    }
    impl Atoms {
        fn new(conn: &RustConnection) -> Result<Self> {
            let names = [
                "CLIPBOARD",
                "TARGETS",
                "INCR",
                "KEEL_CLIPBOARD",
                GNOME,
                URI_LIST,
                KDE_CUT,
            ];
            let cookies = names
                .iter()
                .map(|n| conn.intern_atom(false, n.as_bytes()))
                .collect::<Result<Vec<_>, _>>()?;
            let mut atoms = Vec::with_capacity(names.len());
            for c in cookies {
                atoms.push(c.reply()?.atom);
            }
            Ok(Self {
                clipboard: atoms[0],
                targets: atoms[1],
                incr: atoms[2],
                property: atoms[3],
                gnome: atoms[4],
                uri_list: atoms[5],
                kde_cut: atoms[6],
            })
        }
        fn of(&self, target: &str) -> Atom {
            match target {
                GNOME => self.gnome,
                URI_LIST => self.uri_list,
                _ => self.kde_cut,
            }
        }
    }

    /// A connection with a hidden window to own or request the selection with.
    fn connect() -> Result<(RustConnection, Window, Atoms)> {
        let (conn, screen) = RustConnection::connect(None).context("connect to the X display")?;
        let root = conn.setup().roots[screen].root;
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
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new(),
        )?;
        let atoms = Atoms::new(&conn)?;
        Ok((conn, win, atoms))
    }

    /// Keel's side of the selection: serves what it offers from its own thread for as long
    /// as the process runs (another app's copy only empties the offer).
    struct Owner {
        conn: RustConnection,
        win: Window,
        atoms: Atoms,
        offer: Mutex<Vec<(Atom, Vec<u8>)>>,
    }

    fn owner() -> Result<Arc<Owner>> {
        static OWNER: OnceLock<Result<Arc<Owner>, String>> = OnceLock::new();
        OWNER
            .get_or_init(|| {
                let (conn, win, atoms) = connect().map_err(|e| format!("{e:#}"))?;
                let owner = Arc::new(Owner {
                    conn,
                    win,
                    atoms,
                    offer: Mutex::default(),
                });
                let serving = owner.clone();
                std::thread::Builder::new()
                    .name("keel-x11-clipboard".into())
                    .spawn(move || serving.serve())
                    .map_err(|e| e.to_string())?;
                Ok(owner)
            })
            .clone()
            .map_err(anyhow::Error::msg)
    }

    impl Owner {
        fn serve(&self) {
            while let Ok(event) = self.conn.wait_for_event() {
                match event {
                    Event::SelectionRequest(e) => {
                        let property = match self.answer(&e) {
                            Ok(true) if e.property == NONE => e.target,
                            Ok(true) => e.property,
                            _ => NONE,
                        };
                        let notify = SelectionNotifyEvent {
                            response_type: SELECTION_NOTIFY_EVENT,
                            sequence: 0,
                            time: e.time,
                            requestor: e.requestor,
                            selection: e.selection,
                            target: e.target,
                            property,
                        };
                        let _ =
                            self.conn
                                .send_event(false, e.requestor, EventMask::NO_EVENT, notify);
                        let _ = self.conn.flush();
                    }
                    Event::SelectionClear(e) if e.selection == self.atoms.clipboard => {
                        self.offer.lock().clear();
                    }
                    _ => {}
                }
            }
        }

        /// Puts the requested target on the requestor's property; false when there is none.
        fn answer(&self, e: &SelectionRequestEvent) -> Result<bool> {
            let offer = self.offer.lock();
            if offer.is_empty() || e.selection != self.atoms.clipboard {
                return Ok(false);
            }
            let property = if e.property == NONE {
                e.target
            } else {
                e.property
            };
            if e.target == self.atoms.targets {
                let mut list = vec![self.atoms.targets];
                list.extend(offer.iter().map(|(a, _)| *a));
                self.conn.change_property32(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    AtomEnum::ATOM,
                    &list,
                )?;
                return Ok(true);
            }
            let Some((_, bytes)) = offer.iter().find(|(a, _)| *a == e.target) else {
                return Ok(false);
            };
            // ponytail: no INCR transfers, so a list longer than one X request (usually
            // 16 MiB, at least 256 KiB) is refused; add INCR if that ever matters.
            if bytes.len() + 64 > self.conn.maximum_request_bytes() {
                return Ok(false);
            }
            self.conn.change_property8(
                PropMode::REPLACE,
                e.requestor,
                property,
                e.target,
                bytes,
            )?;
            Ok(true)
        }
    }

    /// Owns the clipboard with `paths` (cut or copy); empty `paths` clears it.
    pub fn write(paths: &[PathBuf], cut: bool) -> Result<()> {
        let o = owner()?;
        let offer = if paths.is_empty() {
            Vec::new()
        } else {
            payload(paths, cut)
                .into_iter()
                .map(|(t, b)| (o.atoms.of(t), b))
                .collect()
        };
        let win = if offer.is_empty() { NONE } else { o.win };
        *o.offer.lock() = offer;
        o.conn
            .set_selection_owner(win, o.atoms.clipboard, CURRENT_TIME)?;
        let now = o
            .conn
            .get_selection_owner(o.atoms.clipboard)?
            .reply()?
            .owner;
        ensure!(now == win, "another app holds the clipboard");
        Ok(())
    }

    /// The files on the clipboard and whether they were cut, from any app.
    pub fn read() -> Result<Option<(Vec<PathBuf>, bool)>> {
        let (conn, win, atoms) = connect()?;
        let get = |target| convert(&conn, win, &atoms, target);
        let result = (|| {
            if let Some(found) = get(atoms.gnome)?.as_deref().and_then(parse_gnome) {
                return Ok(Some(found));
            }
            let Some(list) = get(atoms.uri_list)? else {
                return Ok(None);
            };
            let paths = uri_paths(String::from_utf8_lossy(&list).lines());
            if paths.is_empty() {
                return Ok(None);
            }
            let cut = get(atoms.kde_cut)?.is_some_and(|b| b.starts_with(b"1"));
            Ok(Some((paths, cut)))
        })();
        let _ = conn.destroy_window(win);
        let _ = conn.flush();
        result
    }

    /// The target names the clipboard's owner offers (what a file manager asks first).
    #[cfg(test)]
    pub fn targets() -> Result<Vec<String>> {
        let (conn, win, atoms) = connect()?;
        let bytes = convert(&conn, win, &atoms, atoms.targets)?.unwrap_or_default();
        let mut names = Vec::new();
        for atom in bytes.as_chunks::<4>().0 {
            let atom = u32::from_ne_bytes(*atom);
            names.push(String::from_utf8(conn.get_atom_name(atom)?.reply()?.name)?);
        }
        Ok(names)
    }

    /// One target of the clipboard; None when its owner has no such target (or there is
    /// no owner).
    fn convert(
        conn: &RustConnection,
        win: Window,
        atoms: &Atoms,
        target: Atom,
    ) -> Result<Option<Vec<u8>>> {
        conn.convert_selection(win, atoms.clipboard, target, atoms.property, CURRENT_TIME)?;
        conn.flush()?;
        let until = Instant::now() + ANSWER_WAIT;
        loop {
            match conn.poll_for_event()? {
                Some(Event::SelectionNotify(e)) if e.requestor == win => {
                    if e.property == NONE {
                        return Ok(None);
                    }
                    let reply = conn
                        .get_property(true, win, e.property, AtomEnum::ANY, 0, u32::MAX / 4)?
                        .reply()?;
                    // ponytail: a list sent in INCR pieces (bigger than one X request) reads
                    // as absent, so such a cut pastes as arboard's copy.
                    return Ok((reply.type_ != atoms.incr).then_some(reply.value));
                }
                Some(_) => {}
                None if Instant::now() >= until => {
                    anyhow::bail!("the app holding the clipboard did not answer")
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_has_the_cut_verb_and_escaped_uris() {
        let paths = [PathBuf::from("/home/u/a b%.txt"), PathBuf::from("/tmp/ü#1")];
        let p = payload(&paths, true);
        let get = |t| p.iter().find(|(n, _)| *n == t).unwrap().1.clone();
        assert_eq!(
            get(GNOME),
            b"cut\nfile:///home/u/a%20b%25.txt\nfile:///tmp/%C3%BC%231"
        );
        assert_eq!(
            get(URI_LIST),
            b"file:///home/u/a%20b%25.txt\r\nfile:///tmp/%C3%BC%231\r\n"
        );
        assert_eq!(get(KDE_CUT), b"1");
        let copy = payload(&paths, false);
        assert!(copy[0].1.starts_with(b"copy\n"));
        assert_eq!(copy[2].1, b"0");

        assert_eq!(parse_gnome(&get(GNOME)), Some((paths.to_vec(), true)));
        assert_eq!(parse_gnome(&copy[0].1), Some((paths.to_vec(), false)));
        let list = String::from_utf8(get(URI_LIST)).unwrap();
        assert_eq!(uri_paths(list.lines()), paths);
    }

    #[test]
    fn foreign_lists_parse_or_are_refused() {
        // GNOME Files: no trailing newline; others end with one or use CRLF.
        let nautilus = b"copy\nfile:///home/u/Doc%20one.odt\r\nfile://localhost/srv/x\n";
        assert_eq!(
            parse_gnome(nautilus),
            Some((
                vec![
                    PathBuf::from("/home/u/Doc one.odt"),
                    PathBuf::from("/srv/x")
                ],
                false
            ))
        );
        assert_eq!(parse_gnome(b"move\nfile:///a"), None, "unknown verb");
        assert_eq!(parse_gnome(b"cut\n"), None, "no files");
        assert_eq!(parse_gnome(b"cut\nhttps://example.org/a"), None);
        assert_eq!(uri_path("file://otherhost/a"), None);
        assert_eq!(uri_path("file:///a%2"), None, "cut-off escape");
        assert_eq!(uri_path("file:///a%zz"), None);
        assert_eq!(
            uri_paths(["# comment", "file:///a", "smb://h/s"].into_iter()),
            [PathBuf::from("/a")]
        );
    }

    /// The real X clipboard: owns it, reads it back through a second connection and puts
    /// the user's files back. Needs a display: `KEEL_LINUX_CLIPBOARD_TEST=1 xvfb-run -a -s
    /// -noreset cargo test -p keel-app --bin keel x11_clipboard` (without `-noreset` Xvfb
    /// resets whenever its last client leaves, and a connection made then is dropped).
    #[cfg(target_os = "linux")]
    #[test]
    fn x11_round_trip_keeps_the_cut_flag() {
        if std::env::var("KEEL_LINUX_CLIPBOARD_TEST").as_deref() != Ok("1") || !available() {
            eprintln!("skipped: set KEEL_LINUX_CLIPBOARD_TEST=1 with a DISPLAY");
            return;
        }
        let _only = crate::clipboard::SYSTEM_CLIPBOARD
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let paths = vec![
            PathBuf::from("/tmp/keel clip/one.txt"),
            PathBuf::from("/tmp/keel clip/two#.txt"),
        ];
        let saved = read().unwrap();
        write(&paths, true).unwrap();
        let offered = targets().unwrap();
        for t in ["TARGETS", GNOME, URI_LIST, KDE_CUT] {
            assert!(offered.iter().any(|o| o == t), "{t} in {offered:?}");
        }
        assert_eq!(read().unwrap(), Some((paths.clone(), true)));
        write(&paths, false).unwrap();
        assert_eq!(read().unwrap(), Some((paths.clone(), false)));
        write(&[], false).unwrap();
        assert_eq!(read().unwrap(), None);
        if let Some((p, cut)) = saved {
            write(&p, cut).unwrap();
        }
    }
}
