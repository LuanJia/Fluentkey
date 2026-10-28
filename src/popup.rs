//! Native tool window, independent of the hidden eframe settings viewport.
use anyhow::{ensure, Result};
use std::{
    cell::RefCell,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::atomic::{AtomicBool, AtomicIsize, Ordering},
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::DwmSetWindowAttribute, Gdi::*},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::SetWindowTheme,
        HiDpi::{GetDpiForSystem, GetDpiForWindow},
        WindowsAndMessaging::*,
    },
};

const COPY: usize = 1001;
const CLOSE: usize = 1002;
static ACTIVE_WINDOW: AtomicIsize = AtomicIsize::new(0);
static AUTO_CLOSE: AtomicBool = AtomicBool::new(true);
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn native_lines(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r\n")
}
fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 8) | (b << 16)
}
fn inside(rect: &RECT, point: POINT) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}
pub unsafe fn close_active() {
    let window = ACTIVE_WINDOW.load(Ordering::SeqCst) as HWND;
    if !window.is_null() {
        DestroyWindow(window);
    }
}

/// Called by the existing mouse hook. Never changes focus or blocks on UI work.
pub fn mouse_down(point: POINT) {
    let window = ACTIVE_WINDOW.load(Ordering::SeqCst) as HWND;
    if window.is_null() || !AUTO_CLOSE.load(Ordering::SeqCst) {
        return;
    }
    unsafe {
        let mut rect = zeroed();
        if GetWindowRect(window, &mut rect) != 0 && !inside(&rect, point) {
            PostMessageW(window, WM_CLOSE, 0, 0);
        }
    }
}

struct State {
    token: u64,
    owned: bool,
    text: String,
    heading: HWND,
    status: HWND,
    body: HWND,
    copy: HWND,
    close: HWND,
    footer: HWND,
    font: HFONT,
    scale: f32,
}
impl State {
    fn new(token: u64) -> Self {
        Self {
            token,
            owned: false,
            text: String::new(),
            heading: null_mut(),
            status: null_mut(),
            body: null_mut(),
            copy: null_mut(),
            close: null_mut(),
            footer: null_mut(),
            font: null_mut(),
            scale: 1.0,
        }
    }
    fn px(&self, value: i32) -> i32 {
        (value as f32 * self.scale).round() as i32
    }
    unsafe fn font(&mut self, window: HWND) {
        self.scale = GetDpiForWindow(window).max(96) as f32 / 96.;
        let font = CreateFontW(
            -self.px(14),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            DEFAULT_PITCH as u32,
            wide("Microsoft YaHei UI").as_ptr(),
        );
        if !font.is_null() {
            for child in [
                self.heading,
                self.status,
                self.body,
                self.copy,
                self.close,
                self.footer,
            ] {
                if !child.is_null() {
                    SendMessageW(child, WM_SETFONT, font as usize, 1);
                }
            }
            if !self.font.is_null() {
                DeleteObject(self.font);
            }
            self.font = font;
        }
    }
    unsafe fn layout(&self, window: HWND) {
        let mut rect: RECT = zeroed();
        GetClientRect(window, &mut rect);
        let w = rect.right;
        let h = rect.bottom;
        let p = self.px(18);
        MoveWindow(self.heading, p, p, w - 2 * p, self.px(24), 1);
        MoveWindow(self.status, p, self.px(46), w - 2 * p, self.px(22), 1);
        let body_top = if GetWindowTextLengthW(self.status) > 0 {
            76
        } else {
            50
        };
        MoveWindow(
            self.body,
            p,
            self.px(body_top),
            w - 2 * p,
            (h - self.px(body_top + 62)).max(self.px(40)),
            1,
        );
        let bottom = h - self.px(46);
        MoveWindow(self.footer, p, bottom, self.px(210), self.px(30), 1);
        MoveWindow(
            self.copy,
            w - p - self.px(204),
            bottom,
            self.px(112),
            self.px(30),
            1,
        );
        MoveWindow(
            self.close,
            w - p - self.px(80),
            bottom,
            self.px(80),
            self.px(30),
            1,
        );
    }
}
impl Drop for State {
    fn drop(&mut self) {
        unsafe {
            if !self.font.is_null() {
                DeleteObject(self.font);
            }
        }
    }
}
unsafe fn child(parent: HWND, class: &str, text: &str, style: u32, id: usize) -> HWND {
    CreateWindowExW(
        0,
        wide(class).as_ptr(),
        wide(text).as_ptr(),
        WS_CHILD | WS_VISIBLE | style,
        0,
        0,
        1,
        1,
        parent,
        id as HMENU,
        GetModuleHandleW(null()),
        null(),
    )
}
unsafe extern "system" fn window_proc(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(l as *const CREATESTRUCTW);
        SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
        return 1;
    }
    // Painting and control notifications can reenter during SendMessage or
    // MoveWindow. Do not alias a mutable Rust state reference across Win32 calls.
    if message == WM_ERASEBKGND || message == WM_CTLCOLORSTATIC || message == WM_CTLCOLOREDIT {
        let dc = w as HDC;
        SetBkColor(dc, rgb(17, 24, 32));
        SetDCBrushColor(dc, rgb(17, 24, 32));
        let brush = GetStockObject(DC_BRUSH) as HBRUSH;
        if message == WM_ERASEBKGND {
            let mut rect = zeroed();
            GetClientRect(window, &mut rect);
            FillRect(dc, &rect, brush);
            return 1;
        }
        SetTextColor(
            dc,
            if GetDlgCtrlID(l as HWND) == 1003 {
                rgb(114, 232, 190)
            } else {
                rgb(229, 236, 242)
            },
        );
        return brush as LRESULT;
    }
    let pointer = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut RefCell<State>;
    if pointer.is_null() {
        return DefWindowProcW(window, message, w, l);
    }
    let cell = &*pointer;
    if message == WM_NCDESTROY {
        let owned = cell.borrow().owned;
        ACTIVE_WINDOW
            .compare_exchange(window as isize, 0, Ordering::SeqCst, Ordering::SeqCst)
            .ok();
        SetWindowLongPtrW(window, GWLP_USERDATA, 0);
        if owned {
            drop(Box::from_raw(pointer));
        }
        return DefWindowProcW(window, message, w, l);
    }
    if message == WM_COMMAND && w >> 16 != 0 {
        return 0;
    }
    if message == WM_CLOSE {
        if cell.try_borrow_mut().is_err() {
            PostMessageW(window, WM_CLOSE, 0, 0);
        } else {
            DestroyWindow(window);
        }
        return 0;
    }
    if message == WM_COMMAND && w & 0xffff == CLOSE {
        PostMessageW(window, WM_CLOSE, 0, 0);
        return 0;
    }
    let Ok(mut state) = cell.try_borrow_mut() else {
        return DefWindowProcW(window, message, w, l);
    };
    match message {
        WM_CREATE => {
            state.heading = child(window, "STATIC", "FluentKey · 中文翻译", 0, 1003);
            state.status = child(window, "STATIC", "正在翻译选中的文字…", 0, 0);
            state.body = child(
                window,
                "EDIT",
                "请稍候…",
                WS_VSCROLL
                    | WS_TABSTOP
                    | ES_MULTILINE as u32
                    | ES_AUTOVSCROLL as u32
                    | ES_READONLY as u32
                    | ES_NOHIDESEL as u32,
                0,
            );
            state.copy = child(window, "BUTTON", "复制译文", WS_TABSTOP, COPY);
            state.close = child(window, "BUTTON", "关闭", WS_TABSTOP, CLOSE);
            let hint = if AUTO_CLOSE.load(Ordering::SeqCst) {
                "点击窗外自动关闭"
            } else {
                "自动关闭已关闭"
            };
            state.footer = child(window, "STATIC", hint, 0, 0);
            if [
                state.heading,
                state.status,
                state.body,
                state.copy,
                state.close,
                state.footer,
            ]
            .iter()
            .any(|window| window.is_null())
            {
                return -1;
            }
            EnableWindow(state.copy, 0);
            state.font(window);
            let theme = wide("DarkMode_Explorer");
            for control in [state.body, state.copy, state.close] {
                SetWindowTheme(control, theme.as_ptr(), null());
            }
            let dark: i32 = 1;
            DwmSetWindowAttribute(
                window,
                20,
                &dark as *const _ as *const _,
                size_of::<i32>() as u32,
            );
            state.layout(window);
            0
        }
        WM_SIZE => {
            state.layout(window);
            0
        }
        WM_GETMINMAXINFO => {
            let info = &mut *(l as *mut MINMAXINFO);
            info.ptMinTrackSize = POINT {
                x: state.px(500),
                y: state.px(280),
            };
            0
        }
        WM_DPICHANGED => {
            state.font(window);
            let rect = &*(l as *const RECT);
            SetWindowPos(
                window,
                null_mut(),
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            state.layout(window);
            0
        }
        WM_COMMAND => {
            match w & 0xffff {
                COPY if !state.text.is_empty() => {
                    let copied = arboard::Clipboard::new()
                        .and_then(|mut clipboard| clipboard.set_text(state.text.clone()))
                        .is_ok();
                    SetWindowTextW(
                        state.copy,
                        wide(if copied {
                            "已复制"
                        } else {
                            "复制失败，重试"
                        })
                        .as_ptr(),
                    );
                }
                _ => {}
            }
            0
        }
        _ => DefWindowProcW(window, message, w, l),
    }
}

pub struct TranslationPopup {
    window: HWND,
}
impl Default for TranslationPopup {
    fn default() -> Self {
        Self { window: null_mut() }
    }
}
impl TranslationPopup {
    fn current(&self) -> bool {
        !self.window.is_null()
            && ACTIVE_WINDOW.load(Ordering::SeqCst) == self.window as isize
            && unsafe { IsWindow(self.window) } != 0
    }
    pub fn close(&mut self) {
        if self.current() {
            unsafe {
                DestroyWindow(self.window);
            }
        }
        self.window = null_mut();
    }
    pub fn set_auto_close(&mut self, enabled: bool) {
        AUTO_CLOSE.store(enabled, Ordering::SeqCst);
        if self.current() {
            unsafe {
                let cell =
                    &*(GetWindowLongPtrW(self.window, GWLP_USERDATA) as *const RefCell<State>);
                let footer = cell.borrow().footer;
                SetWindowTextW(
                    footer,
                    wide(if enabled {
                        "点击窗外自动关闭"
                    } else {
                        "自动关闭已关闭"
                    })
                    .as_ptr(),
                );
            }
        }
    }
    pub fn loading(&mut self, token: u64, auto_close: bool) -> Result<()> {
        self.close();
        AUTO_CLOSE.store(auto_close, Ordering::SeqCst);
        unsafe {
            let class = wide("FluentKey.TranslationPopup");
            let module = GetModuleHandleW(null());
            let spec = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: module,
                lpszClassName: class.as_ptr(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                ..zeroed()
            };
            RegisterClassW(&spec);
            let mut point = POINT { x: 0, y: 0 };
            GetCursorPos(&mut point);
            let scale = GetDpiForSystem().max(96) as f32 / 96.;
            let width = (540. * scale) as i32;
            let height = (390. * scale) as i32;
            let mut monitor: MONITORINFO = zeroed();
            monitor.cbSize = size_of::<MONITORINFO>() as u32;
            let work = if GetMonitorInfoW(
                MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST),
                &mut monitor,
            ) != 0
            {
                monitor.rcWork
            } else {
                RECT {
                    left: 0,
                    top: 0,
                    right: GetSystemMetrics(SM_CXSCREEN),
                    bottom: GetSystemMetrics(SM_CYSCREEN),
                }
            };
            let width = width.min(work.right - work.left);
            let height = height.min(work.bottom - work.top);
            let x = (point.x + 16).clamp(work.left, (work.right - width).max(work.left));
            let y = (point.y + 20).clamp(work.top, (work.bottom - height).max(work.top));
            let mut state = Box::new(RefCell::new(State::new(token)));
            let pointer = &mut *state as *mut RefCell<State>;
            let window = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class.as_ptr(),
                wide("FluentKey · 中文翻译").as_ptr(),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_THICKFRAME,
                x,
                y,
                width,
                height,
                null_mut(),
                null_mut(),
                module,
                pointer as *const _,
            );
            ensure!(!window.is_null(), "无法创建中文翻译悬浮窗");
            state.borrow_mut().owned = true;
            let _ = Box::into_raw(state);
            self.window = window;
            ACTIVE_WINDOW.store(window as isize, Ordering::SeqCst);
            // The cursor may be on a monitor with a different DPI from the
            // system default. Refit using the actual new window's DPI.
            let actual_scale = GetDpiForWindow(window).max(96) as f32 / 96.;
            let actual_width = (540. * actual_scale) as i32;
            let actual_height = (390. * actual_scale) as i32;
            let actual_width = actual_width.min(work.right - work.left);
            let actual_height = actual_height.min(work.bottom - work.top);
            let x = (point.x + 16).clamp(work.left, (work.right - actual_width).max(work.left));
            let y = (point.y + 20).clamp(work.top, (work.bottom - actual_height).max(work.top));
            SetWindowPos(
                window,
                HWND_TOPMOST,
                x,
                y,
                actual_width,
                actual_height,
                SWP_NOACTIVATE,
            );
            ShowWindow(window, SW_SHOWNOACTIVATE);
        }
        Ok(())
    }
    /// A dismissed loading window stays dismissed when the network returns.
    pub fn finish(&mut self, token: u64, result: Result<&str, &str>) -> bool {
        if !self.current() {
            return false;
        }
        unsafe {
            let cell = &*(GetWindowLongPtrW(self.window, GWLP_USERDATA) as *const RefCell<State>);
            let mut state = cell.borrow_mut();
            if state.token != token {
                return false;
            }
            let (body, status) = match result {
                Ok(text) => {
                    state.text = text.to_owned();
                    (text, "")
                }
                Err(error) => {
                    state.text.clear();
                    (error, "翻译失败")
                }
            };
            let (body_window, status_window, copy, enabled) =
                (state.body, state.status, state.copy, !state.text.is_empty());
            drop(state);
            SetWindowTextW(body_window, wide(&native_lines(body)).as_ptr());
            SetWindowTextW(status_window, wide(status).as_ptr());
            ShowWindow(
                status_window,
                if status.is_empty() {
                    SW_HIDE
                } else {
                    SW_SHOWNOACTIVATE
                },
            );
            cell.borrow().layout(self.window);
            EnableWindow(copy, i32::from(enabled));
        }
        true
    }
}
impl Drop for TranslationPopup {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_edit_newlines_preserve_paragraphs_without_doubling_crlf() {
        assert_eq!(native_lines("第一段\n\n第二段"), "第一段\r\n\r\n第二段");
        assert_eq!(native_lines("第一段\r\n第二段"), "第一段\r\n第二段");
    }
    // Optional visual QA artifact of this test's own window, never another app.
    fn snapshot(window: HWND) {
        let Some(path) = std::env::var_os("FLUENTKEY_POPUP_SNAPSHOT") else {
            return;
        };
        unsafe {
            RedrawWindow(
                window,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW,
            );
            let mut rect: RECT = zeroed();
            GetClientRect(window, &mut rect);
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;
            let dc = GetDC(window);
            let memory = CreateCompatibleDC(dc);
            let bitmap = CreateCompatibleBitmap(dc, width, height);
            let old = SelectObject(memory, bitmap);
            BitBlt(memory, 0, 0, width, height, dc, 0, 0, SRCCOPY);
            SelectObject(memory, old);
            let mut info: BITMAPINFO = zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..zeroed()
            };
            let mut pixels = vec![0u8; (width * height * 4) as usize];
            let rows = GetDIBits(
                memory,
                bitmap,
                0,
                height as u32,
                pixels.as_mut_ptr() as *mut _,
                &mut info,
                DIB_RGB_COLORS,
            );
            DeleteObject(bitmap);
            DeleteDC(memory);
            ReleaseDC(window, dc);
            assert_ne!(rows, 0);
            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"BM");
            bytes.extend_from_slice(&(54 + pixels.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&[0u8; 4]);
            bytes.extend_from_slice(&54u32.to_le_bytes());
            bytes.extend_from_slice(std::slice::from_raw_parts(
                &info.bmiHeader as *const _ as *const u8,
                40,
            ));
            bytes.extend_from_slice(&pixels);
            std::fs::write(path, bytes).unwrap();
        }
    }
    #[test]
    fn click_hit_testing_handles_negative_monitor_coordinates() {
        let rect = RECT {
            left: -600,
            top: 100,
            right: -100,
            bottom: 500,
        };
        assert!(inside(&rect, POINT { x: -599, y: 101 }));
        assert!(!inside(&rect, POINT { x: -100, y: 300 }));
        assert!(!inside(&rect, POINT { x: -610, y: 300 }));
    }
    #[test]
    #[ignore = "Creates a native floating window; requires interactive Windows desktop"]
    fn native_popup_dismissal_and_completion_lifecycle() {
        fn pump() {
            unsafe {
                let mut msg: MSG = zeroed();
                while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        const RESULT: &str = "请帮忙检查这次部署。\n\n如果发现问题，请补充日志和复现步骤。";
        let _clipboard = crate::platform::preserve_clipboard_for_test(RESULT).unwrap();
        let mut popup = TranslationPopup::default();
        popup.loading(7, true).unwrap();
        assert!(!popup.finish(6, Ok("stale")));
        assert!(popup.finish(7, Ok(RESULT)));
        unsafe {
            let cell = &*(GetWindowLongPtrW(popup.window, GWLP_USERDATA) as *const RefCell<State>);
            let body = cell.borrow().body;
            let mut buffer = [0u16; 256];
            let length = GetWindowTextW(body, buffer.as_mut_ptr(), buffer.len() as i32);
            assert_eq!(
                String::from_utf16_lossy(&buffer[..length as usize]),
                native_lines(RESULT)
            );
        }
        snapshot(popup.window);
        unsafe {
            SendMessageW(popup.window, WM_COMMAND, COPY, 0);
        }
        let copied = arboard::Clipboard::new().unwrap().get_text().unwrap();
        assert_eq!(copied.replace("\r\n", "\n"), RESULT);
        let mut rect: RECT = unsafe { zeroed() };
        unsafe {
            GetWindowRect(popup.window, &mut rect);
        }
        mouse_down(POINT {
            x: rect.left + 10,
            y: rect.top + 10,
        });
        pump();
        assert!(popup.current());
        mouse_down(POINT {
            x: rect.right + 1,
            y: rect.top + 10,
        });
        pump();
        assert!(!popup.current());
        assert!(!popup.finish(7, Ok("must not reopen")));
        popup.loading(8, false).unwrap();
        mouse_down(POINT {
            x: rect.right + 200,
            y: rect.top + 10,
        });
        pump();
        assert!(popup.current());
        popup.close();
    }
}
