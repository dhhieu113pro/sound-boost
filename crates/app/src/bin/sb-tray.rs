//! The Sound Booster tray app.
//!
//! A notification-area icon with a flyout panel (on/off, boost slider, preset,
//! leveling strength) and a right-click menu. It publishes what the user picks
//! into the control block the APO reads, exactly as `sb-gain` does from a
//! terminal, so the two can be used interchangeably -- and the tray follows a
//! change `sb-gain` makes while it is running.
//!
//! The control block only exists once the audio engine has loaded the APO,
//! which takes a playing stream. So the app starts without it, retries once a
//! second, and publishes the saved settings the moment it attaches. Holding the
//! section open also keeps those settings across an `audiodg.exe` restart.
//!
//! Closing the app leaves the last published settings in force (the APO keeps
//! reading the block), the same as `sb-gain`; untick "Enabled" to stop
//! processing.
//!
//! Everything here runs on one thread. The state lives in a `RefCell` that is
//! only ever borrowed for a few statements and never across a call that can
//! pump messages (showing a window, a menu, a message box), because those
//! re-enter the window procedure.

#![windows_subsystem = "windows"]

use std::cell::{OnceCell, RefCell};
use std::io;
use std::mem::{size_of, zeroed};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::rc::Rc;

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, CreateFontIndirectW, DeleteObject, GetMonitorInfoW,
    GetStockObject, MonitorFromPoint, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, COLOR_BTNFACE,
    DEFAULT_GUI_FONT, DIB_RGB_COLORS, FW_SEMIBOLD, HBRUSH, HFONT, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Controls::{
    InitCommonControlsEx, BST_CHECKED, BST_UNCHECKED, ICC_BAR_CLASSES, ICC_STANDARD_CLASSES,
    INITCOMMONCONTROLSEX, TBM_SETLINESIZE, TBM_SETPAGESIZE, TBM_SETPOS, TBM_SETRANGEMAX,
    TBM_SETRANGEMIN, TBM_SETTICFREQ, TBS_AUTOTICKS, TRACKBAR_CLASSW,
};
use windows_sys::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForWindow, GetSystemMetricsForDpi,
    SetProcessDpiAwarenessContext, SystemParametersInfoForDpi,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NINF_KEY, NIN_SELECT, NOTIFYICONDATAW,
    NOTIFYICONIDENTIFIER, NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, AppendMenuW, CheckMenuRadioItem, CreateIconIndirect, CreatePopupMenu,
    CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW,
    FindWindowW, GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowRect, IsDialogMessageW,
    IsWindowVisible, LoadCursorW, MessageBoxW, MoveWindow, PostMessageW, PostQuitMessage,
    RegisterClassExW, RegisterWindowMessageW, SendMessageW, SetForegroundWindow,
    SetMenuDefaultItem, SetTimer, SetWindowPos, SetWindowTextW, ShowWindow, TrackPopupMenuEx,
    TranslateMessage, ASFW_ANY, BM_GETCHECK, BM_SETCHECK, BN_CLICKED, BS_AUTOCHECKBOX,
    CBN_SELCHANGE, CBS_DROPDOWNLIST, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, HICON, HMENU,
    HWND_TOPMOST, ICONINFO, IDCANCEL, IDC_ARROW, MB_ICONERROR, MB_ICONWARNING, MB_OK, MF_BYCOMMAND,
    MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, NONCLIENTMETRICSW, SM_CXSMICON,
    SM_MENUDROPALIGNMENT, SPI_GETNONCLIENTMETRICS, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    SWP_SHOWWINDOW, SW_HIDE, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTALIGN, TPM_RIGHTBUTTON, WA_INACTIVE, WM_ACTIVATE, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_CONTEXTMENU, WM_DESTROY, WM_DPICHANGED, WM_ENDSESSION, WM_HSCROLL, WM_NULL,
    WM_QUERYENDSESSION, WM_SETFONT, WM_TIMER, WNDCLASSEXW, WS_BORDER, WS_CHILD, WS_CLIPCHILDREN,
    WS_EX_CONTROLPARENT, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_TABSTOP, WS_VISIBLE,
    WS_VSCROLL,
};

use sb_app::icon;
use sb_app::placement::{panel_origin, Rect};
use sb_app::settings::{Settings, FULL_STRENGTH, MAX, MIN, PRESETS};
use sb_app::store::{self, wide, BACKGROUND_ARG};
use sb_ipc::mapping::MappedControl;

const CLASS_NAME: &str = "SoundBoosterTray";
const MUTEX_NAME: &str = r"Local\SoundBoosterTray_v1";
const TITLE: &str = "Sound Booster";

/// The shell's callback for our icon.
const WM_APP_TRAY: u32 = WM_APP + 1;
/// Sent by a second instance: "the user launched me again, show yourself".
const WM_APP_SHOW: u32 = WM_APP + 2;

const TIMER_TICK: usize = 1;
const TICK_MS: u32 = 1000;
const ICON_ID: u32 = 1;

/// A click on the icon while the panel is open first deactivates (and hides)
/// the panel, then arrives as a click. Within this window it means "close",
/// not "open again".
const REOPEN_GUARD_MS: u64 = 300;

// Control IDs start well clear of IDOK/IDCANCEL, which IsDialogMessage sends.
const IDC_ENABLED: u16 = 1001;
const IDC_LEVEL: u16 = 1002;
const IDC_PRESET: u16 = 1003;
const IDC_STRENGTH: u16 = 1004;

const IDM_OPEN: u32 = 100;
const IDM_ENABLED: u32 = 101;
const IDM_PRESET: u32 = 110; // + index into PRESETS
const IDM_LEVEL: u32 = 120; // + index into MENU_LEVELS
const IDM_AUTOSTART: u32 = 130;
const IDM_EXIT: u32 = 131;

// Not exported by windows-sys 0.59.
/// `WM_USER`: the trackbar's "get position" message.
const TBM_GETPOS: u32 = 0x0400;
/// Enter or Space on the focused tray icon.
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;
const SS_LEFT: u32 = 0x0000;
const SS_RIGHT: u32 = 0x0002;
const SWP_NOMOVE: u32 = 0x0002;

const MENU_LEVELS: [u32; 6] = [100, 150, 200, 300, 400, 500];

/// Panel layout in device-independent pixels, scaled by DPI at run time.
const PANEL_WIDTH: i32 = 300;
const PANEL_HEIGHT: i32 = 300;
const PANEL_MARGIN: i32 = 12;

struct Controls {
    title: HWND,
    enabled: HWND,
    level_label: HWND,
    level_value: HWND,
    level: HWND,
    preset_label: HWND,
    preset: HWND,
    strength_label: HWND,
    strength_value: HWND,
    strength: HWND,
    status: HWND,
}

impl Controls {
    /// Every control with its position in DIPs: x, y, width, height.
    fn layout(&self) -> [(HWND, [i32; 4]); 11] {
        [
            (self.title, [16, 12, 268, 24]),
            (self.enabled, [16, 42, 268, 22]),
            (self.level_label, [16, 74, 120, 18]),
            (self.level_value, [136, 74, 148, 18]),
            (self.level, [10, 94, 280, 30]),
            (self.preset_label, [16, 132, 268, 18]),
            // A drop-down list's height includes the open list.
            (self.preset, [16, 152, 268, 160]),
            (self.strength_label, [16, 190, 160, 18]),
            (self.strength_value, [176, 190, 108, 18]),
            (self.strength, [10, 210, 280, 30]),
            (self.status, [16, 248, 268, 44]),
        ]
    }
}

/// What changes while the app runs.
struct State {
    settings: Settings,
    /// Settings changed since they were last saved.
    dirty: bool,
    control: Option<MappedControl>,
    /// Why the last attach failed, if it was anything but "not there yet".
    attach_error: Option<String>,
    icon: HICON,
    /// What the current icon shows: enabled, wave count, pixel size.
    icon_key: (bool, usize, i32),
    icon_added: bool,
    font: HFONT,
    title_font: HFONT,
    hidden_at: u64,
    last_anchor: Option<POINT>,
}

struct App {
    hwnd: HWND,
    controls: Controls,
    taskbar_created: u32,
    exe: PathBuf,
    state: RefCell<State>,
}

thread_local! {
    static APP: OnceCell<Rc<App>> = const { OnceCell::new() };
}

fn app() -> Option<Rc<App>> {
    APP.with(|cell| cell.get().cloned())
}

fn main() {
    let background = std::env::args().skip(1).any(|arg| arg == BACKGROUND_ARG);
    if let Err(error) = run(background) {
        message_box(
            null_mut(),
            &format!("Sound Booster could not start:\n\n{error}"),
            MB_ICONERROR,
        );
        std::process::exit(1);
    }
}

fn run(background: bool) -> io::Result<()> {
    unsafe {
        // One tray icon per user. A second launch shows the first one's panel.
        let mutex_name = wide(MUTEX_NAME);
        let _mutex = CreateMutexW(null(), 0, mutex_name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let class = wide(CLASS_NAME);
            let existing = FindWindowW(class.as_ptr(), null());
            if !existing.is_null() && !background {
                AllowSetForegroundWindow(ASFW_ANY);
                PostMessageW(existing, WM_APP_SHOW, 0, 0);
            }
            return Ok(());
        }

        // Crisp text on every monitor. Fails harmlessly on old Windows or if
        // a manifest already set it.
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let common = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_BAR_CLASSES | ICC_STANDARD_CLASSES,
        };
        InitCommonControlsEx(&common);

        let instance = GetModuleHandleW(null());
        let class = wide(CLASS_NAME);
        let window_class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE + 1) as usize as HBRUSH,
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        if RegisterClassExW(&window_class) == 0 {
            return Err(io::Error::last_os_error());
        }

        let title = wide(TITLE);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_CONTROLPARENT,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_BORDER | WS_CLIPCHILDREN,
            0,
            0,
            PANEL_WIDTH,
            PANEL_HEIGHT,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return Err(io::Error::last_os_error());
        }

        let controls = create_controls(hwnd);
        let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());

        let app = Rc::new(App {
            hwnd,
            controls,
            taskbar_created,
            exe: std::env::current_exe().unwrap_or_default(),
            state: RefCell::new(State {
                settings: store::load(),
                dirty: false,
                control: None,
                attach_error: None,
                icon: null_mut(),
                icon_key: (false, 0, 0),
                icon_added: false,
                font: null_mut(),
                title_font: null_mut(),
                hidden_at: 0,
                last_anchor: None,
            }),
        });
        APP.with(|cell| {
            let _ = cell.set(app.clone());
        });

        app.apply_dpi(GetDpiForWindow(hwnd));
        app.tick();
        app.refresh();
        SetTimer(hwnd, TIMER_TICK, TICK_MS, None);
        if !background {
            app.show_panel(None);
        }

        let mut message: MSG = zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            // Tab between controls, arrow keys on the sliders, Esc to close.
            if IsDialogMessageW(hwnd, &message) != 0 {
                continue;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}

unsafe fn create_controls(parent: HWND) -> Controls {
    let instance = GetModuleHandleW(null());
    let make = |class: &str, text: &str, style: u32, id: u16| {
        let class = wide(class);
        let text = wide(text);
        CreateWindowExW(
            0,
            class.as_ptr(),
            text.as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            0,
            0,
            parent,
            id as usize as HMENU,
            instance,
            null(),
        )
    };
    let trackbar = String::from_utf16_lossy(std::slice::from_raw_parts(
        TRACKBAR_CLASSW,
        (0..).take_while(|&i| *TRACKBAR_CLASSW.add(i) != 0).count(),
    ));

    let controls = Controls {
        title: make("STATIC", TITLE, SS_LEFT, 0),
        enabled: make(
            "BUTTON",
            "&Enabled",
            (BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
            IDC_ENABLED,
        ),
        level_label: make("STATIC", "&Boost", SS_LEFT, 0),
        level_value: make("STATIC", "", SS_RIGHT, 0),
        level: make(&trackbar, "", WS_TABSTOP | TBS_AUTOTICKS, IDC_LEVEL),
        preset_label: make("STATIC", "&Preset", SS_LEFT, 0),
        preset: make(
            "COMBOBOX",
            "",
            (CBS_DROPDOWNLIST as u32) | WS_TABSTOP | WS_VSCROLL,
            IDC_PRESET,
        ),
        strength_label: make("STATIC", "&Leveling strength", SS_LEFT, 0),
        strength_value: make("STATIC", "", SS_RIGHT, 0),
        strength: make(&trackbar, "", WS_TABSTOP | TBS_AUTOTICKS, IDC_STRENGTH),
        status: make("STATIC", "", SS_LEFT, 0),
    };

    SendMessageW(controls.level, TBM_SETRANGEMIN, 0, MIN as isize);
    SendMessageW(controls.level, TBM_SETRANGEMAX, 1, MAX as isize);
    SendMessageW(controls.level, TBM_SETTICFREQ, 100, 0);
    SendMessageW(controls.level, TBM_SETLINESIZE, 0, 5);
    SendMessageW(controls.level, TBM_SETPAGESIZE, 0, 25);

    SendMessageW(controls.strength, TBM_SETRANGEMIN, 0, 0);
    SendMessageW(controls.strength, TBM_SETRANGEMAX, 1, 100);
    SendMessageW(controls.strength, TBM_SETTICFREQ, 25, 0);
    SendMessageW(controls.strength, TBM_SETLINESIZE, 0, 5);
    SendMessageW(controls.strength, TBM_SETPAGESIZE, 0, 25);

    for preset in PRESETS {
        let label = wide(preset.label());
        SendMessageW(controls.preset, CB_ADDSTRING, 0, label.as_ptr() as isize);
    }
    controls
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match app() {
        Some(app) => match app.handle(message, wparam, lparam) {
            Some(result) => result,
            None => DefWindowProcW(hwnd, message, wparam, lparam),
        },
        // Messages sent while the window is still being built.
        None => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

impl App {
    fn handle(&self, message: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match message {
            WM_APP_TRAY => {
                // NOTIFYICON_VERSION_4: the event is in the low word of
                // lParam and the anchor point in wParam.
                let anchor = POINT {
                    x: signed_low(wparam),
                    y: signed_high(wparam),
                };
                match (lparam as usize & 0xFFFF) as u32 {
                    NIN_SELECT | NIN_KEYSELECT => self.toggle_panel(anchor),
                    WM_CONTEXTMENU => self.show_menu(anchor),
                    _ => {}
                }
                Some(0)
            }
            WM_APP_SHOW => {
                self.show_panel(None);
                Some(0)
            }
            message if message == self.taskbar_created && message != 0 => {
                // Explorer restarted and forgot every icon.
                self.state.borrow_mut().icon_added = false;
                self.refresh_icon(true);
                Some(0)
            }
            WM_TIMER if wparam == TIMER_TICK => {
                self.tick();
                Some(0)
            }
            WM_HSCROLL => {
                let source = lparam as HWND;
                let settings = self.settings();
                if source == self.controls.level {
                    let percent = trackbar_position(self.controls.level) as u32;
                    self.apply(Settings {
                        percent,
                        ..settings
                    });
                } else if source == self.controls.strength {
                    let strength = trackbar_position(self.controls.strength) as u32;
                    self.apply(Settings {
                        strength_milli: strength * FULL_STRENGTH / 100,
                        ..settings
                    });
                }
                Some(0)
            }
            WM_COMMAND => {
                let id = (wparam & 0xFFFF) as u32;
                let code = ((wparam >> 16) & 0xFFFF) as u32;
                if lparam == 0 {
                    // Menu (or IsDialogMessage's Esc -> IDCANCEL).
                    self.command(id);
                    return Some(0);
                }
                let settings = self.settings();
                match (id as u16, code) {
                    (IDC_ENABLED, BN_CLICKED) => {
                        let checked = unsafe {
                            SendMessageW(self.controls.enabled, BM_GETCHECK, 0, 0)
                                == BST_CHECKED as isize
                        };
                        self.apply(Settings {
                            enabled: checked,
                            ..settings
                        });
                    }
                    (IDC_PRESET, CBN_SELCHANGE) => {
                        let index =
                            unsafe { SendMessageW(self.controls.preset, CB_GETCURSEL, 0, 0) };
                        if let Some(preset) = usize::try_from(index)
                            .ok()
                            .and_then(|index| PRESETS.get(index))
                        {
                            self.apply(Settings {
                                preset: *preset,
                                ..settings
                            });
                        }
                    }
                    _ => {}
                }
                Some(0)
            }
            WM_ACTIVATE => {
                if (wparam & 0xFFFF) as u32 == WA_INACTIVE && self.panel_visible() {
                    self.hide_panel();
                }
                None
            }
            WM_DPICHANGED => {
                self.apply_dpi(((wparam >> 16) & 0xFFFF) as u32);
                if self.panel_visible() {
                    let anchor = self.state.borrow().last_anchor;
                    self.place_panel(anchor);
                }
                Some(0)
            }
            WM_CLOSE => {
                self.hide_panel();
                Some(0)
            }
            WM_QUERYENDSESSION => Some(1),
            WM_ENDSESSION => {
                if wparam != 0 {
                    self.save();
                }
                Some(0)
            }
            WM_DESTROY => {
                self.save();
                self.remove_icon();
                unsafe { PostQuitMessage(0) };
                Some(0)
            }
            _ => None,
        }
    }

    fn settings(&self) -> Settings {
        self.state.borrow().settings
    }

    /// Take new settings from the UI: publish, mark for saving, redraw.
    fn apply(&self, new: Settings) {
        let new = new.sanitized();
        {
            let mut state = self.state.borrow_mut();
            if state.settings == new {
                return;
            }
            state.settings = new;
            state.dirty = true;
            if let Some(control) = &state.control {
                sb_app::publish(control, &new.snapshot());
            }
        }
        self.refresh();
    }

    /// Once a second: attach if we are not attached, follow changes someone
    /// else published, re-add the icon if the shell was not ready, save.
    fn tick(&self) {
        let mut changed = false;
        {
            let mut state = self.state.borrow_mut();
            let state = &mut *state;
            match &state.control {
                None => match MappedControl::open() {
                    Ok(control) => {
                        // Our saved settings win on attach: they are what the
                        // user last chose in this app.
                        sb_app::publish(&control, &state.settings.snapshot());
                        state.control = Some(control);
                        state.attach_error = None;
                        changed = true;
                    }
                    Err(error) => {
                        let error =
                            (error.kind() != io::ErrorKind::NotFound).then(|| error.to_string());
                        changed |= state.attach_error != error;
                        state.attach_error = error;
                    }
                },
                Some(control) => {
                    let block = control.block();
                    if !block.is_compatible() {
                        sb_app::publish(control, &state.settings.snapshot());
                    } else {
                        let seen = state.settings.adopt(&block.read());
                        if seen != state.settings {
                            state.settings = seen;
                            state.dirty = true;
                            changed = true;
                        }
                    }
                }
            }
        }
        self.save();
        let retry_icon = !self.state.borrow().icon_added;
        if changed || retry_icon {
            self.refresh();
        }
    }

    fn save(&self) {
        let mut state = self.state.borrow_mut();
        if state.dirty {
            // A failure here is not worth a dialog: the next change retries,
            // and the settings are still in force through the control block.
            if store::save(&state.settings).is_ok() {
                state.dirty = false;
            }
        }
    }

    /// Bring every control, the tooltip and the icon in line with the state.
    fn refresh(&self) {
        let (settings, attached, attach_error) = {
            let state = self.state.borrow();
            (
                state.settings,
                state.control.is_some(),
                state.attach_error.clone(),
            )
        };
        let controls = &self.controls;
        unsafe {
            let check = if settings.enabled {
                BST_CHECKED
            } else {
                BST_UNCHECKED
            };
            SendMessageW(controls.enabled, BM_SETCHECK, check as usize, 0);
            SendMessageW(controls.level, TBM_SETPOS, 1, settings.percent as isize);
            SendMessageW(
                controls.strength,
                TBM_SETPOS,
                1,
                ((settings.strength_milli * 100 + FULL_STRENGTH / 2) / FULL_STRENGTH) as isize,
            );
            SendMessageW(controls.preset, CB_SETCURSEL, settings.preset_index(), 0);
            for control in [controls.preset, controls.strength] {
                EnableWindow(control, i32::from(settings.enabled));
            }
        }
        set_text(controls.level_value, &settings.level_text());
        set_text(controls.strength_value, &settings.strength_text());

        let status = match (attached, attach_error) {
            (true, _) if settings.enabled => "Connected to the audio engine.".to_string(),
            (true, _) => {
                "Connected. Processing is off: audio passes through untouched.".to_string()
            }
            (false, None) => "Waiting for the audio engine. Play any sound; it loads \
                              the effect with the first stream."
                .to_string(),
            (false, Some(error)) => format!("Cannot open the control block: {error}"),
        };
        set_text(controls.status, &status);
        self.refresh_icon(false);
    }

    /// Update the tray icon and tooltip, adding the icon if it is not there.
    fn refresh_icon(&self, force_add: bool) {
        let (settings, attached) = {
            let state = self.state.borrow();
            (state.settings, state.control.is_some())
        };
        let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForWindow(self.hwnd)) };
        let size = if size > 0 { size } else { 16 };
        let key = (settings.enabled, icon::wave_count(settings.percent), size);

        let old_icon = {
            let mut state = self.state.borrow_mut();
            if state.icon.is_null() || state.icon_key != key {
                let new_icon = make_icon(size, settings.enabled, settings.percent);
                if new_icon.is_null() {
                    None
                } else {
                    state.icon_key = key;
                    Some(std::mem::replace(&mut state.icon, new_icon))
                }
            } else {
                None
            }
        };

        let tip = if attached {
            format!("{TITLE}\n{}", settings.describe())
        } else {
            format!("{TITLE}\nwaiting for the audio engine")
        };

        let added = self.state.borrow().icon_added;
        let mut data = self.notify_data(NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP);
        copy_into(&mut data.szTip, &tip);
        unsafe {
            if added && !force_add {
                Shell_NotifyIconW(NIM_MODIFY, &data);
            } else if Shell_NotifyIconW(NIM_ADD, &data) != 0 {
                data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                Shell_NotifyIconW(NIM_SETVERSION, &data);
                self.state.borrow_mut().icon_added = true;
            }
            if let Some(old) = old_icon {
                if !old.is_null() {
                    DestroyIcon(old);
                }
            }
        }
    }

    fn remove_icon(&self) {
        let data = self.notify_data(0);
        unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
        let mut state = self.state.borrow_mut();
        state.icon_added = false;
        if !state.icon.is_null() {
            unsafe { DestroyIcon(state.icon) };
            state.icon = null_mut();
        }
    }

    fn notify_data(&self, flags: u32) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = unsafe { zeroed() };
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd;
        data.uID = ICON_ID;
        data.uFlags = flags;
        data.uCallbackMessage = WM_APP_TRAY;
        data.hIcon = self.state.borrow().icon;
        data
    }

    fn panel_visible(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd) != 0 }
    }

    fn toggle_panel(&self, anchor: POINT) {
        if self.panel_visible() {
            self.hide_panel();
            return;
        }
        let hidden_at = self.state.borrow().hidden_at;
        if unsafe { GetTickCount64() }.saturating_sub(hidden_at) < REOPEN_GUARD_MS {
            return;
        }
        self.show_panel(Some(anchor));
    }

    fn show_panel(&self, anchor: Option<POINT>) {
        let anchor = anchor.or_else(|| self.icon_anchor()).unwrap_or_else(|| {
            let mut cursor = POINT { x: 0, y: 0 };
            unsafe { GetCursorPos(&mut cursor) };
            cursor
        });
        self.state.borrow_mut().last_anchor = Some(anchor);
        self.refresh();
        self.place_panel(Some(anchor));
        unsafe {
            SetForegroundWindow(self.hwnd);
            SetFocus(self.controls.level);
        }
    }

    fn place_panel(&self, anchor: Option<POINT>) {
        let Some(anchor) = anchor else { return };
        unsafe {
            let mut window = RECT { ..zeroed() };
            GetWindowRect(self.hwnd, &mut window);
            let width = window.right - window.left;
            let height = window.bottom - window.top;

            let monitor = MonitorFromPoint(anchor, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..zeroed()
            };
            GetMonitorInfoW(monitor, &mut info);
            let work = Rect {
                left: info.rcWork.left,
                top: info.rcWork.top,
                right: info.rcWork.right,
                bottom: info.rcWork.bottom,
            };
            let margin = scale(PANEL_MARGIN, GetDpiForWindow(self.hwnd));
            let (x, y) = panel_origin((anchor.x, anchor.y), work, width, height, margin);
            SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_SHOWWINDOW,
            );
        }
    }

    fn hide_panel(&self) {
        unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        self.state.borrow_mut().hidden_at = unsafe { GetTickCount64() };
    }

    /// The middle of our icon, when the panel is opened without a click
    /// (startup, second launch, the menu's "Open").
    fn icon_anchor(&self) -> Option<POINT> {
        let identifier = NOTIFYICONIDENTIFIER {
            cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            ..unsafe { zeroed() }
        };
        let mut rect: RECT = unsafe { zeroed() };
        let result = unsafe { Shell_NotifyIconGetRect(&identifier, &mut rect) };
        (result == 0 && rect.right > rect.left).then(|| POINT {
            x: (rect.left + rect.right) / 2,
            y: (rect.top + rect.bottom) / 2,
        })
    }

    fn show_menu(&self, anchor: POINT) {
        let settings = self.settings();
        let autostart = store::autostart_enabled(&self.exe);
        let checked = |on: bool| if on { MF_CHECKED } else { 0 };
        unsafe {
            let menu = CreatePopupMenu();
            let levels = CreatePopupMenu();
            if menu.is_null() || levels.is_null() {
                return;
            }
            append(menu, MF_STRING, IDM_OPEN as usize, "&Open Sound Booster");
            SetMenuDefaultItem(menu, IDM_OPEN, 0);
            append(menu, MF_SEPARATOR, 0, "");
            append(
                menu,
                MF_STRING | checked(settings.enabled),
                IDM_ENABLED as usize,
                "&Enabled",
            );
            append(menu, MF_SEPARATOR, 0, "");
            for (index, preset) in PRESETS.iter().enumerate() {
                append(
                    menu,
                    MF_STRING,
                    (IDM_PRESET + index as u32) as usize,
                    preset.label(),
                );
            }
            if settings.enabled {
                let chosen = IDM_PRESET + settings.preset_index() as u32;
                CheckMenuRadioItem(
                    menu,
                    IDM_PRESET,
                    IDM_PRESET + PRESETS.len() as u32 - 1,
                    chosen,
                    MF_BYCOMMAND,
                );
            }
            for (index, percent) in MENU_LEVELS.iter().enumerate() {
                append(
                    levels,
                    MF_STRING | checked(*percent == settings.percent),
                    (IDM_LEVEL + index as u32) as usize,
                    &format!("{percent} %"),
                );
            }
            append(menu, MF_POPUP, levels as usize, "&Boost level");
            append(menu, MF_SEPARATOR, 0, "");
            append(
                menu,
                MF_STRING | checked(autostart),
                IDM_AUTOSTART as usize,
                "&Start with Windows",
            );
            append(menu, MF_SEPARATOR, 0, "");
            append(menu, MF_STRING, IDM_EXIT as usize, "E&xit");

            // Without this the menu does not close when the user clicks away.
            SetForegroundWindow(self.hwnd);
            let horizontal = if GetSystemMetrics(SM_MENUDROPALIGNMENT) != 0 {
                TPM_RIGHTALIGN
            } else {
                TPM_LEFTALIGN
            };
            let command = TrackPopupMenuEx(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | horizontal,
                anchor.x,
                anchor.y,
                self.hwnd,
                null(),
            );
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu); // also destroys the submenu
            if command > 0 {
                self.command(command as u32);
            }
        }
    }

    fn command(&self, id: u32) {
        let settings = self.settings();
        match id {
            IDM_OPEN => self.show_panel(None),
            IDM_ENABLED => self.apply(Settings {
                enabled: !settings.enabled,
                ..settings
            }),
            id if (IDM_PRESET..IDM_PRESET + PRESETS.len() as u32).contains(&id) => {
                self.apply(Settings {
                    enabled: true,
                    preset: PRESETS[(id - IDM_PRESET) as usize],
                    ..settings
                })
            }
            id if (IDM_LEVEL..IDM_LEVEL + MENU_LEVELS.len() as u32).contains(&id) => {
                self.apply(Settings {
                    percent: MENU_LEVELS[(id - IDM_LEVEL) as usize],
                    ..settings
                })
            }
            IDM_AUTOSTART => {
                let on = !store::autostart_enabled(&self.exe);
                if let Err(error) = store::set_autostart(&self.exe, on) {
                    message_box(
                        self.hwnd,
                        &format!("Could not change the startup setting:\n\n{error}"),
                        MB_ICONWARNING,
                    );
                }
            }
            IDM_EXIT => unsafe {
                DestroyWindow(self.hwnd);
            },
            id if id == IDCANCEL as u32 => self.hide_panel(),
            _ => {}
        }
    }

    /// Size the panel, place its controls and pick fonts for this DPI.
    fn apply_dpi(&self, dpi: u32) {
        let dpi = if dpi == 0 { 96 } else { dpi };
        let (font, title_font) = message_fonts(dpi);
        let (old_font, old_title_font) = {
            let mut state = self.state.borrow_mut();
            (
                std::mem::replace(&mut state.font, font),
                std::mem::replace(&mut state.title_font, title_font),
            )
        };
        unsafe {
            for (control, [x, y, width, height]) in self.controls.layout() {
                MoveWindow(
                    control,
                    scale(x, dpi),
                    scale(y, dpi),
                    scale(width, dpi),
                    scale(height, dpi),
                    1,
                );
                let font = if control == self.controls.title {
                    title_font
                } else {
                    font
                };
                SendMessageW(control, WM_SETFONT, font as usize, 1);
            }

            let mut frame = RECT {
                left: 0,
                top: 0,
                right: scale(PANEL_WIDTH, dpi),
                bottom: scale(PANEL_HEIGHT, dpi),
            };
            AdjustWindowRectExForDpi(
                &mut frame,
                WS_POPUP | WS_BORDER | WS_CLIPCHILDREN,
                0,
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_CONTROLPARENT,
                dpi,
            );
            SetWindowPos(
                self.hwnd,
                null_mut(),
                0,
                0,
                frame.right - frame.left,
                frame.bottom - frame.top,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE,
            );

            let stock = GetStockObject(DEFAULT_GUI_FONT);
            for old in [old_font, old_title_font] {
                if !old.is_null() && old != stock {
                    DeleteObject(old);
                }
            }
        }
        self.refresh_icon(false);
    }
}

/// The system message font at this DPI, and a larger semibold one for the
/// title. Falls back to the stock GUI font.
fn message_fonts(dpi: u32) -> (HFONT, HFONT) {
    unsafe {
        let mut metrics = NONCLIENTMETRICSW {
            cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
            ..zeroed()
        };
        let ok = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS,
            metrics.cbSize,
            (&mut metrics as *mut NONCLIENTMETRICSW).cast(),
            0,
            dpi,
        );
        if ok == 0 {
            let stock = GetStockObject(DEFAULT_GUI_FONT);
            return (stock, stock);
        }
        let font = CreateFontIndirectW(&metrics.lfMessageFont);
        let mut title = metrics.lfMessageFont;
        title.lfHeight = title.lfHeight * 5 / 4;
        title.lfWeight = FW_SEMIBOLD as i32;
        let title_font = CreateFontIndirectW(&title);
        (font, title_font)
    }
}

/// Build an `HICON` from the pixels [`icon::render`] draws.
fn make_icon(size: i32, enabled: bool, percent: u32) -> HICON {
    let pixels = icon::render(size as u32, enabled, percent);
    unsafe {
        let header = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size, // top-down, matching render()'s row order
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..zeroed()
        };
        let info = BITMAPINFO {
            bmiHeader: header,
            ..zeroed()
        };
        let mut bits: *mut core::ffi::c_void = null_mut();
        let colour = CreateDIBSection(null_mut(), &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if colour.is_null() || bits.is_null() {
            return null_mut();
        }
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u32>(), pixels.len());

        // The mask is ignored for a 32-bit colour bitmap with alpha but must
        // exist; all zeros means "use the colour bitmap everywhere".
        let stride = (size as usize).div_ceil(16) * 2;
        let mask_bits = vec![0u8; stride * size as usize];
        let mask = CreateBitmap(size, size, 1, 1, mask_bits.as_ptr().cast());

        let icon_info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: colour,
        };
        let icon = CreateIconIndirect(&icon_info);
        DeleteObject(colour);
        if !mask.is_null() {
            DeleteObject(mask);
        }
        icon
    }
}

fn trackbar_position(trackbar: HWND) -> i32 {
    unsafe { SendMessageW(trackbar, TBM_GETPOS, 0, 0) as i32 }
}

fn set_text(control: HWND, text: &str) {
    let text = wide(text);
    unsafe { SetWindowTextW(control, text.as_ptr()) };
}

unsafe fn append(menu: HMENU, flags: u32, id: usize, text: &str) {
    let text = wide(text);
    AppendMenuW(menu, flags, id, text.as_ptr());
}

fn message_box(owner: HWND, text: &str, icon: u32) {
    let text = wide(text);
    let title = wide(TITLE);
    unsafe { MessageBoxW(owner, text.as_ptr(), title.as_ptr(), MB_OK | icon) };
}

/// Copy `text` into a fixed UTF-16 buffer, truncating and NUL-terminating.
fn copy_into(buffer: &mut [u16], text: &str) {
    if buffer.is_empty() {
        return;
    }
    let room = buffer.len() - 1;
    let mut length = 0;
    for (slot, unit) in buffer[..room].iter_mut().zip(text.encode_utf16()) {
        *slot = unit;
        length += 1;
    }
    buffer[length] = 0;
}

fn scale(value: i32, dpi: u32) -> i32 {
    (value as i64 * dpi as i64 / 96) as i32
}

fn signed_low(value: usize) -> i32 {
    (value & 0xFFFF) as u16 as i16 as i32
}

fn signed_high(value: usize) -> i32 {
    ((value >> 16) & 0xFFFF) as u16 as i16 as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_coordinates_are_signed() {
        // A click on a monitor left of the primary one.
        let packed = ((1050u16 as usize) << 16) | ((-200i16) as u16 as usize);
        assert_eq!(signed_low(packed), -200);
        assert_eq!(signed_high(packed), 1050);
    }

    #[test]
    fn tooltips_are_truncated_and_terminated() {
        let mut buffer = [0xFFFFu16; 8];
        copy_into(&mut buffer, "Sound Booster");
        assert_eq!(String::from_utf16_lossy(&buffer[..7]), "Sound B");
        assert_eq!(buffer[7], 0);

        let mut roomy = [0xFFFFu16; 8];
        copy_into(&mut roomy, "on");
        assert_eq!(&roomy[..3], &[b'o' as u16, b'n' as u16, 0]);
    }

    #[test]
    fn layout_fits_inside_the_panel() {
        for [x, y, width, height] in [[16, 12, 268, 24], [16, 248, 268, 44], [10, 210, 280, 30]] {
            assert!(x >= 0 && x + width <= PANEL_WIDTH);
            assert!(y >= 0 && y + height <= PANEL_HEIGHT);
        }
        assert_eq!(scale(PANEL_WIDTH, 144), 450);
    }

    #[test]
    fn menu_ids_do_not_overlap() {
        assert!(IDM_PRESET + PRESETS.len() as u32 <= IDM_LEVEL);
        assert!(IDM_LEVEL + MENU_LEVELS.len() as u32 <= IDM_AUTOSTART);
        for percent in MENU_LEVELS {
            assert!((MIN..=MAX).contains(&percent));
        }
    }
}
