//! All injected keys are paired, and no Enter key is ever sent.
use anyhow::{bail, ensure, Context, Result};
use std::{
    mem::{size_of, zeroed},
    ptr::null_mut,
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    System::{DataExchange::*, Memory::*},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

const WAKE_MESSAGE: u32 = WM_APP + 55;
unsafe extern "system" fn dispatch_proc(
    window: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
) -> LRESULT {
    if message == WAKE_MESSAGE {
        crate::dispatch(crate::Action::Worker);
        return 0;
    }
    DefWindowProcW(window, message, w, l)
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
    input_tick: u32,
}
fn input_tick() -> u32 {
    let mut info = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    unsafe {
        GetLastInputInfo(&mut info);
    }
    info.dwTime
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
            input_tick: input_tick(),
        })
    }
    pub fn untouched(self) -> bool {
        focus() == (self.window, self.focus) && input_tick() == self.input_tick
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
                    dwExtraInfo: 0,
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
                    dwExtraInfo: 0,
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
pub fn capture(max_chars: usize) -> Result<(Target, String)> {
    let target = Target::current()?;
    wait_release()?;
    ensure!(target.same_focus(), "焦点已改变，请重试");
    let mut snapshot = Snapshot::take()?;
    let mut clipboard = arboard::Clipboard::new()?;
    // A unique sentinel prevents a failed Ctrl+C from sending stale clipboard text.
    let marker = format!("FluentKey-capture-{:?}", Instant::now());
    clipboard.set_text(marker.clone())?;
    snapshot.claim();
    ensure!(target.same_focus(), "焦点已改变，请重试");
    keys(0x41)?;
    thread::sleep(Duration::from_millis(65));
    ensure!(target.same_focus(), "焦点已改变，请重试");
    keys(0x43)?;
    let deadline = Instant::now() + Duration::from_millis(900);
    let text = loop {
        if let Ok(text) = clipboard.get_text() {
            if text != marker {
                snapshot.claim();
                break text;
            }
        }
        if Instant::now() >= deadline {
            bail!("未能复制文字，请确认光标在可编辑输入框中");
        }
        thread::sleep(Duration::from_millis(20));
    };
    ensure!(target.same_focus(), "焦点已改变，已停止");
    snapshot.restore()?;
    ensure!(!text.trim().is_empty(), "输入框为空");
    ensure!(
        text.chars().count() <= max_chars,
        "文字超过字符上限，已停止发送"
    );
    Ok((
        Target {
            input_tick: input_tick(),
            ..target
        },
        text,
    ))
}
pub fn paste(target: Target, text: &str) -> Result<()> {
    ensure!(
        target.untouched(),
        "检测到输入或切换窗口，结果已保留在面板中"
    );
    let mut snapshot = Snapshot::take()?;
    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_text(text.to_owned())?;
    snapshot.claim();
    ensure!(
        target.untouched(),
        "检测到输入或切换窗口，结果已保留在面板中"
    );
    // Original selection is left intact during the network request. Never reselect a changed field.
    keys(0x56)?;
    thread::sleep(Duration::from_millis(350));
    snapshot.restore().context("已发送粘贴，但剪贴板恢复失败")?;
    Ok(())
}
