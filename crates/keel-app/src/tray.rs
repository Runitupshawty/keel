//! Windows notification icon using the already-approved windows crate (MIT/Apache-2.0).
//! No extra tray dependencies; other platforms retain normal close behavior.
#[derive(Clone, Copy)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum Event {
    Show,
    NewWindow,
    Exit,
}

#[cfg(windows)]
pub use native::Tray;

#[cfg(not(windows))]
pub struct Tray;
#[cfg(not(windows))]
impl Tray {
    pub fn new(_: &egui::Context) -> Result<Self, String> {
        Err("Tray is Windows only".into())
    }
    pub fn events(&self) -> Vec<Event> {
        Vec::new()
    }
}

#[cfg(windows)]
mod native {
    use super::Event;
    use windows::{
        core::w,
        Win32::{
            Foundation::*,
            UI::{Shell::*, WindowsAndMessaging::*},
        },
    };

    const CALLBACK: u32 = WM_APP + 76;
    struct State {
        ctx: egui::Context,
        tx: crossbeam_channel::Sender<Event>,
        icon: NOTIFYICONDATAW,
        taskbar_created: u32,
    }

    pub struct Tray {
        hwnd: HWND,
        // Stable address while the hidden window exists; only its owning UI thread uses it.
        state: Box<State>,
        rx: crossbeam_channel::Receiver<Event>,
    }

    impl Tray {
        pub fn new(ctx: &egui::Context) -> Result<Self, String> {
            // SAFETY: all window and shell calls stay on the UI thread. State outlives HWND.
            unsafe {
                let class = WNDCLASSW {
                    lpfnWndProc: Some(wndproc),
                    lpszClassName: w!("KeelTrayWindow"),
                    ..Default::default()
                };
                RegisterClassW(&class); // Already registered after toggling the setting is fine.
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("KeelTrayWindow"),
                    w!("Keel tray"),
                    WINDOW_STYLE::default(),
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    None,
                    None,
                )
                .map_err(|e| e.to_string())?;
                let (tx, rx) = crossbeam_channel::unbounded();
                let mut icon = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: hwnd,
                    uID: 1,
                    uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
                    uCallbackMessage: CALLBACK,
                    hIcon: app_icon(),
                    ..Default::default()
                };
                for (dst, src) in icon.szTip.iter_mut().zip("Keel".encode_utf16()) {
                    *dst = src;
                }
                let mut state = Box::new(State {
                    ctx: ctx.clone(),
                    tx,
                    icon,
                    taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
                });
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut *state as *mut State) as isize);
                if !Shell_NotifyIconW(NIM_ADD, &state.icon).as_bool() {
                    let _ = DestroyWindow(hwnd);
                    return Err("Could not create the tray icon; closing will exit Keel".into());
                }
                Ok(Self { hwnd, state, rx })
            }
        }

        pub fn events(&self) -> Vec<Event> {
            self.rx.try_iter().collect()
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            // SAFETY: window is destroyed before its callback data, on the creating thread.
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.state.icon);
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }

    /// Keel's own icon (resource 1 in `keel.rc`) at the small-icon size; Windows' default
    /// application icon when it cannot be loaded.
    // MAKEINTRESOURCE(1): the pointer value is a resource ID, never dereferenced as memory.
    #[allow(clippy::manual_dangling_ptr)]
    unsafe fn app_icon() -> HICON {
        use windows::{core::PCWSTR, Win32::System::LibraryLoader::GetModuleHandleW};
        let module = GetModuleHandleW(None).unwrap_or_default();
        let (cx, cy) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        // LR_SHARED: the system owns the icon, nothing to destroy.
        match LoadImageW(
            HINSTANCE(module.0),
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            cx,
            cy,
            LR_DEFAULTCOLOR | LR_SHARED,
        ) {
            Ok(handle) => HICON(handle.0),
            Err(_) => LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        if !ptr.is_null() {
            let s = &*ptr;
            if msg == s.taskbar_created && msg != 0 {
                let _ = Shell_NotifyIconW(NIM_ADD, &s.icon);
            }
            if msg == CALLBACK {
                let event = match lp.0 as u32 {
                    WM_LBUTTONUP => Some(Event::Show),
                    WM_RBUTTONUP | WM_CONTEXTMENU => {
                        let mut event = None;
                        if let Ok(menu) = CreatePopupMenu() {
                            let _ = AppendMenuW(menu, MF_STRING, 1, w!("Show Keel"));
                            let _ = AppendMenuW(menu, MF_STRING, 2, w!("New window"));
                            let _ = AppendMenuW(menu, MF_STRING, 3, w!("Exit"));
                            let mut pos = POINT::default();
                            let _ = GetCursorPos(&mut pos);
                            let _ = SetForegroundWindow(hwnd);
                            let picked = TrackPopupMenu(
                                menu,
                                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                                pos.x,
                                pos.y,
                                0,
                                hwnd,
                                None,
                            );
                            event = match picked.0 {
                                1 => Some(Event::Show),
                                2 => Some(Event::NewWindow),
                                3 => Some(Event::Exit),
                                _ => None,
                            };
                            let _ = DestroyMenu(menu);
                            let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
                        }
                        event
                    }
                    _ => None,
                };
                if let Some(event) = event {
                    let _ = s.tx.send(event);
                    s.ctx.request_repaint();
                }
                return LRESULT(0);
            }
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}
