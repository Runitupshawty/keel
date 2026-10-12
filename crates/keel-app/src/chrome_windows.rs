//! Windows: the Finder style's title bar (Task 65), a subclass of the eframe window.
//!
//! For an undecorated window winit keeps the caption and sizing styles (so snapping, the
//! system menu and the maximize animation still work) and answers `WM_NCCALCSIZE` with
//! the whole window as client area. What is left is telling the system which part of that
//! client area is what: `WM_NCHITTEST` is answered from the toolbar's [`Layout`] (points;
//! converted from pixels here). The empty toolbar is the caption (drag, double-click to
//! maximize, right-click for the system menu), the edges resize, and Keel's maximize button
//! is `HTMAXBUTTON`, which makes Windows 11 offer its snap layouts on hover.
//!
//! The maximize button's mouse messages are handled here, not by the system: pressed, the
//! system would draw its own old-style button over Keel's. Hover is tracked here too,
//! because the pointer over a nonclient area never reaches egui.
//!
//! The subclass runs on the UI thread, which owns the window. Its data is boxed in
//! [`Native`] and outlives the subclass: it is removed when `Native` drops or when the
//! window is destroyed (`WM_NCDESTROY`), whichever comes first.

use super::{Hit, Layout};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    Graphics::{
        Dwm::{
            DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMSBT_AUTO, DWMSBT_MAINWINDOW,
            DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
            DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DEFAULT, DWMWCP_ROUND, DWMWINDOWATTRIBUTE,
        },
        Gdi::ScreenToClient,
    },
    UI::{
        Controls::MARGINS,
        Input::KeyboardAndMouse::{
            TrackMouseEvent, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TRACKMOUSEEVENT_FLAGS,
        },
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            IsZoomed, PostMessageW, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTCLIENT,
            HTLEFT, HTMAXBUTTON, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, SC_MAXIMIZE, SC_RESTORE,
            WM_MOUSEMOVE, WM_NCDESTROY, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN,
            WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_SYSCOMMAND,
        },
    },
};

/// This subclass's id on the window ("KEEL").
const SUBCLASS: usize = 0x4b45_454c;

struct Data {
    layout: Layout,
    /// Pixels per point when `layout` was drawn.
    scale: f32,
    /// The Finder style is shown: Keel draws the title bar.
    enabled: bool,
    /// False once the window is gone (the subclass removed itself).
    alive: bool,
    hover: bool,
    pressed: bool,
    /// `TrackMouseEvent` asked for `WM_NCMOUSELEAVE`.
    tracking: bool,
    ctx: egui::Context,
}

pub struct Native {
    hwnd: HWND,
    data: Box<Data>,
    /// (enabled, dark) last handed to the DWM.
    applied: Option<(bool, bool)>,
}

impl Native {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Option<Self> {
        let handle = cc.window_handle().ok()?;
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return None;
        };
        let hwnd = HWND(handle.hwnd.get() as *mut _);
        let mut data = Box::new(Data {
            layout: Layout::default(),
            scale: 1.0,
            enabled: false,
            alive: true,
            hover: false,
            pressed: false,
            tracking: false,
            ctx: cc.egui_ctx.clone(),
        });
        let ptr: *mut Data = &mut *data;
        // SAFETY: on the thread that owns the window (eframe creates the app there). The
        // box does not move when `Native` does, and `Drop` removes the subclass before it
        // is freed.
        let ok = unsafe { SetWindowSubclass(hwnd, Some(subclass), SUBCLASS, ptr as usize) };
        if !ok.as_bool() {
            tracing::warn!("window chrome: could not subclass the window");
            return None;
        }
        Some(Self {
            hwnd,
            data,
            applied: None,
        })
    }

    pub fn publish(&mut self, layout: Layout, scale: f32) {
        self.data.layout = layout;
        self.data.scale = scale.max(0.1);
    }

    pub fn hovered(&self) -> bool {
        self.data.enabled && self.data.hover
    }

    /// Hit testing on or off, and the frame the DWM draws: rounded corners and a shadow
    /// (a one-pixel frame extended into the client area) and the Mica backdrop on Windows
    /// 11 (22H2 and later; older versions refuse the attribute and nothing changes).
    pub fn sync(&mut self, enabled: bool, dark: bool) {
        if !self.data.alive {
            return;
        }
        self.data.enabled = enabled;
        if !enabled {
            self.data.hover = false;
            self.data.pressed = false;
        }
        if self.applied == Some((enabled, dark)) {
            return;
        }
        self.applied = Some((enabled, dark));
        let set = |attribute: DWMWINDOWATTRIBUTE, value: i32| {
            // SAFETY: a live window; the DWM copies the value before returning.
            unsafe {
                DwmSetWindowAttribute(
                    self.hwnd,
                    attribute,
                    (&value as *const i32).cast(),
                    std::mem::size_of::<i32>() as u32,
                )
            }
            .is_ok()
        };
        let margin = i32::from(enabled);
        let margins = MARGINS {
            cxLeftWidth: margin,
            cxRightWidth: margin,
            cyTopHeight: margin,
            cyBottomHeight: margin,
        };
        // SAFETY: a live window; `margins` outlives the call.
        let _ = unsafe { DwmExtendFrameIntoClientArea(self.hwnd, &margins) };
        if enabled {
            set(DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND.0);
            set(DWMWA_USE_IMMERSIVE_DARK_MODE, i32::from(dark));
            if !set(DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_MAINWINDOW.0) {
                tracing::debug!("window chrome: no system backdrop (before Windows 11 22H2)");
            }
        } else {
            set(DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DEFAULT.0);
            set(DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_AUTO.0);
        }
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        if self.data.alive {
            // SAFETY: on the UI thread, before the data the subclass points to is freed.
            unsafe {
                let _ = RemoveWindowSubclass(self.hwnd, Some(subclass), SUBCLASS);
            }
        }
    }
}

/// The hit test code for a point in screen pixels (`WM_NCHITTEST`'s lParam).
fn hit_code(hwnd: HWND, lp: LPARAM, data: &Data) -> Option<u32> {
    // GET_X_LPARAM / GET_Y_LPARAM: signed, for monitors left of or above the primary one.
    let mut point = POINT {
        x: i32::from(lp.0 as u16 as i16),
        y: i32::from((lp.0 >> 16) as u16 as i16),
    };
    // SAFETY: a live window and a valid POINT.
    if !unsafe { ScreenToClient(hwnd, &mut point) }.as_bool() {
        return None;
    }
    let p = egui::pos2(point.x as f32 / data.scale, point.y as f32 / data.scale);
    Some(match data.layout.hit(p) {
        Hit::Client => HTCLIENT,
        Hit::Caption => HTCAPTION,
        Hit::Maximize => HTMAXBUTTON,
        Hit::Left => HTLEFT,
        Hit::Right => HTRIGHT,
        Hit::Top => HTTOP,
        Hit::Bottom => HTBOTTOM,
        Hit::TopLeft => HTTOPLEFT,
        Hit::TopRight => HTTOPRIGHT,
        Hit::BottomLeft => HTBOTTOMLEFT,
        Hit::BottomRight => HTBOTTOMRIGHT,
    })
}

fn set_hover(data: &mut Data, hover: bool) {
    if data.hover != hover {
        data.hover = hover;
        data.ctx.request_repaint();
    }
    if !hover {
        data.pressed = false;
    }
}

unsafe extern "system" fn subclass(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: `data` is the `Data` boxed in `Native`, alive while this subclass is installed
    // (see the module docs), and only this thread touches it. The reference is not held
    // across `DefSubclassProc`, which can send messages back here.
    let ptr = data as *mut Data;
    {
        let data = &mut *ptr;
        if msg == WM_NCDESTROY {
            data.alive = false;
            let _ = RemoveWindowSubclass(hwnd, Some(subclass), SUBCLASS);
        } else if data.enabled {
            let on_max = wp.0 as u32 == HTMAXBUTTON;
            match msg {
                WM_NCHITTEST => {
                    if let Some(code) = hit_code(hwnd, lp, data) {
                        return LRESULT(code as isize);
                    }
                }
                WM_NCMOUSEMOVE => {
                    set_hover(data, on_max);
                    if on_max && !data.tracking {
                        let mut track = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TRACKMOUSEEVENT_FLAGS(TME_LEAVE.0 | TME_NONCLIENT.0),
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        data.tracking = TrackMouseEvent(&mut track).is_ok();
                    }
                    // On to the system: the snap layouts flyout follows these.
                }
                WM_NCMOUSELEAVE => {
                    data.tracking = false;
                    set_hover(data, false);
                }
                WM_MOUSEMOVE => set_hover(data, false),
                WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if on_max => {
                    data.pressed = true;
                    data.ctx.request_repaint();
                    return LRESULT(0);
                }
                WM_NCLBUTTONUP if on_max => {
                    if std::mem::take(&mut data.pressed) {
                        let cmd = if IsZoomed(hwnd).as_bool() {
                            SC_RESTORE
                        } else {
                            SC_MAXIMIZE
                        };
                        let _ = PostMessageW(hwnd, WM_SYSCOMMAND, WPARAM(cmd as usize), LPARAM(0));
                    }
                    return LRESULT(0);
                }
                _ => {}
            }
        }
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}
