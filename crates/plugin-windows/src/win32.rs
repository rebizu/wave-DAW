//! The Win32 backend: the only file in the crate that knows what an `HWND` is.
//!
//! # Why raw Win32 and not `tao`
//! `tao` can only create windows from inside its own event loop, through an
//! `EventLoopWindowTarget` Tauri does not hand out where an editor is opened.
//! Plain Win32 windows created **on the same thread** as tao's loop need no
//! such handle: tao's `GetMessageW(hwnd = NULL)` + `DispatchMessageW` delivers
//! to every window of the thread, ours and the plugin's child alike.
//!
//! # Layout
//! A top-level window whose caption is removed (`WM_NCCALCSIZE`) so WAVE's header is the
//! title bar: `WM_NCHITTEST` answers `HTCAPTION` over it (Windows moves, snaps and
//! double-click-maximises as with any caption) and `HTCLIENT` over its buttons. Below the
//! header a child "host" window fills the rest: the plugin's editor is attached to that
//! child, so it can never paint over the header. A resizable window keeps the system's
//! side and bottom resize borders; its top edge resizes from the header's top pixels.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Once};

use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetStockObject, InvalidateRect, ScreenToClient, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLACK_BRUSH,
    DIB_RGB_COLORS, HBRUSH, PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindow, GetWindowLongPtrW, IsWindow, IsZoomed,
    KillTimer, MoveWindow, RegisterClassW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, CREATESTRUCTW,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, GW_CHILD, HTCAPTION, HTCLIENT, HTTOP, HTTOPLEFT, HTTOPRIGHT, HWND_NOTOPMOST,
    HWND_TOPMOST, MINMAXINFO, NCCALCSIZE_PARAMS, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNORMAL,
    USER_DEFAULT_SCREEN_DPI, WINDOW_EX_STYLE, WM_CLOSE, WM_DPICHANGED, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_NCCALCSIZE, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST, WM_NCMOUSEMOVE, WM_PAINT, WM_SIZE, WM_TIMER, WNDCLASSW,
    WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU, WS_THICKFRAME,
    WS_VISIBLE,
};

use crate::header::{self, Canvas, Part};
use crate::service::{WindowError, WindowEvent, WindowId, WindowOptions, WindowSize, WindowState, PUMP_INTERVAL_MS};

/// Window class names, unique to WAVE so they cannot collide with a plugin's own.
const CLASS_NAME: &str = "WavePluginWindow";
const HOST_CLASS_NAME: &str = "WavePluginHost";

/// Id of the wake-up timer. Plugin timers live on the plugin's own windows, so
/// collision is impossible.
const PUMP_TIMER_ID: usize = 0xDA11;

/// Pixels at the top of a resizable window's header that resize instead of move.
const TOP_RESIZE: i32 = 4;

const NO_EX_STYLE: WINDOW_EX_STYLE = 0;

/// Sent after `TrackMouseEvent(TME_LEAVE)` (not exported where windows-sys has the rest).
const WM_MOUSELEAVE: u32 = 0x02A3;

static CLASS: Once = Once::new();
/// `0` = registered, anything else = the `GetLastError` from the attempt.
static CLASS_ERROR: AtomicU32 = AtomicU32::new(u32::MAX);

const STYLE_RESIZABLE: u32 = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_CLIPCHILDREN;

/// No drag frame, no maximise box: an editor that cannot resize must not be
/// given a frame the user can drag.
const STYLE_FIXED: u32 = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;

fn style_for(resizable: bool) -> u32 {
    if resizable {
        STYLE_RESIZABLE
    } else {
        STYLE_FIXED
    }
}

/// UTF-16, NUL-terminated.
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error() -> u32 {
    // SAFETY: no preconditions; reads this thread's last-error value.
    unsafe { GetLastError() }
}

/// `GWL_STYLE` is not exported by `windows-sys` under a usable name here.
const GWL_STYLE: windows_sys::Win32::UI::WindowsAndMessaging::WINDOW_LONG_PTR_INDEX = -16;

fn ensure_class() -> Result<(), WindowError> {
    CLASS.call_once(|| {
        // SAFETY: `GetModuleHandleW(NULL)` returns this executable's instance
        // handle and cannot fail for the current process.
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        let mut error = 0;
        for (name, proc) in [(CLASS_NAME, window_proc as WndProc), (HOST_CLASS_NAME, host_proc as WndProc)] {
            let class_name = wide(name);
            let class = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: ptr::null_mut(),
                hCursor: ptr::null_mut(),
                // SAFETY: `BLACK_BRUSH` is a valid stock-object index; the system
                // owns the brush.
                hbrBackground: unsafe { GetStockObject(BLACK_BRUSH) } as HBRUSH,
                lpszMenuName: ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };
            // SAFETY: `class` and the name it points at outlive the call; Windows
            // copies both.
            if unsafe { RegisterClassW(&class) } == 0 && error == 0 {
                error = last_error();
            }
        }
        CLASS_ERROR.store(error, Ordering::Release);
    });
    match CLASS_ERROR.load(Ordering::Acquire) {
        0 => Ok(()),
        code => Err(WindowError::ClassRegistration(code)),
    }
}

type WndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

fn scale_of(dpi: u32) -> f32 {
    dpi.max(1) as f32 / USER_DEFAULT_SCREEN_DPI as f32
}

/// The frame around the client area: (left/right, bottom) border widths. A fixed window
/// has none (the caption is removed and there is no thick frame).
fn borders(style: u32, dpi: u32) -> (i32, i32) {
    if style & WS_THICKFRAME == 0 {
        return (0, 0);
    }
    let mut rect = RECT { left: 0, top: 0, right: 100, bottom: 100 };
    // SAFETY: `rect` is a valid, initialised RECT we own.
    unsafe {
        AdjustWindowRectExForDpi(&mut rect, style, 0, NO_EX_STYLE, dpi);
    }
    (-rect.left, rect.bottom - 100)
}

/// The outer size whose plugin area (below the header) is `size` at `dpi`.
fn outer_size_for(size: WindowSize, style: u32, dpi: u32) -> (i32, i32) {
    let (bx, by) = borders(style, dpi);
    let hh = header::height(scale_of(dpi)) as i32;
    (i32::try_from(size.width).unwrap_or(i32::MAX / 2) + 2 * bx, i32::try_from(size.height).unwrap_or(i32::MAX / 2) + hh + by)
}

pub(crate) fn create_window(
    options: &WindowOptions,
    size: WindowSize,
    events: Sender<WindowEvent>,
    ticks: Arc<AtomicU64>,
    owner: isize,
) -> Result<isize, WindowError> {
    ensure_class()?;
    // owned by the app window (not a child: it keeps its own frame), so it stays in front of it
    let owner = if alive(owner) { owner as HWND } else { ptr::null_mut() };
    let style = style_for(options.resizable);
    // The real DPI is unknown until the window exists (it depends on the
    // monitor), so create at the default scale and correct immediately after.
    let (w, h) = outer_size_for(size, style, USER_DEFAULT_SCREEN_DPI);
    let title = wide(&options.title);
    let class_name = wide(CLASS_NAME);

    let state = Box::into_raw(Box::new(WindowState::new(options, events, ticks)));

    // SAFETY: the class is registered, the strings are NUL-terminated and live
    // across the call, and `state` is a valid pointer whose ownership the window
    // procedure takes in WM_NCCREATE.
    let hwnd = unsafe {
        CreateWindowExW(
            NO_EX_STYLE,
            class_name.as_ptr(),
            title.as_ptr(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            w,
            h,
            owner,
            ptr::null_mut(),
            GetModuleHandleW(ptr::null()),
            state.cast::<c_void>(),
        )
    };
    if hwnd.is_null() {
        let code = last_error();
        // SAFETY: WM_NCCREATE never ran, so the state was never adopted.
        drop(unsafe { Box::from_raw(state) });
        return Err(WindowError::Creation(code));
    }
    let host_class = wide(HOST_CLASS_NAME);
    // SAFETY: `hwnd` is a window we just created on this thread; the host is its child.
    unsafe {
        let host = CreateWindowExW(
            NO_EX_STYLE,
            host_class.as_ptr(),
            ptr::null(),
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            0,
            0,
            1,
            1,
            hwnd,
            ptr::null_mut(),
            GetModuleHandleW(ptr::null()),
            ptr::null(),
        );
        if host.is_null() {
            let code = last_error();
            DestroyWindow(hwnd);
            return Err(WindowError::Creation(code));
        }
        if let Some(state) = state_of(hwnd) {
            state.embed.set(host as isize);
        }
        // the caption goes now that the state is in place (WM_NCCALCSIZE reads it)
        SetWindowPos(hwnd, ptr::null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED | 0x0001);
        apply_size(hwnd, size, style, GetDpiForWindow(hwnd));
        layout(hwnd);
        SetTimer(hwnd, PUMP_TIMER_ID, PUMP_INTERVAL_MS, None);
    }
    Ok(hwnd as isize)
}

/// The child window plugins embed into.
pub(crate) fn embed_handle(hwnd: isize) -> isize {
    if !alive(hwnd) {
        return 0;
    }
    // SAFETY: a live window of this class on this thread.
    unsafe { state_of(hwnd as HWND).map_or(0, |s| s.embed.get()) }
}

/// Whether `hwnd` is a live window (0 is the portable layer's "no window").
fn alive(hwnd: isize) -> bool {
    // SAFETY: `IsWindow` accepts any value and only answers.
    hwnd != 0 && unsafe { IsWindow(hwnd as HWND) } != 0
}

pub(crate) fn destroy_window(hwnd: isize) {
    if !alive(hwnd) {
        return;
    }
    let hwnd = hwnd as HWND;
    // SAFETY: created by this module on this thread; DestroyWindow frees the
    // boxed state through WM_NCDESTROY (and the host child with its parent).
    unsafe {
        KillTimer(hwnd, PUMP_TIMER_ID);
        DestroyWindow(hwnd);
    }
}

pub(crate) fn show_window(hwnd: isize) {
    if alive(hwnd) {
        // SAFETY: created by this module on this thread.
        // and in front: the click that asked for it may have gone to another process (the
        // webview), and then a plain show leaves a new window behind the app
        unsafe {
            ShowWindow(hwnd as HWND, SW_SHOWNORMAL);
            BringWindowToTop(hwnd as HWND);
            SetForegroundWindow(hwnd as HWND);
        }
    }
}

/// Pin (`pinned`) or unpin the window on top of the app's: `HWND_TOPMOST`, the one
/// always-on-top guarantee Windows keeps even where an owned window's stacking does not.
pub(crate) fn set_always_on_top(hwnd: isize, pinned: bool) {
    if !alive(hwnd) {
        return;
    }
    // SAFETY: created by this module on this thread; the state pointer is valid
    // until WM_NCDESTROY.
    unsafe {
        if let Some(state) = state_of(hwnd as HWND) {
            state.set_pinned_state(pinned);
        }
        SetWindowPos(
            hwnd as HWND,
            if pinned { HWND_TOPMOST } else { HWND_NOTOPMOST },
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        // the header's pin follows the state (the client rect's top strip, as `set_state`
        // repaints it: a zero-width rectangle would invalidate nothing)
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetClientRect(hwnd as HWND, &mut rect);
        rect.bottom = header_height(hwnd as HWND);
        InvalidateRect(hwnd as HWND, &rect, 0);
    }
}

pub(crate) fn request_close(hwnd: isize) {
    if !alive(hwnd) {
        return;
    }
    // SAFETY: a window of this class on this thread; the state pointer is valid
    // until WM_NCDESTROY, and `run_tick` keeps the handler alive on its own.
    unsafe {
        if let Some(state) = state_of(hwnd as HWND) {
            state.post(WindowEvent::Closed);
            state.run_tick();
        }
    }
}

pub(crate) fn set_tick_handler(hwnd: isize, handler: Option<Rc<dyn Fn()>>) {
    if !alive(hwnd) {
        return;
    }
    // SAFETY: as in `request_close`.
    unsafe {
        if let Some(state) = state_of(hwnd as HWND) {
            *state.tick.borrow_mut() = handler;
        }
    }
}

pub(crate) fn dpi(hwnd: isize) -> u32 {
    // SAFETY: returns 0 for an invalid window, mapped to the default scale.
    let dpi = unsafe { GetDpiForWindow(hwnd as HWND) };
    if dpi == 0 {
        USER_DEFAULT_SCREEN_DPI
    } else {
        dpi
    }
}

pub(crate) fn has_child_window(hwnd: isize) -> Result<bool, WindowError> {
    let host = embed_handle(hwnd);
    // SAFETY: created by this module on this thread.
    let child = unsafe { GetWindow(host as HWND, GW_CHILD) };
    Ok(!child.is_null())
}

pub(crate) fn embedded_size(hwnd: isize) -> Result<Option<WindowSize>, WindowError> {
    // SAFETY: created by this module on this thread; `rect` is ours.
    unsafe {
        let child = GetWindow(embed_handle(hwnd) as HWND, GW_CHILD);
        if child.is_null() {
            return Ok(None);
        }
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetClientRect(child, &mut rect);
        Ok(Some(WindowSize::new((rect.right - rect.left).max(0) as u32, (rect.bottom - rect.top).max(0) as u32)))
    }
}

pub(crate) fn client_size(id: WindowId, hwnd: isize) -> Result<WindowSize, WindowError> {
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: created by this module on this thread; `rect` is ours.
    if unsafe { GetClientRect(embed_handle(hwnd) as HWND, &mut rect) } == 0 {
        return Err(WindowError::Win32 { id, api: "GetClientRect", code: last_error() });
    }
    Ok(WindowSize { width: (rect.right - rect.left).max(0) as u32, height: (rect.bottom - rect.top).max(0) as u32 })
}

pub(crate) fn set_client_size(id: WindowId, hwnd: isize, size: WindowSize) -> Result<(), WindowError> {
    let hwnd = hwnd as HWND;
    // SAFETY: created by this module on this thread.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
    let dpi = dpi(hwnd as isize);
    // SAFETY: as above.
    if unsafe { apply_size(hwnd, size, style, dpi) } {
        Ok(())
    } else {
        Err(WindowError::Win32 { id, api: "SetWindowPos", code: last_error() })
    }
}

/// Resize `hwnd` so the plugin's area is exactly `size`, keeping its position.
///
/// # Safety
/// `hwnd` must be a live window owned by the calling thread.
unsafe fn apply_size(hwnd: HWND, size: WindowSize, style: u32, dpi: u32) -> bool {
    let (w, h) = outer_size_for(size, style, dpi);
    SetWindowPos(hwnd, ptr::null_mut(), 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE) != 0
}

/// Header height of `hwnd` now.
fn header_height(hwnd: HWND) -> i32 {
    header::height(scale_of(dpi(hwnd as isize))) as i32
}

/// Put the host child under the header and report the plugin area's size.
///
/// # Safety
/// `hwnd` must be a live window of this class on this thread.
unsafe fn layout(hwnd: HWND) {
    let Some(state) = state_of(hwnd) else { return };
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    GetClientRect(hwnd, &mut rect);
    let hh = header_height(hwnd);
    let (w, h) = (rect.right - rect.left, (rect.bottom - rect.top - hh).max(0));
    let host = state.embed.get();
    if host != 0 {
        MoveWindow(host as HWND, 0, hh, w, h, 1);
    }
    state.post(WindowEvent::Resized(WindowSize::new(w.max(0) as u32, h as u32)));
    InvalidateRect(hwnd, ptr::null(), 0);
}

/// The state a window carries. `None` before WM_NCCREATE and after WM_NCDESTROY.
///
/// # Safety
/// `hwnd` must be a window of this class, on this thread.
unsafe fn state_of<'a>(hwnd: HWND) -> Option<&'a WindowState> {
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState;
    pointer.as_ref()
}

/// The embed host's procedure: a plain black child.
unsafe extern "system" fn host_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, message, wparam, lparam)
}

/// The window procedure. Runs inside the UI loop's `DispatchMessageW`.
///
/// It queues events and returns; the only owner code it runs is the tick
/// handler, at `WM_TIMER`, which is always dispatched from the top of the loop,
/// and the settings handler, on a click of the header's gear.
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // A panic unwinding into Windows' frames is undefined behaviour.
    let handled = catch_unwind(AssertUnwindSafe(|| dispatch(hwnd, message, wparam, lparam))).unwrap_or(None);
    match handled {
        Some(result) => result,
        None => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn point_of(lparam: LPARAM) -> (i32, i32) {
    (i32::from((lparam & 0xFFFF) as u16 as i16), i32::from(((lparam >> 16) & 0xFFFF) as u16 as i16))
}

/// The header part under client point (`x`, `y`).
///
/// # Safety
/// `hwnd` must be a live window of this class on this thread.
unsafe fn part_at(hwnd: HWND, x: i32, y: i32) -> Option<Part> {
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    GetClientRect(hwnd, &mut rect);
    header::hit((rect.right - rect.left).max(0) as u32, x, y, scale_of(dpi(hwnd as isize)))
}

/// Repaint the header after a hover / press change.
///
/// # Safety
/// As [`part_at`].
unsafe fn set_state(hwnd: HWND, state: &WindowState, hover: Option<Part>, pressed: Option<Part>) {
    if state.set_header_state(hover, pressed) {
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetClientRect(hwnd, &mut rect);
        rect.bottom = header_height(hwnd);
        InvalidateRect(hwnd, &rect, 0);
    }
}

/// # Safety
/// Called only from [`window_proc`], with its arguments.
unsafe fn dispatch(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    match message {
        WM_NCCREATE => {
            let create = lparam as *const CREATESTRUCTW;
            if let Some(create) = create.as_ref() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            }
            None
        }
        WM_NCCALCSIZE if wparam != 0 => {
            let state = state_of(hwnd)?;
            let params = (lparam as *mut NCCALCSIZE_PARAMS).as_mut()?;
            if state.resizable {
                // the system's side and bottom borders stay (they resize); the caption goes
                let top = params.rgrc[0].top;
                DefWindowProcW(hwnd, message, wparam, lparam);
                params.rgrc[0].top = top;
                if IsZoomed(hwnd) != 0 {
                    // a maximised window hangs its border off screen: keep the header on it
                    let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
                    params.rgrc[0].top += borders(style, dpi(hwnd as isize)).1;
                }
            }
            Some(0)
        }
        WM_NCHITTEST => {
            let mut p = POINT { x: point_of(lparam).0, y: point_of(lparam).1 };
            ScreenToClient(hwnd, &mut p);
            let state = state_of(hwnd)?;
            let hh = header_height(hwnd);
            if p.y < 0 || p.y >= hh {
                return None;
            }
            let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            GetClientRect(hwnd, &mut rect);
            if state.resizable && IsZoomed(hwnd) == 0 && p.y < TOP_RESIZE {
                return Some(if p.x < 8 {
                    HTTOPLEFT
                } else if p.x >= rect.right - 8 {
                    HTTOPRIGHT
                } else {
                    HTTOP
                } as LRESULT);
            }
            Some(match part_at(hwnd, p.x, p.y) {
                Some(Part::Pin | Part::Settings | Part::Close) => HTCLIENT,
                _ => HTCAPTION,
            } as LRESULT)
        }
        WM_ERASEBKGND => Some(1),
        WM_PAINT => {
            let state = state_of(hwnd)?;
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            BeginPaint(hwnd, &mut ps);
            let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            GetClientRect(hwnd, &mut rect);
            let w = (rect.right - rect.left).max(1) as u32;
            let scale = scale_of(dpi(hwnd as isize));
            let h = header::height(scale);
            let mut px = vec![0_u32; (w * h) as usize];
            state.header.borrow().render(&mut Canvas { px: &mut px, width: w, height: h }, scale);
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            StretchDIBits(ps.hdc, 0, 0, w as i32, h as i32, 0, 0, w as i32, h as i32, px.as_ptr().cast(), &info, DIB_RGB_COLORS, SRCCOPY);
            EndPaint(hwnd, &ps);
            Some(0)
        }
        WM_MOUSEMOVE => {
            let state = state_of(hwnd)?;
            let (x, y) = point_of(lparam);
            let pressed = state.header.borrow().pressed;
            set_state(hwnd, state, part_at(hwnd, x, y), pressed);
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            TrackMouseEvent(&mut track);
            Some(0)
        }
        WM_NCMOUSEMOVE | WM_MOUSELEAVE => {
            let state = state_of(hwnd)?;
            let pressed = state.header.borrow().pressed;
            set_state(hwnd, state, None, pressed);
            None
        }
        WM_LBUTTONDOWN => {
            let state = state_of(hwnd)?;
            let (x, y) = point_of(lparam);
            if let Some(part @ (Part::Pin | Part::Settings | Part::Close)) = part_at(hwnd, x, y) {
                SetCapture(hwnd);
                set_state(hwnd, state, Some(part), Some(part));
            }
            Some(0)
        }
        WM_LBUTTONUP => {
            let state = state_of(hwnd)?;
            let (x, y) = point_of(lparam);
            let (pressed, now) = (state.header.borrow().pressed, part_at(hwnd, x, y));
            ReleaseCapture();
            set_state(hwnd, state, now, None);
            if let Some(part) = pressed.filter(|p| Some(*p) == now) {
                // may run the app's handlers; nothing below touches `state`
                state.header_click(part);
                if part == Part::Pin {
                    // the toggle already happened in `header_click`: apply it
                    set_always_on_top(hwnd as isize, state.pinned());
                }
            }
            Some(0)
        }
        WM_SIZE => {
            layout(hwnd);
            Some(0)
        }
        WM_GETMINMAXINFO => {
            let state = state_of(hwnd)?;
            let info = (lparam as *mut MINMAXINFO).as_mut()?;
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            let (w, h) = outer_size_for(state.min_size, style, dpi(hwnd as isize));
            info.ptMinTrackSize = POINT { x: w, y: h };
            Some(0)
        }
        WM_DPICHANGED => {
            let state = state_of(hwnd)?;
            let new_dpi = (wparam as u32) & 0xFFFF;
            // Taking the suggested rectangle verbatim keeps the window the same
            // physical size as it crosses monitors.
            if let Some(suggested) = (lparam as *const RECT).as_ref() {
                SetWindowPos(
                    hwnd,
                    ptr::null_mut(),
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            state.post(WindowEvent::DpiChanged { dpi: new_dpi });
            layout(hwnd);
            Some(0)
        }
        WM_TIMER if wparam == PUMP_TIMER_ID => {
            if let Some(state) = state_of(hwnd) {
                state.ticks.fetch_add(1, Ordering::Relaxed);
                // May destroy this window; nothing below touches `state`.
                state.run_tick();
            }
            Some(0)
        }
        WM_CLOSE => {
            // Report, do not destroy: the editor must be detached first.
            if let Some(state) = state_of(hwnd) {
                state.post(WindowEvent::Closed);
            }
            Some(0)
        }
        WM_NCDESTROY => {
            let pointer = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut WindowState;
            if !pointer.is_null() {
                drop(Box::from_raw(pointer));
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_nul_terminated() {
        assert_eq!(wide("hi"), vec![b'h' as u16, b'i' as u16, 0]);
        assert_eq!(wide(""), vec![0]);
        assert_eq!(wide("é").len(), 2);
    }

    #[test]
    fn a_fixed_window_has_no_drag_frame_and_a_resizable_one_does() {
        assert_eq!(style_for(true) & WS_THICKFRAME, WS_THICKFRAME);
        assert_eq!(style_for(false) & WS_THICKFRAME, 0);
        assert_eq!(style_for(false) & WS_MAXIMIZEBOX, 0);
        assert_eq!(style_for(true) & WS_CLIPCHILDREN, WS_CLIPCHILDREN);
        assert_eq!(style_for(false) & WS_CLIPCHILDREN, WS_CLIPCHILDREN);
    }

    #[test]
    fn the_outer_window_adds_the_header_and_only_resizable_borders() {
        let size = WindowSize::new(400, 250);
        let (w, h) = outer_size_for(size, style_for(false), USER_DEFAULT_SCREEN_DPI);
        assert_eq!((w, h), (400, 250 + 30), "fixed: the plugin plus the header, no frame");
        let (w, h) = outer_size_for(size, style_for(true), USER_DEFAULT_SCREEN_DPI);
        let (bx, by) = borders(style_for(true), USER_DEFAULT_SCREEN_DPI);
        assert!(bx > 0 && by > 0 && bx < 30, "{bx} {by}");
        assert_eq!((w, h), (400 + 2 * bx, 250 + 30 + by));
        let (_, h2) = outer_size_for(size, style_for(false), 192);
        assert_eq!(h2, 250 + 60, "the header scales with the monitor");
    }
}
