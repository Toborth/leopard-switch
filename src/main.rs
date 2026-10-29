#![windows_subsystem = "windows"]

mod ipc;
mod state;

use crate::ipc::{
    fetch_and_update_state, start_background_poller, start_subscriber, switch_to_workspace,
};
use crate::state::get_state;

use std::mem::zeroed;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::core::w;
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW,
    CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetMonitorInfoW, InvalidateRect, MonitorFromWindow, RoundRect, SelectObject, SetBkMode,
    SetTextColor, DT_CENTER, DT_SINGLELINE, DT_VCENTER, FW_BOLD, FW_NORMAL, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, PS_NULL, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    FindWindowW, GetAncestor, GetClassNameW, GetClientRect, GetCursorPos, GetDesktopWindow,
    GetForegroundWindow, GetMessageW, GetShellWindow, GetWindowRect, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, LoadCursorW, PostQuitMessage, RegisterClassExW, SetCursor,
    SetLayeredWindowAttributes, SetTimer, SetWindowPos, ShowWindow, TrackPopupMenu,
    TranslateMessage, GA_ROOT, IDC_ARROW, IDC_HAND, LWA_COLORKEY, MF_STRING, MSG, SWP_NOACTIVATE,
    SW_HIDE, SW_SHOWNA, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_DESTROY, WM_ERASEBKGND, WM_LBUTTONUP,
    WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

#[link(name = "user32")]
extern "system" {
    fn GetAsyncKeyState(vkey: i32) -> i16;
}

#[link(name = "shell32")]
extern "system" {
    fn SHQueryUserNotificationState(pquns: *mut i32) -> i32;
}

const VK_LBUTTON: i32 = 0x01;

// Notification states indicating exclusive fullscreen games / presentations
const QUNS_RUNNING_D3D_FULL_SCREEN: i32 = 3;
const QUNS_PRESENTATION_MODE: i32 = 4;

// Layout Metrics
const TASKBAR_LEFT_MARGIN: i32 = 8;
const BUTTON_GAP: i32 = 3;
const PADDING: i32 = 0;
const TIMER_ID: usize = 1;
const HWND_TOPMOST: HWND = HWND(-1isize as *mut _);

// Color Key: 100% transparent across the entire taskbar
const TRANSPARENT_COLOR_KEY: u32 = 0x00050505;

// Colors matching Windows 11 Taskbar
const COLOR_ACTIVE_INDICATOR: COLORREF = COLORREF(0x00F9B870); // Windows 11 Cyan (#4CC2FF)
const COLOR_BUSY_INDICATOR: COLORREF = COLORREF(0x00B9B8B7);   // Silver dot
const COLOR_HOVER_BG: COLORREF = COLORREF(0x00333333);         // Fluent hover tile
const COLOR_HOVER_BORDER: COLORREF = COLORREF(0x004A4A4A);     // 1px border
const COLOR_TEXT_ACTIVE: COLORREF = COLORREF(0x00FFFFFF);      // Bright White
const COLOR_TEXT_BUSY: COLORREF = COLORREF(0x00E0E0E0);        // Light Silver
const COLOR_TEXT_EMPTY: COLORREF = COLORREF(0x00E0E0E0);       // Legible gray

static BTN_SIZE: AtomicIsize = AtomicIsize::new(40);
static HOVERED_INDEX: AtomicIsize = AtomicIsize::new(-1);

/// Non-blocking 60 FPS tracker: inspects cursor coordinates directly without Windows hooks
fn start_mouse_tracker(hwnd: HWND) {
    let hwnd_raw = hwnd.0 as usize;

    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut _);
        let mut was_pressed = false;

        loop {
            std::thread::sleep(std::time::Duration::from_millis(16)); // ~60 FPS polling

            if !unsafe { IsWindowVisible(hwnd).as_bool() } {
                if HOVERED_INDEX.swap(-1, Ordering::Relaxed) != -1 {
                    unsafe {
                        let _ = InvalidateRect(hwnd, None, BOOL(0));
                    }
                }
                continue;
            }

            let mut win_rect = RECT::default();
            if unsafe { GetWindowRect(hwnd, &mut win_rect) }.is_err() {
                continue;
            }

            let mut pt = POINT::default();
            if unsafe { GetCursorPos(&mut pt) }.is_err() {
                continue;
            }

            let is_inside_window = pt.x >= win_rect.left
                && pt.x < win_rect.right
                && pt.y >= win_rect.top
                && pt.y < win_rect.bottom;

            let mut new_hover: isize = -1;
            let mut hit_ws_id: Option<usize> = None;

            if is_inside_window {
                let rel_x = pt.x - win_rect.left;
                let btn_size = BTN_SIZE.load(Ordering::Relaxed) as i32;
                let workspaces = get_state().lock().unwrap().workspaces.clone();

                for (i, ws) in workspaces.iter().enumerate() {
                    let btn_left = PADDING + (i as i32 * (btn_size + BUTTON_GAP));
                    let btn_right = btn_left + btn_size;
                    if rel_x >= btn_left && rel_x <= btn_right {
                        new_hover = i as isize;
                        hit_ws_id = Some(ws.id);
                        break;
                    }
                }
            }

            if HOVERED_INDEX.swap(new_hover, Ordering::Relaxed) != new_hover {
                unsafe {
                    let _ = InvalidateRect(hwnd, None, BOOL(0));
                }
            }

            let is_pressed = unsafe { GetAsyncKeyState(VK_LBUTTON) } < 0;
            if is_pressed && !was_pressed {
                if let Some(ws_id) = hit_ws_id {
                    let should_switch = {
                        let mut state = get_state().lock().unwrap();
                        if state.active_workspace != ws_id {
                            state.active_workspace = ws_id;
                            for w in state.workspaces.iter_mut() {
                                w.is_active = w.id == ws_id;
                            }
                            true
                        } else {
                            false
                        }
                    };

                    if should_switch {
                        unsafe {
                            dock_to_taskbar(hwnd);
                            let _ = InvalidateRect(hwnd, None, BOOL(0));
                        }
                        switch_to_workspace(ws_id);
                    }
                }
            }
            was_pressed = is_pressed;
        }
    });
}

/// Determines if the taskbar is currently visible on the monitor hosting `our_hwnd`.
pub unsafe fn is_taskbar_visible(our_hwnd: HWND) -> bool {
    // 1. Locate the taskbar window
    let taskbar = match FindWindowW(w!("Shell_TrayWnd"), None) {
        Ok(h) if !h.is_invalid() && h.0 as usize != 0 => h,
        _ => return false,
    };

    if !IsWindowVisible(taskbar).as_bool() {
        return false;
    }

    // 2. Auto-hide check: verify whether the taskbar is currently retracted
    let mut tb_rect = RECT::default();
    if GetWindowRect(taskbar, &mut tb_rect).is_ok() {
        if (tb_rect.bottom - tb_rect.top) <= 4 || (tb_rect.right - tb_rect.left) <= 4 {
            return false;
        }
    } else {
        return false;
    }

    // 3. Direct Shell Check for Exclusive Fullscreen 3D Games and Presentations
    let mut notification_state: i32 = 0;
    if SHQueryUserNotificationState(&mut notification_state) == 0 {
        if notification_state == QUNS_RUNNING_D3D_FULL_SCREEN
            || notification_state == QUNS_PRESENTATION_MODE
        {
            return false;
        }
    }

    // 4. Check for active borderless/fullscreen user applications (e.g. YouTube, VLC, Games)
    let fg_hwnd = GetForegroundWindow();
    if !fg_hwnd.is_invalid() && fg_hwnd.0 as usize != 0 {
        let root_fg = GetAncestor(fg_hwnd, GA_ROOT);
        let desktop = GetDesktopWindow();
        let shell = GetShellWindow();

        if root_fg != our_hwnd && root_fg != taskbar && root_fg != desktop && root_fg != shell {
            if !IsIconic(root_fg).as_bool() && IsWindowVisible(root_fg).as_bool() {
                // If the foreground window belongs to explorer (tray, desktop, flyouts), it's not a fullscreen app
                let mut shell_pid: u32 = 0;
                let mut fg_pid: u32 = 0;
                GetWindowThreadProcessId(taskbar, Some(&mut shell_pid));
                GetWindowThreadProcessId(root_fg, Some(&mut fg_pid));

                if fg_pid != 0 && fg_pid != shell_pid {
                    let mut class_buf = [0u16; 64];
                    let len = GetClassNameW(root_fg, &mut class_buf);
                    let class_name = String::from_utf16_lossy(&class_buf[..len as usize]);

                    // Exclude Windows Shell / UWP flyouts (Start menu, Search, Quick Settings, etc.)
                    let is_shell_ui = class_name.starts_with("Windows.UI.")
                        || class_name.starts_with("Xaml")
                        || class_name == "Progman"
                        || class_name == "WorkerW"
                        || class_name == "NotifyIconOverflowWindow"
                        || class_name == "TrayFlyoutWClass"
                        || class_name == "Shell_InputSwitchTopLevelWindow";

                    if !is_shell_ui {
                        let hmon = MonitorFromWindow(our_hwnd, MONITOR_DEFAULTTONEAREST);
                        let mut mi = MONITORINFO {
                            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                            ..Default::default()
                        };

                        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
                            let mut fg_rect = RECT::default();
                            if GetWindowRect(root_fg, &mut fg_rect).is_ok() {
                                // A true fullscreen window covers the entire monitor rect
                                if fg_rect.left <= mi.rcMonitor.left
                                    && fg_rect.top <= mi.rcMonitor.top
                                    && fg_rect.right >= mi.rcMonitor.right
                                    && fg_rect.bottom >= mi.rcMonitor.bottom
                                {
                                    return false;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    true
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),

        WM_SETCURSOR => {
            let cursor = if HOVERED_INDEX.load(Ordering::Relaxed) >= 0 {
                LoadCursorW(None, IDC_HAND)
            } else {
                LoadCursorW(None, IDC_ARROW)
            };
            if let Ok(c) = cursor {
                let _ = SetCursor(c);
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }

        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);

            let mut client_rect: RECT = zeroed();
            let _ = GetClientRect(hwnd, &mut client_rect);
            let width = client_rect.right - client_rect.left;
            let height = client_rect.bottom - client_rect.top;

            let mem_dc = CreateCompatibleDC(hdc);
            let mem_bmp = CreateCompatibleBitmap(hdc, width, height);
            let old_bmp = SelectObject(mem_dc, mem_bmp);

            let key_brush = CreateSolidBrush(COLORREF(TRANSPARENT_COLOR_KEY));
            FillRect(mem_dc, &client_rect, key_brush);
            let _ = DeleteObject(key_brush);

            SetBkMode(mem_dc, TRANSPARENT);

            let workspaces = {
                let s = get_state().lock().unwrap();
                s.workspaces.clone()
            };

            let btn_size = BTN_SIZE.load(Ordering::Relaxed) as i32;
            let hovered_idx = HOVERED_INDEX.load(Ordering::Relaxed);

            let font_h = (btn_size as f32 * 0.64) as i32;
            let font_normal = CreateFontW(
                font_h, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 0, 0,
                w!("Segoe UI Variable Small"),
            );
            let font_busy = CreateFontW(
                font_h, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 0, 0,
                w!("Segoe UI Variable Small"),
            );
            let font_active = CreateFontW(
                font_h, 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 0, 0,
                w!("Segoe UI Variable Small"),
            );

            let null_pen = CreatePen(PS_NULL, 0, COLORREF(0));

            for (i, ws) in workspaces.iter().enumerate() {
                let x = PADDING + (i as i32 * (btn_size + BUTTON_GAP));
                let y = PADDING;

                let btn_rect = RECT {
                    left: x,
                    top: y,
                    right: x + btn_size,
                    bottom: y + btn_size,
                };

                let is_hovered = hovered_idx == i as isize;

                if is_hovered {
                    let hover_brush = CreateSolidBrush(COLOR_HOVER_BG);
                    let hover_pen = CreatePen(PS_SOLID, 1, COLOR_HOVER_BORDER);
                    let old_p = SelectObject(mem_dc, hover_pen);
                    let old_b = SelectObject(mem_dc, hover_brush);

                    let _ = RoundRect(
                        mem_dc,
                        btn_rect.left,
                        btn_rect.top,
                        btn_rect.right,
                        btn_rect.bottom,
                        6,
                        6,
                    );

                    let _ = SelectObject(mem_dc, old_p);
                    let _ = SelectObject(mem_dc, old_b);
                    let _ = DeleteObject(hover_brush);
                    let _ = DeleteObject(hover_pen);
                }

                let text_color = if ws.is_active {
                    COLOR_TEXT_ACTIVE
                } else if ws.has_windows {
                    COLOR_TEXT_BUSY
                } else {
                    COLOR_TEXT_EMPTY
                };

                SetTextColor(mem_dc, text_color);
                let chosen_font = if ws.is_active {
                    font_active
                } else if ws.has_windows {
                    font_busy
                } else {
                    font_normal
                };
                let old_font = SelectObject(mem_dc, chosen_font);

                let mut label: Vec<u16> = format!("{}", ws.id).encode_utf16().collect();
                let mut text_rect = btn_rect;
                text_rect.bottom -= 3;
                let _ = DrawTextW(
                    mem_dc,
                    &mut label,
                    &mut text_rect,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
                let _ = SelectObject(mem_dc, old_font);

                if ws.is_active {
                    let bar_w = ((btn_size as f32) * 0.56) as i32;
                    let bar_h = 3;
                    let bar_left = btn_rect.left + (btn_size + 1 - bar_w) / 2;
                    let bar_top = btn_rect.bottom - bar_h - 1;

                    let brush = CreateSolidBrush(COLOR_ACTIVE_INDICATOR);
                    let old_p = SelectObject(mem_dc, null_pen);
                    let old_b = SelectObject(mem_dc, brush);

                    let _ = RoundRect(
                        mem_dc,
                        bar_left,
                        bar_top,
                        bar_left + bar_w,
                        bar_top + bar_h,
                        3,
                        3,
                    );

                    let _ = SelectObject(mem_dc, old_b);
                    let _ = SelectObject(mem_dc, old_p);
                    let _ = DeleteObject(brush);
                } else if ws.has_windows {
                    let dot_w = 7;
                    let dot_h = 3;
                    let dot_left = btn_rect.left + (btn_size + 1 - dot_w) / 2;
                    let dot_top = btn_rect.bottom - dot_h - 1;

                    let brush = CreateSolidBrush(COLOR_BUSY_INDICATOR);
                    let old_p = SelectObject(mem_dc, null_pen);
                    let old_b = SelectObject(mem_dc, brush);

                    let _ = RoundRect(
                        mem_dc,
                        dot_left,
                        dot_top,
                        dot_left + dot_w,
                        dot_top + dot_h,
                        3,
                        3,
                    );

                    let _ = SelectObject(mem_dc, old_b);
                    let _ = SelectObject(mem_dc, old_p);
                    let _ = DeleteObject(brush);
                }
            }

            let _ = DeleteObject(font_normal);
            let _ = DeleteObject(font_active);
            let _ = DeleteObject(null_pen);

            let _ = BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

            let _ = SelectObject(mem_dc, old_bmp);
            let _ = DeleteObject(mem_bmp);
            let _ = DeleteDC(mem_dc);

            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }

        WM_LBUTTONUP => {
            let x = (lparam.0 & 0xFFFF) as i32;
            let workspaces = get_state().lock().unwrap().workspaces.clone();
            let btn_size = BTN_SIZE.load(Ordering::Relaxed) as i32;

            for (i, ws) in workspaces.iter().enumerate() {
                let btn_left = PADDING + (i as i32 * (btn_size + BUTTON_GAP));
                let btn_right = btn_left + btn_size;

                if x >= btn_left && x <= btn_right {
                    {
                        let mut state = get_state().lock().unwrap();
                        state.active_workspace = ws.id;
                        for w in state.workspaces.iter_mut() {
                            w.is_active = w.id == ws.id;
                        }
                    }
                    let _ = InvalidateRect(hwnd, None, BOOL(0));
                    switch_to_workspace(ws.id);
                    break;
                }
            }
            LRESULT(0)
        }

        WM_RBUTTONUP => {
            let mut pt = POINT {
                x: (lparam.0 & 0xFFFF) as i32,
                y: ((lparam.0 >> 16) & 0xFFFF) as i32,
            };
            let _ = ClientToScreen(hwnd, &mut pt);

            let menu = CreatePopupMenu().unwrap();
            let _ = AppendMenuW(menu, MF_STRING, 101, w!("Refresh"));
            let _ = AppendMenuW(menu, MF_STRING, 102, w!("Exit"));

            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD,
                pt.x,
                pt.y,
                0,
                hwnd,
                None,
            );
            let _ = DestroyMenu(menu);

            if cmd.0 == 101 {
                fetch_and_update_state();
                let _ = InvalidateRect(hwnd, None, BOOL(0));
            } else if cmd.0 == 102 {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }

        WM_TIMER => {
            let visible = unsafe { is_taskbar_visible(hwnd) };
            let is_currently_visible = unsafe { IsWindowVisible(hwnd).as_bool() };

            if visible {
                dock_to_taskbar(hwnd);
                if !is_currently_visible {
                    unsafe {
                        let _ = ShowWindow(hwnd, SW_SHOWNA);
                    }
                }
            } else {
                if is_currently_visible {
                    unsafe {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                    }
                }
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn dock_to_taskbar(hwnd: HWND) {
    let ws_count = get_state().lock().unwrap().workspaces.len() as i32;
    if ws_count == 0 {
        return;
    }

    let taskbar = FindWindowW(w!("Shell_TrayWnd"), None);
    if let Ok(tb_hwnd) = taskbar {
        let mut tb_rect: RECT = zeroed();
        let _ = GetWindowRect(tb_hwnd, &mut tb_rect);

        let tb_height = tb_rect.bottom - tb_rect.top + 2;
        if tb_height <= 0 {
            return;
        }

        let btn_size = (tb_height - 6).clamp(18, 32);
        BTN_SIZE.store(btn_size as isize, Ordering::Relaxed);

        let width = (PADDING * 2) + (ws_count * btn_size) + ((ws_count - 1) * BUTTON_GAP);
        let height = btn_size + (PADDING * 2);

        let x = tb_rect.left + TASKBAR_LEFT_MARGIN;
        let y = tb_rect.top + (tb_height - height) / 2;

        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE,
        );
    }
}

fn main() {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap();

        let class_name = w!("RAIL_WINDOW");

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..zeroed()
        };

        RegisterClassExW(&wc);

        let tb_hwnd = FindWindowW(w!("Shell_TrayWnd"), None).unwrap_or_default();

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            class_name,
            w!("LeopardSwitch"),
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            300,
            32,
            tb_hwnd,
            None,
            instance,
            None,
        )
        .expect("Failed to create switcher window");

        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(TRANSPARENT_COLOR_KEY), 0, LWA_COLORKEY);

        fetch_and_update_state();
        dock_to_taskbar(hwnd);

        // Start non-blocking mouse tracker, fast poller & event stream
        start_mouse_tracker(hwnd);
        start_background_poller(hwnd);
        start_subscriber(hwnd);

        // Check visibility state every 250ms
        let _ = SetTimer(hwnd, TIMER_ID, 250, None);

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
