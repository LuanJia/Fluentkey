//! All injected keys are paired, and no Enter key is ever sent.
use anyhow::{bail, ensure, Context, Result};
use std::{
    mem::{size_of, zeroed},
    ptr::null_mut,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    System::{DataExchange::*, Memory::*},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

const WAKE_MESSAGE: u32 = WM_APP + 55;
#[cfg(test)]
static TEST_WAKE_COUNT: AtomicU64 = AtomicU64::new(0);
unsafe extern "system" fn dispatch_proc(
    window: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
) -> LRESULT {
    if message == WAKE_MESSAGE {
        #[cfg(test)]
        TEST_WAKE_COUNT.fetch_add(1, Ordering::SeqCst);
        crate::drain_actions();
        return 0;
    }
    DefWindowProcW(window, message, w, l)
}

// Tag all our injected keys. Copy/paste must not invalidate their own target.
const INPUT_TAG: usize = 0x464B4559;
static USER_REVISION: AtomicU64 = AtomicU64::new(0);
pub struct ActivityMonitor {
    keyboard: HHOOK,
    mouse: HHOOK,
}
impl ActivityMonitor {
    pub fn new() -> Result<Self> {
        unsafe {
            let module =
                windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
            let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_activity), module, 0);
            ensure!(!keyboard.is_null(), "无法安装键盘活动检测");
            let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_activity), module, 0);
            if mouse.is_null() {
                UnhookWindowsHookEx(keyboard);
                bail!("无法安装鼠标活动检测");
            }
            Ok(Self { keyboard, mouse })
        }
    }
}
impl Drop for ActivityMonitor {
    fn drop(&mut self) {
        unsafe {
            UnhookWindowsHookEx(self.keyboard);
            UnhookWindowsHookEx(self.mouse);
        }
    }
}
unsafe extern "system" fn keyboard_activity(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 {
        let event = &*(l as *const KBDLLHOOKSTRUCT);
        if event.dwExtraInfo != INPUT_TAG {
            USER_REVISION.fetch_add(1, Ordering::SeqCst);
        }
    }
    CallNextHookEx(null_mut(), code, w, l)
}
fn invalidating_mouse_message(message: u32) -> bool {
    matches!(
        message,
        WM_LBUTTONDOWN
            | WM_RBUTTONDOWN
            | WM_MBUTTONDOWN
            | WM_XBUTTONDOWN
            | WM_MOUSEWHEEL
            | WM_MOUSEHWHEEL
    )
}
unsafe extern "system" fn mouse_activity(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 && invalidating_mouse_message(w as u32) {
        let event = &*(l as *const MSLLHOOKSTRUCT);
        if event.dwExtraInfo != INPUT_TAG {
            USER_REVISION.fetch_add(1, Ordering::SeqCst);
        }
    }
    CallNextHookEx(null_mut(), code, w, l)
}
pub fn dispatch_window() -> Result<isize> {
    let class: Vec<u16> = "FluentKey.Dispatch\0".encode_utf16().collect();
    unsafe {
        let spec = WNDCLASSW {
            lpfnWndProc: Some(dispatch_proc),
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        RegisterClassW(&spec);
        let window = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            null_mut(),
            null_mut(),
        );
        ensure!(!window.is_null(), "无法创建后台消息窗口");
        Ok(window as isize)
    }
}
pub fn wake(window: isize) {
    unsafe {
        PostMessageW(window as HWND, WAKE_MESSAGE, 0, 0);
    }
}
pub fn show_settings() {
    let title: Vec<u16> = "FluentKey · 随手写，自然说\0".encode_utf16().collect();
    unsafe {
        let window = FindWindowW(std::ptr::null(), title.as_ptr());
        if window.is_null() {
            return;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(window, &mut pid);
        if pid == windows_sys::Win32::System::Threading::GetCurrentProcessId() {
            ShowWindow(window, SW_SHOW);
            SetForegroundWindow(window);
        }
    }
}

#[derive(Clone, Copy)]
pub struct Target {
    window: isize,
    focus: isize,
    revision: u64,
}
fn focus() -> (isize, isize) {
    unsafe {
        let window = GetForegroundWindow();
        let thread = GetWindowThreadProcessId(window, null_mut());
        let mut info: GUITHREADINFO = zeroed();
        info.cbSize = size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(thread, &mut info) == 0 {
            return (window as isize, 0);
        }
        (window as isize, info.hwndFocus as isize)
    }
}
impl Target {
    pub fn current() -> Result<Self> {
        let (window, focus) = focus();
        ensure!(window != 0 && focus != 0, "无法识别输入焦点");
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(window as HWND, &mut pid);
        }
        ensure!(
            pid != unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessId() },
            "请在其他应用的输入框中使用快捷键"
        );
        let mut class = [0u16; 64];
        unsafe {
            let len = GetClassNameW(focus as HWND, class.as_mut_ptr(), class.len() as i32);
            let name = String::from_utf16_lossy(&class[..len as usize]).to_lowercase();
            if name.contains("edit") && GetWindowLongW(focus as HWND, GWL_STYLE) & 0x20 != 0 {
                bail!("密码输入框不支持改写");
            }
        }
        Ok(Self {
            window,
            focus,
            revision: USER_REVISION.load(Ordering::SeqCst),
        })
    }
    pub fn untouched(self) -> bool {
        focus() == (self.window, self.focus)
            && USER_REVISION.load(Ordering::SeqCst) == self.revision
    }
    pub fn same_focus(self) -> bool {
        focus() == (self.window, self.focus)
    }
}
fn keys(code: u16) -> Result<()> {
    let sequence = [
        (VK_CONTROL, 0),
        (code, 0),
        (code, KEYEVENTF_KEYUP),
        (VK_CONTROL, KEYEVENTF_KEYUP),
    ];
    let inputs: Vec<INPUT> = sequence
        .into_iter()
        .map(|(vk, flags)| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: INPUT_TAG,
                },
            },
        })
        .collect();
    let count = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    };
    if count != inputs.len() as u32 {
        // Best effort release after partial injection.
        let release = [code, VK_CONTROL].map(|vk| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: INPUT_TAG,
                },
            },
        });
        unsafe {
            SendInput(2, release.as_ptr(), size_of::<INPUT>() as i32);
        }
        bail!("无法发送按键；目标可能以管理员权限运行");
    }
    Ok(())
}
fn wait_release() -> Result<()> {
    let until = Instant::now() + Duration::from_secs(2);
    while (8..=254).any(|k| unsafe { GetAsyncKeyState(k) } < 0) {
        ensure!(Instant::now() < until, "请松开快捷键后再试");
        thread::sleep(Duration::from_millis(15));
    }
    Ok(())
}

// Eagerly copy HGLOBAL formats, including Unicode, HTML, RTF, DIB and file lists.
// GDI bitmap is redundant when DIB exists; other handle-based formats fail closed.
struct Snapshot {
    formats: Vec<(u32, Vec<u8>)>,
    owner: HWND,
    owned_sequence: u32,
}
struct OpenClipboardGuard;
impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
        }
    }
}
fn open(owner: HWND) -> Result<OpenClipboardGuard> {
    for _ in 0..20 {
        if unsafe { OpenClipboard(owner) } != 0 {
            return Ok(OpenClipboardGuard);
        }
        thread::sleep(Duration::from_millis(10));
    }
    bail!("剪贴板正被其他程序占用")
}
impl Snapshot {
    fn take() -> Result<Self> {
        unsafe {
            let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
            let owner = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                null_mut(),
                null_mut(),
            );
            ensure!(!owner.is_null(), "无法创建剪贴板窗口");
            let mut snap = Self {
                formats: vec![],
                owner,
                owned_sequence: 0,
            };
            let _guard = open(owner)?;
            let mut formats = vec![];
            let mut format = EnumClipboardFormats(0);
            while format != 0 {
                formats.push(format);
                format = EnumClipboardFormats(format);
            }
            let has_dib = formats.contains(&8) || formats.contains(&17);
            for format in formats {
                if format == 2 && has_dib {
                    continue;
                }
                ensure!(
                    !matches!(format, 2 | 3 | 9 | 14 | 0x0080..=0x008e),
                    "剪贴板包含不支持备份的特殊格式，请先复制一段普通文字"
                );
                let handle = GetClipboardData(format);
                ensure!(!handle.is_null(), "无法备份剪贴板格式 {format}");
                let len = GlobalSize(handle);
                ensure!(
                    len > 0 && len <= 32 * 1024 * 1024,
                    "剪贴板格式过大或无法安全备份"
                );
                let data = GlobalLock(handle);
                ensure!(!data.is_null(), "无法读取剪贴板");
                snap.formats.push((
                    format,
                    std::slice::from_raw_parts(data as *const u8, len).to_vec(),
                ));
                GlobalUnlock(handle);
            }
            Ok(snap)
        }
    }
    fn claim(&mut self) {
        self.owned_sequence = unsafe { GetClipboardSequenceNumber() };
    }
    fn restore(&mut self) -> Result<()> {
        if self.owned_sequence == 0
            || unsafe { GetClipboardSequenceNumber() } != self.owned_sequence
        {
            return Ok(());
        }
        let _guard = open(self.owner)?;
        unsafe {
            // Allocate everything before touching the current clipboard.
            let mut blocks = Vec::new();
            for (format, bytes) in &self.formats {
                let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
                if handle.is_null() {
                    for (_, h) in blocks {
                        GlobalFree(h);
                    }
                    bail!("恢复剪贴板时内存不足");
                }
                let dest = GlobalLock(handle);
                if dest.is_null() {
                    GlobalFree(handle);
                    for (_, h) in blocks {
                        GlobalFree(h);
                    }
                    bail!("无法锁定剪贴板内存");
                }
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), dest as *mut u8, bytes.len());
                GlobalUnlock(handle);
                blocks.push((*format, handle));
            }
            if EmptyClipboard() == 0 {
                for (_, h) in blocks {
                    GlobalFree(h);
                }
                bail!("无法恢复剪贴板");
            }
            let mut failed = false;
            for (format, handle) in blocks {
                if SetClipboardData(format, handle).is_null() {
                    GlobalFree(handle);
                    failed = true;
                }
            }
            self.owned_sequence = 0;
            ensure!(!failed, "部分剪贴板格式未能恢复");
        }
        Ok(())
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = self.restore();
        unsafe {
            DestroyWindow(self.owner);
        }
    }
}
pub fn capture(target: Target, max_chars: usize) -> Result<(Target, String)> {
    wait_release()?;
    ensure!(target.same_focus(), "焦点已改变，请重试");
    let target = Target {
        revision: USER_REVISION.load(Ordering::SeqCst),
        ..target
    };
    let mut snapshot = Snapshot::take()?;
    let mut clipboard = arboard::Clipboard::new()?;
    // A unique sentinel prevents a failed Ctrl+C from sending stale clipboard text.
    let marker = format!("FluentKey-capture-{:?}", Instant::now());
    clipboard.set_text(marker.clone())?;
    snapshot.claim();
    let marker_sequence = unsafe { GetClipboardSequenceNumber() };
    ensure!(target.untouched(), "读取期间检测到输入或焦点变化，请重试");
    keys(0x41)?;
    thread::sleep(Duration::from_millis(65));
    ensure!(target.untouched(), "读取期间检测到输入或焦点变化，请重试");
    keys(0x43)?;
    let deadline = Instant::now() + Duration::from_millis(900);
    let text = loop {
        ensure!(target.untouched(), "读取期间检测到输入或焦点变化，请重试");
        if unsafe { GetClipboardSequenceNumber() } != marker_sequence {
            if let Ok(text) = clipboard.get_text() {
                if text != marker {
                    snapshot.claim();
                    break text;
                }
            }
        }
        if Instant::now() >= deadline {
            bail!("未能复制文字，请确认光标在可编辑输入框中");
        }
        thread::sleep(Duration::from_millis(20));
    };
    ensure!(target.untouched(), "读取期间检测到输入或焦点变化，已停止");
    snapshot.restore()?;
    ensure!(!text.trim().is_empty(), "输入框为空");
    ensure!(
        text.chars().count() <= max_chars,
        "文字超过字符上限，已停止发送"
    );
    Ok((
        Target {
            revision: USER_REVISION.load(Ordering::SeqCst),
            ..target
        },
        text,
    ))
}
fn ensure_target(target: Target, cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::SeqCst), "已取消自动替换");
    ensure!(
        target.untouched(),
        "检测到键盘输入、点击或焦点变化，未自动替换"
    );
    Ok(())
}
fn canonical_text(text: &str) -> String {
    text.replace("\r\n", "\n").trim_end_matches('\n').to_owned()
}
fn copy_all(
    target: Target,
    clipboard: &mut arboard::Clipboard,
    snapshot: &mut Snapshot,
    cancel: &AtomicBool,
) -> Result<String> {
    ensure_target(target, cancel)?;
    let marker = format!("FluentKey-verify-{:?}", Instant::now());
    clipboard.set_text(marker.clone())?;
    snapshot.claim();
    let marker_sequence = unsafe { GetClipboardSequenceNumber() };
    keys(0x41)?;
    thread::sleep(Duration::from_millis(80));
    ensure_target(target, cancel)?;
    keys(0x43)?;
    let deadline = Instant::now() + Duration::from_millis(1200);
    loop {
        ensure_target(target, cancel)?;
        if unsafe { GetClipboardSequenceNumber() } != marker_sequence {
            if let Ok(text) = clipboard.get_text() {
                if text != marker {
                    snapshot.claim();
                    return Ok(text);
                }
            }
        }
        ensure!(Instant::now() < deadline, "无法核对目标输入框的当前内容");
        thread::sleep(Duration::from_millis(20));
    }
}
pub fn paste(target: Target, original: &str, text: &str, cancel: &AtomicBool) -> Result<()> {
    ensure_target(target, cancel)?;
    let mut snapshot = Snapshot::take()?;
    let mut clipboard = arboard::Clipboard::new()?;
    // Electron editors can collapse their selection while the API is running.
    // Reselect, but only replace if the entire original field is unchanged.
    let current = copy_all(target, &mut clipboard, &mut snapshot, cancel)?;
    ensure!(
        canonical_text(&current) == canonical_text(original),
        "输入框原文已改变，未自动替换"
    );
    clipboard.set_text(text.to_owned())?;
    snapshot.claim();
    ensure_target(target, cancel)?;
    keys(0x56)?;
    thread::sleep(Duration::from_millis(500));
    // A successful SendInput only queues events. Read back the editor before
    // reporting success, so rejected/delayed Electron pastes aren't silent.
    let actual = copy_all(target, &mut clipboard, &mut snapshot, cancel)
        .context("已尝试粘贴，但无法确认替换结果")?;
    ensure!(
        canonical_text(&actual) == canonical_text(text),
        "未确认目标输入框已替换，请在结果面板手动复制"
    );
    ensure_target(target, cancel)?;
    // Readback selected the entire field. Collapse that selection to the end.
    let right = [0, KEYEVENTF_KEYUP].map(|flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_RIGHT,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INPUT_TAG,
            },
        },
    });
    unsafe {
        SendInput(2, right.as_ptr(), size_of::<INPUT>() as i32);
    }
    snapshot.restore().context("已发送粘贴，但剪贴板恢复失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::null;
    thread_local! { static ORIGINAL_EDIT_PROC: std::cell::Cell<isize> = const { std::cell::Cell::new(0) }; }
    unsafe extern "system" fn fixture_edit_proc(
        window: HWND,
        message: u32,
        w: WPARAM,
        l: LPARAM,
    ) -> LRESULT {
        // Classic EDIT doesn't implement Ctrl+A; real applications (including
        // Slack) provide it. Give the isolated fixture that standard shortcut.
        if message == WM_KEYDOWN && w == 0x41 && GetKeyState(VK_CONTROL as i32) < 0 {
            SendMessageW(window, windows_sys::Win32::UI::Controls::EM_SETSEL, 0, -1);
            return 0;
        }
        if message == WM_CHAR && w == 1 {
            return 0;
        }
        let original: WNDPROC = std::mem::transmute(ORIGINAL_EDIT_PROC.with(|value| value.get()));
        CallWindowProcW(original, window, message, w, l)
    }
    fn pump() {
        unsafe {
            let mut message: MSG = zeroed();
            while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    #[test]
    fn hidden_dispatch_window_receives_worker_wakes_without_ui_redraw() {
        let window = dispatch_window().unwrap();
        let before = TEST_WAKE_COUNT.load(Ordering::SeqCst);
        thread::spawn(move || wake(window)).join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while TEST_WAKE_COUNT.load(Ordering::SeqCst) == before && Instant::now() < deadline {
            pump();
            thread::sleep(Duration::from_millis(5));
        }
        unsafe {
            DestroyWindow(window as HWND);
        }
        assert!(TEST_WAKE_COUNT.load(Ordering::SeqCst) > before);
    }
    #[test]
    #[ignore = "Opens an isolated native edit fixture and injects keys; requires an interactive Windows desktop"]
    fn native_edit_capture_replace_and_rejection() {
        use windows_sys::Win32::UI::Controls::{EM_SETREADONLY, EM_SETSEL};
        let _monitor = ActivityMonitor::new().unwrap();
        let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let parent_class = wide("STATIC");
        let edit_class = wide("EDIT");
        let title = wide("FluentKey isolated input test — no network");
        let parent = unsafe {
            CreateWindowExW(
                0,
                parent_class.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                120,
                120,
                620,
                180,
                null_mut(),
                null_mut(),
                null_mut(),
                null(),
            )
        };
        assert!(!parent.is_null());
        struct Window(HWND);
        impl Drop for Window {
            fn drop(&mut self) {
                unsafe {
                    DestroyWindow(self.0);
                }
            }
        }
        let _window = Window(parent);
        let edit = unsafe {
            CreateWindowExW(
                0,
                edit_class.as_ptr(),
                wide("can you check this bug").as_ptr(),
                WS_CHILD | WS_VISIBLE | ES_MULTILINE as u32,
                10,
                10,
                570,
                100,
                parent,
                null_mut(),
                null_mut(),
                null(),
            )
        };
        assert!(!edit.is_null());
        let original = unsafe {
            SetWindowLongPtrW(edit, GWLP_WNDPROC, fixture_edit_proc as *const () as isize)
        };
        assert_ne!(original, 0);
        ORIGINAL_EDIT_PROC.with(|value| value.set(original));
        unsafe {
            SetForegroundWindow(parent);
            SetFocus(edit);
        }
        pump();
        let destination = Target {
            window: parent as isize,
            focus: edit as isize,
            revision: USER_REVISION.load(Ordering::SeqCst),
        };
        assert!(
            destination.same_focus(),
            "Test fixture couldn't obtain foreground focus; no keys were injected"
        );
        let edit_id = edit as isize;
        let worker = thread::spawn(move || -> Result<()> {
            let (target, original) = capture(destination, 12000)?;
            ensure!(
                original == "can you check this bug",
                "Unexpected fixture capture"
            );
            // A real mouse move, not a posted window message, exercises the hook.
            let movement = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 1,
                        dy: 0,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            unsafe {
                SendInput(1, &movement, size_of::<INPUT>() as i32);
            }
            thread::sleep(Duration::from_millis(100));
            ensure!(
                target.untouched(),
                "Mouse movement or our own copy keys invalidated the target"
            );
            // Simulate an Electron editor collapsing its selected range.
            unsafe {
                SendMessageW(edit_id as HWND, EM_SETSEL, usize::MAX, -1);
            }
            let cancel = AtomicBool::new(false);
            let output = "Could you check this bug?";
            paste(target, &original, output, &cancel)?;
            // Read-only editors reject Ctrl+V even when SendInput reports success.
            unsafe {
                SendMessageW(edit_id as HWND, EM_SETREADONLY, 1, 0);
            }
            let error = paste(target, output, "This must not appear.", &cancel).unwrap_err();
            ensure!(
                error.to_string().contains("未确认"),
                "Rejected paste was not reported"
            );
            unsafe {
                SendMessageW(edit_id as HWND, EM_SETREADONLY, 0, 0);
            }
            // Programmatic edits don't produce input events, so compare text too.
            let changed: Vec<u16> = "newer draft".encode_utf16().chain(Some(0)).collect();
            unsafe {
                SetWindowTextW(edit_id as HWND, changed.as_ptr());
            }
            let error = paste(target, output, "Do not overwrite.", &cancel).unwrap_err();
            ensure!(
                error.to_string().contains("原文已改变"),
                "Changed draft was not protected"
            );
            cancel.store(true, Ordering::SeqCst);
            ensure!(
                paste(target, "newer draft", "Do not paste.", &cancel).is_err(),
                "Cancelled paste was allowed"
            );
            Ok(())
        });
        while !worker.is_finished() {
            pump();
            thread::sleep(Duration::from_millis(3));
        }
        worker.join().unwrap().unwrap();
    }
    #[test]
    fn mouse_movement_does_not_invalidate_but_clicks_and_scrolls_do() {
        assert!(!invalidating_mouse_message(WM_MOUSEMOVE));
        for event in [
            WM_LBUTTONDOWN,
            WM_RBUTTONDOWN,
            WM_MBUTTONDOWN,
            WM_XBUTTONDOWN,
            WM_MOUSEWHEEL,
            WM_MOUSEHWHEEL,
        ] {
            assert!(invalidating_mouse_message(event));
        }
    }
    #[test]
    fn editor_newlines_are_normalized_without_losing_meaningful_spaces() {
        assert_eq!(canonical_text("one\r\ntwo\r\n"), canonical_text("one\ntwo"));
        assert_ne!(canonical_text("one two"), canonical_text("one  two"));
        assert_ne!(
            canonical_text("original"),
            canonical_text("original edited")
        );
    }
}
