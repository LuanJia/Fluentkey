#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod api;
mod config;
mod platform;

use anyhow::{ensure, Context, Result};
use config::{credential, Config};
use eframe::egui::{self, Color32, RichText};
use global_hotkey::{hotkey::HotKey, GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::{
    cell::RefCell,
    collections::VecDeque,
    rc::{Rc, Weak},
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};

const MINT: Color32 = Color32::from_rgb(114, 232, 190);
enum Action {
    Hotkey(GlobalHotKeyEvent),
    Menu(MenuEvent),
    Worker,
}
thread_local! {
    static ACTIVE: RefCell<Weak<RefCell<App>>> = const { RefCell::new(Weak::new()) };
    static PENDING: RefCell<VecDeque<Action>> = const { RefCell::new(VecDeque::new()) };
}
// Native callbacks execute on the UI thread; only the released-key callback is
// delivered from a helper thread (and is ignored before dispatch).
fn dispatch(action: Action) {
    PENDING.with(|queue| queue.borrow_mut().push_back(action));
    ACTIVE.with(|active| {
        let Some(shared) = active.borrow().upgrade() else {
            return;
        };
        let Ok(mut app) = shared.try_borrow_mut() else {
            return;
        };
        let ctx = app.context.clone();
        loop {
            let action = PENDING.with(|queue| queue.borrow_mut().pop_front());
            match action {
                Some(Action::Hotkey(event)) => {
                    if event.id == app.hotkey.id() && !app.paused {
                        app.start(&ctx, false);
                    }
                }
                Some(Action::Menu(event)) => app.menu_event(event, &ctx),
                Some(Action::Worker) => app.pump_worker(),
                None => break,
            }
        }
        let _ = app
            .tray
            .set_tooltip(Some(&format!("FluentKey · {}", app.status)));
        ctx.request_repaint();
    });
}
struct Shell(Rc<RefCell<App>>);
impl eframe::App for Shell {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.0.borrow_mut().update(ctx, frame);
        // Drain a callback deferred by try_borrow_mut during a reentrant Win32 call.
        if PENDING.with(|queue| !queue.borrow().is_empty()) {
            platform::wake(self.0.borrow().wake);
        }
    }
}
fn icon(color: [u8; 3]) -> Icon {
    let mut rgba = vec![0; 32 * 32 * 4];
    for y in 0..32 {
        for x in 0..32 {
            let i = (y * 32 + x) * 4;
            let inside = (4..28).contains(&x) && (3..29).contains(&y);
            let f = (9..13).contains(&x) && (8..25).contains(&y)
                || (9..23).contains(&x) && (8..12).contains(&y)
                || (9..20).contains(&x) && (15..19).contains(&y);
            let c = if f { [17, 24, 32] } else { color };
            rgba[i..i + 3].copy_from_slice(&c);
            rgba[i + 3] = if inside { 255 } else { 0 };
        }
    }
    Icon::from_rgba(rgba, 32, 32).unwrap()
}
fn parse_hotkey(text: &str) -> Result<HotKey> {
    let hotkey =
        HotKey::from_str(text).context("快捷键格式示例：Alt+KeyF 或 Control+Shift+KeyE")?;
    ensure!(
        !hotkey.mods.is_empty(),
        "快捷键至少需要 Alt、Control 或 Shift 修饰键"
    );
    Ok(hotkey)
}
enum WorkerEvent {
    Captured,
    Done {
        text: Option<String>,
        status: String,
        success: bool,
    },
}
struct App {
    saved: Config,
    draft: Config,
    key: String,
    show_key: bool,
    manager: GlobalHotKeyManager,
    hotkey: HotKey,
    tray: TrayIcon,
    settings: MenuItem,
    pause: MenuItem,
    cancel_menu: MenuItem,
    quit: MenuItem,
    paused: bool,
    busy: bool,
    visible: bool,
    status: String,
    result: String,
    tx: mpsc::Sender<WorkerEvent>,
    rx: mpsc::Receiver<WorkerEvent>,
    cancel: Arc<AtomicBool>,
    started: Instant,
    tab: usize,
    exit_requested: bool,
    context: egui::Context,
    wake: isize,
}
impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        saved: Config,
        load_error: Option<String>,
    ) -> Result<Self> {
        let ctx = &cc.egui_ctx;
        ctx.set_theme(egui::Theme::Dark);
        ctx.set_visuals(egui::Visuals::dark());
        let mut style = (*ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(12., 12.);
        style.spacing.button_padding = egui::vec2(14., 8.);
        style.spacing.interact_size.y = 32.;
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(14.));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(14.));
        style.visuals.panel_fill = Color32::from_rgb(17, 24, 32);
        style.visuals.extreme_bg_color = Color32::from_rgb(13, 20, 28);
        style.visuals.widgets.inactive.rounding = egui::Rounding::same(6.);
        style.visuals.widgets.hovered.rounding = egui::Rounding::same(6.);
        style.visuals.selection.bg_fill = Color32::from_rgb(42, 103, 88);
        style.visuals.widgets.active.bg_fill = Color32::from_rgb(42, 103, 88);
        ctx.set_style(style);
        if let Ok(bytes) = std::fs::read("C:\\Windows\\Fonts\\msyh.ttc") {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("Chinese".into(), egui::FontData::from_owned(bytes));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "Chinese".into());
            ctx.set_fonts(fonts);
        }
        let manager = GlobalHotKeyManager::new()?;
        let hotkey = parse_hotkey(&saved.hotkey).or_else(|_| parse_hotkey("Alt+KeyF"))?;
        let registration = manager.register(hotkey);
        let paused = registration.is_err();
        let menu = Menu::new();
        let settings = MenuItem::new("打开 FluentKey / 设置", true, None);
        let pause = MenuItem::new("暂停 / 恢复快捷键", true, None);
        let cancel_menu = MenuItem::new("取消当前改写", true, None);
        let quit = MenuItem::new("退出 FluentKey", true, None);
        menu.append_items(&[&settings, &pause, &cancel_menu, &quit])?;
        let tray = TrayIconBuilder::new()
            .with_tooltip("FluentKey · 随手写，自然说")
            .with_icon(icon([114, 232, 190]))
            .with_menu(Box::new(menu))
            .build()?;
        let (tx, rx) = mpsc::channel();
        let key = credential()
            .and_then(|e| Ok(e.get_password()?))
            .unwrap_or_default();
        let visible = !config::config_path().exists() || load_error.is_some() || paused;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(visible));
        let status = if paused {
            "快捷键注册失败（可能已被占用），请修改并保存".into()
        } else {
            load_error.unwrap_or_else(|| "准备就绪 · 在输入框中按快捷键开始".into())
        };
        Ok(Self {
            draft: saved.clone(),
            saved,
            key,
            show_key: false,
            manager,
            hotkey,
            tray,
            settings,
            pause,
            cancel_menu,
            quit,
            paused,
            busy: false,
            visible,
            status,
            result: String::new(),
            tx,
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
            tab: 0,
            exit_requested: false,
            context: ctx.clone(),
            wake: platform::dispatch_window()?,
        })
    }
    fn show(&mut self, ctx: &egui::Context) {
        self.visible = true;
        platform::show_settings();
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }
    fn save(&mut self) -> Result<()> {
        self.draft.validate()?;
        ensure!(!self.key.trim().is_empty(), "请填写 API Key");
        let hotkey = parse_hotkey(&self.draft.hotkey)?;
        let changed = hotkey != self.hotkey;
        if changed || self.paused {
            self.manager
                .register(hotkey)
                .context("快捷键已被占用，请换一个组合")?;
        }
        // Roll back new registration if persistence fails. Old shortcut keeps working.
        let persist = (|| -> Result<()> {
            credential()?.set_password(self.key.trim())?;
            self.draft.save()
        })();
        if let Err(error) = persist {
            if changed || self.paused {
                let _ = self.manager.unregister(hotkey);
            }
            return Err(error);
        }
        if changed && !self.paused {
            let _ = self.manager.unregister(self.hotkey);
        }
        self.hotkey = hotkey;
        self.saved = self.draft.clone();
        self.paused = false;
        self.status = "已保存 · 快捷键已生效".into();
        Ok(())
    }
    fn start(&mut self, ctx: &egui::Context, test: bool) {
        if self.busy || self.exit_requested {
            return;
        }
        let config = if test {
            self.draft.clone()
        } else {
            self.saved.clone()
        };
        if let Err(e) = config.validate() {
            self.status = e.to_string();
            self.show(ctx);
            return;
        }
        let key = if test {
            self.key.clone()
        } else {
            credential()
                .and_then(|e| Ok(e.get_password()?))
                .unwrap_or_default()
        };
        if key.trim().is_empty() {
            self.status = "请填写 API Key 并保存".into();
            self.show(ctx);
            return;
        }
        self.busy = true;
        self.started = Instant::now();
        self.cancel.store(false, Ordering::SeqCst);
        self.status = if test {
            "正在测试连接…"
        } else {
            "正在读取输入框…"
        }
        .into();
        let _ = self.tray.set_icon(Some(icon([120, 176, 255])));
        let cancel = self.cancel.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let wake = self.wake;
        std::thread::spawn(move || {
            let operation = || -> Result<(String, String)> {
                let (target, input) = if test {
                    (None, "please check this connection".into())
                } else {
                    let (target, text) = platform::capture(config.max_chars)?;
                    (Some(target), text)
                };
                ensure!(!cancel.load(Ordering::SeqCst), "已取消，原文未替换");
                let _ = tx.send(WorkerEvent::Captured);
                platform::wake(wake);
                ctx.request_repaint();
                let text = api::rewrite(&config, &key, &input)?;
                ensure!(!cancel.load(Ordering::SeqCst), "已取消，原文未替换");
                let status = if let Some(target) = target {
                    if config.auto_replace {
                        match platform::paste(target, &text) {
                            Ok(()) => "已发送替换 · 如需撤销，请在原输入框按 Ctrl+Z".into(),
                            Err(e) => format!("{} · 可打开面板复制结果", e),
                        }
                    } else {
                        "改写完成 · 打开面板复制结果".into()
                    }
                } else {
                    "连接成功 · 模型已返回结果".into()
                };
                Ok((text, status))
            };
            let event = match operation() {
                Ok((text, status)) => WorkerEvent::Done {
                    text: Some(text),
                    status,
                    success: true,
                },
                Err(e) => WorkerEvent::Done {
                    text: None,
                    status: e.to_string(),
                    success: false,
                },
            };
            let _ = tx.send(event);
            platform::wake(wake);
            ctx.request_repaint();
        });
    }
    fn toggle_pause(&mut self) {
        if self.busy {
            self.status = "请先取消或等待当前任务结束".into();
            return;
        }
        let result = if self.paused {
            self.manager.register(self.hotkey)
        } else {
            self.manager.unregister(self.hotkey)
        };
        match result {
            Ok(()) => {
                self.paused = !self.paused;
                self.status = if self.paused {
                    "已暂停快捷键"
                } else {
                    "准备就绪"
                }
                .into();
                let _ = self.tray.set_icon(Some(icon(if self.paused {
                    [139, 154, 170]
                } else {
                    [114, 232, 190]
                })));
            }
            Err(e) => self.status = format!("快捷键操作失败：{e}"),
        }
    }
    fn exit(&self) -> ! {
        // No worker is active here, so clipboard guards have finished restoring.
        let _ = self.tray.set_visible(false);
        std::process::exit(0)
    }
    fn menu_event(&mut self, event: MenuEvent, ctx: &egui::Context) {
        if event.id == self.settings.id() {
            self.show(ctx);
        } else if event.id == self.pause.id() {
            self.toggle_pause();
        } else if event.id == self.cancel_menu.id() {
            self.cancel.store(true, Ordering::SeqCst);
            self.status = "已请求取消，等待当前操作结束…".into();
        } else if event.id == self.quit.id() {
            self.cancel.store(true, Ordering::SeqCst);
            self.exit_requested = true;
            self.status = "正在退出，等待当前操作安全收尾…".into();
            if !self.busy {
                self.exit();
            }
        }
    }
    fn pump_worker(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                WorkerEvent::Captured => self.status = "正在优化英文… 请暂时不要操作键鼠".into(),
                WorkerEvent::Done {
                    text,
                    status,
                    success,
                } => {
                    self.busy = false;
                    self.status = status;
                    if self.exit_requested {
                        self.exit();
                    }
                    if let Some(text) = text {
                        self.result = text;
                    }
                    let _ = self.tray.set_icon(Some(icon(if success {
                        [114, 232, 190]
                    } else {
                        [255, 188, 110]
                    })));
                }
            }
        }
    }
}
impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(100));
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.visible = false;
        }
        self.pump_worker();
        let _ = self
            .tray
            .set_tooltip(Some(&format!("FluentKey · {}", self.status)));
        if !self.visible {
            return;
        }
        egui::CentralPanel::default().frame(egui::Frame::central_panel(&ctx.style()).inner_margin(24.)).show(ctx, |ui| {
            ui.add_space(8.);
            ui.horizontal(|ui| {
                ui.label(RichText::new("F").size(34.).color(MINT).strong());
                ui.vertical(|ui| { ui.label(RichText::new("FluentKey").size(25.).strong()); ui.label(RichText::new("随手写，自然说。").color(Color32::GRAY)); });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { ui.label(RichText::new(if self.busy { "● WORKING" } else if self.paused { "● PAUSED" } else { "● READY" }).color(MINT).size(12.)); });
            });
            ui.add_space(8.);
            egui::Frame::group(ui.style()).inner_margin(16.).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| { ui.label(RichText::new(self.saved.hotkey.replace("Key", "")).size(20.).color(MINT)); ui.label("选中文字 → 优化英文 → 原位替换"); });
                ui.label(RichText::new("光标放在输入框中即可。请求期间继续输入会停止自动回填。").size(12.).color(Color32::GRAY));
            });
            ui.horizontal(|ui| { for (index, label) in ["连接与快捷键", "写作规则", "最近结果"].into_iter().enumerate() { ui.selectable_value(&mut self.tab, index, label); } });
            ui.separator();
            egui::ScrollArea::vertical().max_height((ui.available_height() - 110.).max(100.)).show(ui, |ui| {
                if self.tab == 0 {
                    ui.add_enabled_ui(!self.busy, |ui| {
                        field(ui, "Base URL", &mut self.draft.base_url, "https://your-provider.com/v1");
                        ui.label(RichText::new("自动追加 /chat/completions，也支持填写完整端点。").size(12.).color(Color32::GRAY));
                        ui.label("API Key");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.key).password(!self.show_key).desired_width(475.));
                            ui.checkbox(&mut self.show_key, "显示");
                        });
                        ui.label(RichText::new("Key 保存在 Windows 凭据管理器，不写入配置文件。").size(12.).color(Color32::GRAY));
                        field(ui, "Model · 填写供应商提供的模型 ID", &mut self.draft.model, "例如你的模型 ID");
                        field(ui, "全局快捷键", &mut self.draft.hotkey, "Alt+KeyF");
                        ui.label(RichText::new("示例：Alt+KeyF、Control+Shift+KeyE。冲突时保存会提示。").size(12.).color(Color32::GRAY));
                        ui.horizontal(|ui| {
                            ui.label("超时 / 秒"); ui.add(egui::DragValue::new(&mut self.draft.timeout_secs).range(5..=120));
                            ui.label("字符上限"); ui.add(egui::DragValue::new(&mut self.draft.max_chars).range(1..=50000));
                        });
                        ui.checkbox(&mut self.draft.auto_replace, "完成后自动替换输入框中的文字");
                    });
                    ui.add_space(4.);
                    ui.label(RichText::new("按快捷键会将整个输入框的文字发送给所配置的 API。请在非敏感文本框中使用；浏览器密码框和特殊编辑器无法保证识别。").size(12.).color(Color32::from_rgb(224, 185, 128)));
                } else if self.tab == 1 {
                    ui.label("保持原意 · 美式口语 · IT 行业表达 · 只输出译文");
                    ui.add_enabled(!self.busy, egui::TextEdit::multiline(&mut self.draft.prompt).desired_width(f32::INFINITY).desired_rows(14));
                    if ui.add_enabled(!self.busy, egui::Button::new("恢复默认规则")).clicked() { self.draft.prompt = config::PROMPT.into(); }
                    ui.label("已去掉对话初始化时的“收到”要求，每次请求都独立翻译。");
                } else {
                    ui.label("仅保留本次运行中的最近一个结果，退出即清除。");
                    ui.add(egui::TextEdit::multiline(&mut self.result).desired_width(f32::INFINITY).desired_rows(13));
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!self.result.is_empty(), egui::Button::new("复制结果")).clicked() {
                            match arboard::Clipboard::new().and_then(|mut c| c.set_text(self.result.clone())) { Ok(()) => self.status = "已复制结果".into(), Err(_) => self.status = "复制失败，请重试".into() }
                        }
                        if ui.button("清除结果").clicked() { self.result.clear(); }
                    });
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.busy, egui::Button::new(RichText::new("保存设置").color(Color32::from_rgb(17, 24, 32))).fill(MINT)).clicked() {
                    if let Err(e) = self.save() { self.status = e.to_string(); }
                }
                if ui.add_enabled(!self.busy, egui::Button::new("测试连接")).clicked() { self.start(ctx, true); }
                if self.busy && ui.button("取消").clicked() { self.cancel.store(true, Ordering::SeqCst); self.status = "已请求取消…".into(); }
                if ui.button("收起到托盘").clicked() { self.visible = false; ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false)); }
            });
            ui.horizontal(|ui| {
                if self.busy { ui.spinner(); ui.label(format!("{}s", self.started.elapsed().as_secs())); }
                ui.label(RichText::new(&self.status).size(12.).color(Color32::LIGHT_GRAY));
            });
        });
    }
}
fn field(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) {
    ui.label(label);
    ui.add(
        egui::TextEdit::singleline(value)
            .desired_width(f32::INFINITY)
            .hint_text(hint),
    );
}
fn main() -> eframe::Result<()> {
    // Named mutex avoids duplicate tray icons and shortcut ownership surprises.
    let name: Vec<u16> = "Local\\FluentKey.SingleInstance\0".encode_utf16().collect();
    let mutex = unsafe {
        windows_sys::Win32::System::Threading::CreateMutexW(std::ptr::null(), 0, name.as_ptr())
    };
    if mutex.is_null() {
        return Ok(());
    }
    if unsafe { windows_sys::Win32::Foundation::GetLastError() } == 183 {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(mutex);
        }
        return Ok(());
    }
    let (saved, error) = match Config::load() {
        Ok(c) => (c, None),
        Err(e) => (
            Config::default(),
            Some(format!("配置读取失败，请重新设置：{e}")),
        ),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FluentKey · 随手写，自然说")
            .with_inner_size([650., 780.])
            .with_min_inner_size([610., 650.])
            .with_icon(Arc::new(egui::IconData {
                rgba: icon_rgba(),
                width: 32,
                height: 32,
            })),
        ..Default::default()
    };
    let result = eframe::run_native(
        "FluentKey",
        options,
        Box::new(move |cc| {
            let app = Rc::new(RefCell::new(App::new(cc, saved, error)?));
            ACTIVE.with(|active| *active.borrow_mut() = Rc::downgrade(&app));
            GlobalHotKeyEvent::set_event_handler(Some(|event: GlobalHotKeyEvent| {
                if event.state == HotKeyState::Pressed {
                    dispatch(Action::Hotkey(event));
                }
            }));
            MenuEvent::set_event_handler(Some(|event| dispatch(Action::Menu(event))));
            Ok(Box::new(Shell(app)) as Box<dyn eframe::App>)
        }),
    );
    unsafe {
        windows_sys::Win32::Foundation::CloseHandle(mutex);
    }
    if let Err(error) = &result {
        let title: Vec<u16> = "FluentKey 启动失败\0".encode_utf16().collect();
        let message: Vec<u16> = format!("{error}\n\n请检查桌面会话和系统托盘是否可用。\0")
            .encode_utf16()
            .collect();
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
                std::ptr::null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
            );
        }
    }
    result
}
fn icon_rgba() -> Vec<u8> {
    let mut rgba = vec![0u8; 32 * 32 * 4];
    for y in 3..29 {
        for x in 4..28 {
            let i = (y * 32 + x) * 4;
            let f = (9..13).contains(&x) && (8..25).contains(&y)
                || (9..23).contains(&x) && (8..12).contains(&y)
                || (9..20).contains(&x) && (15..19).contains(&y);
            rgba[i..i + 4].copy_from_slice(if f {
                &[17, 24, 32, 255]
            } else {
                &[114, 232, 190, 255]
            });
        }
    }
    rgba
}
