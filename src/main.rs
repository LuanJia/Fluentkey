#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod api;
mod autostart;
mod config;
mod hotkeys;
mod platform;
mod popup;

use anyhow::{ensure, Context, Result};
use config::{credential, Config};
use eframe::egui::{self, Color32, RichText};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use hotkeys::Hotkeys;
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, OnceLock,
    },
    time::{Duration, Instant},
};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};

const MINT: Color32 = Color32::from_rgb(114, 232, 190);
enum Action {
    Hotkey(GlobalHotKeyEvent, Result<platform::Target, String>),
    Menu(MenuEvent),
    Worker,
}
thread_local! {
    static ACTIVE: RefCell<Weak<RefCell<App>>> = const { RefCell::new(Weak::new()) };
}
// Any callback thread can enqueue. Only the native dispatcher window accesses App.
static EVENTS: OnceLock<(mpsc::Sender<Action>, isize)> = OnceLock::new();
fn post_action(action: Action) {
    if let Some((sender, window)) = EVENTS.get() {
        if sender.send(action).is_ok() {
            platform::wake(*window);
        }
    }
}
fn drain_actions() {
    ACTIVE.with(|active| {
        let Some(shared) = active.borrow().upgrade() else {
            return;
        };
        let Ok(mut app) = shared.try_borrow_mut() else {
            // Reentrant Win32 callbacks must retry even while the UI is hidden.
            if let Some((_, window)) = EVENTS.get() {
                platform::wake(*window);
            }
            return;
        };
        let ctx = app.context.clone();
        while let Ok(action) = app.actions.try_recv() {
            match action {
                Action::Hotkey(event, target) => {
                    if !app.paused && !app.busy {
                        let kind = if event.id == app.hotkeys.rewrite.id() {
                            Some(JobKind::Rewrite)
                        } else if event.id == app.hotkeys.translate.id() {
                            Some(JobKind::Translate)
                        } else {
                            None
                        };
                        if let Some(kind) = kind {
                            match target {
                                Ok(target) => app.start(&ctx, kind, Some(target)),
                                Err(error) => {
                                    app.status = error.clone();
                                    let _ = app.tray.set_icon(Some(icon([255, 188, 110])));
                                    if kind == JobKind::Translate {
                                        app.show_translation_error(&error);
                                    }
                                }
                            }
                        }
                    }
                }
                Action::Menu(event) => app.menu_event(event, &ctx),
                Action::Worker => app.pump_worker(),
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobKind {
    Rewrite,
    Translate,
    Test,
}
enum WorkerEvent {
    Captured {
        kind: JobKind,
        request: u64,
    },
    Done {
        kind: JobKind,
        request: u64,
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
    hotkeys: Hotkeys,
    translation_menu: MenuItem,
    translation_result: String,
    popup: popup::TranslationPopup,
    popup_started: bool,
    request: u64,
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
    actions: mpsc::Receiver<Action>,
    _activity: platform::ActivityMonitor,
    config_notice: Option<String>,
}
impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        mut saved: Config,
        mut load_error: Option<String>,
    ) -> Result<Self> {
        let ctx = &cc.egui_ctx;
        let activity = platform::ActivityMonitor::new()?;
        // Reflect the current executable's registered state, rather than silently
        // re-enabling a startup item the user removed outside the application.
        match autostart::is_enabled() {
            Ok(enabled) => saved.start_at_login = enabled,
            Err(error) => {
                saved.start_at_login = false;
                let detail = format!("无法读取开机启动状态：{error}");
                load_error = Some(match load_error {
                    Some(previous) => format!("{previous}；{detail}"),
                    None => detail,
                });
            }
        }
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
        let hotkeys =
            Hotkeys::from_config(&saved).or_else(|_| Hotkeys::from_config(&Config::default()))?;
        let registration = hotkeys.prepare(&manager, None);
        let paused = registration.is_err();
        let menu = Menu::new();
        let settings = MenuItem::new("打开 FluentKey / 设置", true, None);
        let translation_menu = MenuItem::new("查看最近中文翻译", true, None);
        let pause = MenuItem::new("暂停 / 恢复快捷键", true, None);
        let cancel_menu = MenuItem::new("取消当前任务", true, None);
        let quit = MenuItem::new("退出 FluentKey", true, None);
        menu.append_items(&[&settings, &translation_menu, &pause, &cancel_menu, &quit])?;
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
        let config_notice = load_error.clone();
        let status = if paused {
            "快捷键注册失败（可能已被占用），请修改并保存".into()
        } else {
            load_error.unwrap_or_else(|| "准备就绪 · 在输入框中按快捷键开始".into())
        };
        let wake = platform::dispatch_window()?;
        let (sender, actions) = mpsc::channel();
        EVENTS
            .set((sender, wake))
            .map_err(|_| anyhow::anyhow!("后台事件通道已初始化"))?;
        Ok(Self {
            draft: saved.clone(),
            saved,
            key,
            show_key: false,
            manager,
            hotkeys,
            translation_menu,
            translation_result: String::new(),
            popup: popup::TranslationPopup::default(),
            popup_started: false,
            request: 0,
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
            actions,
            _activity: activity,
            config_notice,
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
        let hotkeys = Hotkeys::from_config(&self.draft)?;
        let added = hotkeys.prepare(
            &self.manager,
            if self.paused {
                None
            } else {
                Some(self.hotkeys)
            },
        )?;
        // Roll back new registration if persistence fails. Old shortcut keeps working.
        let persist = autostart::save_with(self.draft.start_at_login, || -> Result<()> {
            credential()?.set_password(self.key.trim())?;
            self.draft.save()
        });
        if let Err(error) = persist {
            for key in added {
                let _ = self.manager.unregister(key);
            }
            return Err(error);
        }
        if !self.paused {
            for key in self.hotkeys.removals(hotkeys) {
                let _ = self.manager.unregister(key);
            }
        }
        self.hotkeys = hotkeys;
        self.popup.set_auto_close(self.draft.translation_auto_close);
        self.saved = self.draft.clone();
        self.config_notice = None;
        self.paused = false;
        self.status = "已保存 · 快捷键已生效".into();
        Ok(())
    }
    fn show_translation_error(&mut self, error: &str) {
        self.request = self.request.wrapping_add(1);
        if self
            .popup
            .loading(self.request, self.saved.translation_auto_close)
            .is_ok()
        {
            self.popup.finish(self.request, Err(error));
        }
    }
    fn show_last_translation(&mut self) {
        if self.translation_result.is_empty() {
            self.status = "暂无中文译文，请先选中文字并按翻译快捷键".into();
            return;
        }
        self.request = self.request.wrapping_add(1);
        match self
            .popup
            .loading(self.request, self.saved.translation_auto_close)
        {
            Ok(()) => {
                self.popup
                    .finish(self.request, Ok(&self.translation_result));
            }
            Err(error) => self.status = error.to_string(),
        }
    }
    fn start(&mut self, ctx: &egui::Context, kind: JobKind, target: Option<platform::Target>) {
        if self.busy || self.exit_requested {
            return;
        }
        let config = if kind == JobKind::Test {
            self.draft.clone()
        } else {
            self.saved.clone()
        };
        if let Err(error) = config.validate() {
            self.status = error.to_string();
            self.show(ctx);
            return;
        }
        let key = if kind == JobKind::Test {
            self.key.clone()
        } else {
            credential()
                .and_then(|entry| Ok(entry.get_password()?))
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
        self.request = self.request.wrapping_add(1);
        let request = self.request;
        if kind == JobKind::Translate {
            self.popup.close();
            self.popup_started = false;
        }
        self.status = match kind {
            JobKind::Test => "正在测试连接…",
            JobKind::Rewrite => "正在读取输入框…",
            JobKind::Translate => "正在复制所选文字…",
        }
        .into();
        let _ = self.tray.set_icon(Some(icon([120, 176, 255])));
        let cancel = self.cancel.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let operation = || -> Result<(String, String, bool)> {
                let (destination, input) = match kind {
                    JobKind::Test => (None, "please check this connection".into()),
                    JobKind::Rewrite => {
                        let (target, text) = platform::capture(
                            target.context("未记录触发时的目标窗口")?,
                            config.max_chars,
                        )?;
                        (Some(target), text)
                    }
                    JobKind::Translate => (
                        None,
                        platform::capture_selection(
                            target.context("未记录触发时的目标窗口")?,
                            config.max_chars,
                        )?,
                    ),
                };
                ensure!(!cancel.load(Ordering::SeqCst), "已取消当前任务");
                let _ = tx.send(WorkerEvent::Captured { kind, request });
                post_action(Action::Worker);
                ctx.request_repaint();
                let text = if kind == JobKind::Translate {
                    api::translate(&config, &key, &input)?
                } else {
                    api::rewrite(&config, &key, &input)?
                };
                ensure!(!cancel.load(Ordering::SeqCst), "已取消当前任务");
                let (status, success) = match kind {
                    JobKind::Translate => (
                        "中文翻译完成 · 可在悬浮窗复制，也可从托盘重新打开".into(),
                        true,
                    ),
                    JobKind::Test => ("连接成功 · 模型已返回结果".into(), true),
                    JobKind::Rewrite if config.auto_replace => match platform::paste(
                        destination.context("未记录回填目标")?,
                        &input,
                        &text,
                        &cancel,
                    ) {
                        Ok(()) => ("已确认替换 · 如需撤销，请在原输入框按 Ctrl+Z".into(), true),
                        Err(error) => (format!("{error} · 可打开面板复制结果"), false),
                    },
                    JobKind::Rewrite => {
                        ("改写完成 · 自动替换已关闭，可打开面板复制结果".into(), true)
                    }
                };
                Ok((text, status, success))
            };
            let event = match operation() {
                Ok((text, status, success)) => WorkerEvent::Done {
                    kind,
                    request,
                    text: Some(text),
                    status,
                    success,
                },
                Err(error) => WorkerEvent::Done {
                    kind,
                    request,
                    text: None,
                    status: error.to_string(),
                    success: false,
                },
            };
            let _ = tx.send(event);
            post_action(Action::Worker);
            ctx.request_repaint();
        });
    }
    fn toggle_pause(&mut self) {
        if self.busy {
            self.status = "请先取消或等待当前任务结束".into();
            return;
        }
        let result = if self.paused {
            self.hotkeys.prepare(&self.manager, None).map(|_| ())
        } else {
            self.hotkeys.unregister(&self.manager)
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
        // Close native windows before terminating; active worker guards finished.
        unsafe {
            popup::close_active();
        }
        std::process::exit(0)
    }
    fn menu_event(&mut self, event: MenuEvent, ctx: &egui::Context) {
        if event.id == self.settings.id() {
            self.show(ctx);
        } else if event.id == self.translation_menu.id() {
            self.show_last_translation();
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
                WorkerEvent::Captured { kind, request } => {
                    if kind == JobKind::Translate {
                        self.status = "正在翻译为中文…".into();
                        self.popup_started = true;
                        if let Err(error) = self
                            .popup
                            .loading(request, self.saved.translation_auto_close)
                        {
                            self.status = error.to_string();
                        }
                    } else {
                        self.status = "正在优化英文… 请保持原输入框焦点".into();
                    }
                }
                WorkerEvent::Done {
                    kind,
                    request,
                    text,
                    status,
                    success,
                } => {
                    self.busy = false;
                    self.status = status;
                    if self.exit_requested {
                        self.exit();
                    }
                    if kind == JobKind::Translate {
                        if !self.popup_started {
                            let _ = self
                                .popup
                                .loading(request, self.saved.translation_auto_close);
                        }
                        match text {
                            Some(text) => {
                                self.popup.finish(request, Ok(&text));
                                self.translation_result = text;
                            }
                            None => {
                                self.popup.finish(request, Err(&self.status));
                            }
                        }
                    } else if let Some(text) = text {
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
                ui.vertical(|ui| { ui.label(RichText::new("FluentKey").size(25.).strong()); ui.label(RichText::new(concat!("v",env!("CARGO_PKG_VERSION")," · 随手写，自然说。")).color(Color32::GRAY)); });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { ui.label(RichText::new(if self.busy { "● WORKING" } else if self.paused { "● PAUSED" } else { "● READY" }).color(MINT).size(12.)); });
            });
            ui.add_space(8.);
            egui::Frame::group(ui.style()).inner_margin(16.).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| { ui.label(RichText::new(self.saved.hotkey.replace("Key", "")).size(20.).color(MINT)); ui.label("输入框 → 英文润色 → 原位替换"); });
                ui.horizontal(|ui| { ui.label(RichText::new(self.saved.translation_hotkey.replace("Key", "")).size(20.).color(MINT)); ui.label("选中文字 → 中文翻译 → 悬浮窗"); });
                ui.label(RichText::new("中文翻译只读取当前选区，可用于聊天记录或网页，不会替换原文。").size(12.).color(Color32::GRAY));
            });
            ui.horizontal(|ui| { for (index, label) in ["连接与快捷键", "写作规则", "英文结果", "中文译文"].into_iter().enumerate() { ui.selectable_value(&mut self.tab, index, label); } });
            ui.separator();
            if let Some(notice) = &self.config_notice {
                ui.label(RichText::new(notice).color(Color32::from_rgb(255, 188, 110)));
            }
            egui::ScrollArea::vertical().max_height((ui.available_height() - 110.).max(100.)).show(ui, |ui| {
                if self.tab == 0 {
                    ui.add_enabled_ui(!self.busy, |ui| {
                        ui.checkbox(&mut self.draft.start_at_login, "开机启动（登录 Windows 后在托盘运行）");
                        ui.label(RichText::new("勾选或取消后点击“保存设置”生效；请将程序放在固定目录。").size(12.).color(Color32::GRAY));
                        ui.separator();
                        field(ui, "Base URL", &mut self.draft.base_url, "https://your-provider.com/v1");
                        ui.label(RichText::new("自动追加 /chat/completions，也支持填写完整端点。").size(12.).color(Color32::GRAY));
                        ui.label("API Key");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.key).password(!self.show_key).desired_width(475.));
                            ui.checkbox(&mut self.show_key, "显示");
                        });
                        ui.label(RichText::new("Key 保存在 Windows 凭据管理器，不写入配置文件。").size(12.).color(Color32::GRAY));
                        field(ui, "Model · 填写供应商提供的模型 ID", &mut self.draft.model, "例如你的模型 ID");
                        field(ui, "英文润色快捷键", &mut self.draft.hotkey, "Alt+KeyF");
                        field(ui, "中文翻译快捷键", &mut self.draft.translation_hotkey, "Alt+KeyD");
                        ui.checkbox(&mut self.draft.translation_auto_close, "点击悬浮窗外的其他位置时自动关闭");
                        ui.label(RichText::new("示例：Alt+KeyF、Control+Shift+KeyE。冲突时保存会提示。").size(12.).color(Color32::GRAY));
                        ui.horizontal(|ui| {
                            ui.label("超时 / 秒"); ui.add(egui::DragValue::new(&mut self.draft.timeout_secs).range(5..=120));
                            ui.label("字符上限"); ui.add(egui::DragValue::new(&mut self.draft.max_chars).range(1..=50000));
                        });
                        ui.checkbox(&mut self.draft.auto_replace, "英文润色完成后自动替换输入框中的文字");
                    });
                    ui.add_space(4.);
                    ui.label(RichText::new("英文润色发送整个输入框；中文翻译仅发送当前所选文字。两者都使用已配置的 API，请避免选中敏感信息。").size(12.).color(Color32::from_rgb(224, 185, 128)));
                } else if self.tab == 1 {
                    ui.label("保持原意 · 美式口语 · IT 行业表达 · 只输出译文");
                    ui.add_enabled(!self.busy, egui::TextEdit::multiline(&mut self.draft.prompt).desired_width(f32::INFINITY).desired_rows(14));
                    if ui.add_enabled(!self.busy, egui::Button::new("恢复默认规则")).clicked() { self.draft.prompt = config::PROMPT.into(); }
                    ui.separator();
                    ui.label("选中文字 → 简体中文翻译规则");
                    ui.add_enabled(!self.busy,egui::TextEdit::multiline(&mut self.draft.translation_prompt).desired_width(f32::INFINITY).desired_rows(8));
                    if ui.add_enabled(!self.busy,egui::Button::new("恢复中文翻译规则")).clicked() { self.draft.translation_prompt = config::TRANSLATION_PROMPT.into(); }
                } else if self.tab == 2 {
                    ui.label("仅保留本次运行中的最近一个结果，退出即清除。");
                    ui.add(egui::TextEdit::multiline(&mut self.result).desired_width(f32::INFINITY).desired_rows(13));
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!self.result.is_empty(), egui::Button::new("复制结果")).clicked() {
                            match arboard::Clipboard::new().and_then(|mut c| c.set_text(self.result.clone())) { Ok(()) => self.status = "已复制结果".into(), Err(_) => self.status = "复制失败，请重试".into() }
                        }
                        if ui.button("清除结果").clicked() { self.result.clear(); }
                    });
                } else {
                    ui.label("最近一次中文译文 · 仅存本次运行内存");
                    ui.add(egui::TextEdit::multiline(&mut self.translation_result).desired_width(f32::INFINITY).desired_rows(12));
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!self.translation_result.is_empty(),egui::Button::new("复制中文译文")).clicked() {
                            self.status = match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(self.translation_result.clone())) { Ok(()) => "已复制中文译文".into(),Err(_) => "复制失败，请重试".into() };
                        }
                        if ui.add_enabled(!self.busy && !self.translation_result.is_empty(),egui::Button::new("打开悬浮窗")).clicked() { self.show_last_translation(); }
                    });
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.busy, egui::Button::new(RichText::new("保存设置").color(Color32::from_rgb(17, 24, 32))).fill(MINT)).clicked() {
                    if let Err(e) = self.save() { self.status = e.to_string(); }
                }
                if ui.add_enabled(!self.busy, egui::Button::new("测试连接")).clicked() { self.start(ctx, JobKind::Test, None); }
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
    let (saved, error) = match Config::load_with_notice() {
        Ok(loaded) => loaded,
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
                    // Snapshot the destination before any foreground UI changes.
                    let target = platform::Target::current().map_err(|error| error.to_string());
                    post_action(Action::Hotkey(event, target));
                }
            }));
            MenuEvent::set_event_handler(Some(|event| post_action(Action::Menu(event))));
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
