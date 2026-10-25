# 技术架构

## 模块

| 文件 | 职责 |
|---|---|
| `src/main.rs` | eframe/egui 设置窗口、tray-icon 菜单、global-hotkey 注册、单实例互斥、后台任务状态 |
| `src/platform.rs` | Win32 焦点、密码编辑框检查、输入注入、用户操作检测、剪贴板事务 |
| `src/config.rs` | 非敏感配置、默认提示词、URL 校验、系统凭据入口 |
| `src/autostart.rs` | 当前用户登录启动项、带引号的绝对路径、保存失败回滚 |
| `src/api.rs` | OpenAI 兼容 Chat Completions、超时、响应限制、错误归一化 |

主线程驱动 Win32 消息循环、托盘和界面。全局快捷键与托盘菜单使用原生事件回调，直接通过主线程的控制器分派。按快捷键后将当前配置复制给后台线程；后台执行捕获 → 网络请求 → 条件回填，通过通道传回结果，并向独立的 message-only 窗口 PostMessage 唤醒主线程。最多一个后台任务，网络不会阻塞 UI。

控制器由主线程 Rc/RefCell 持有，线程局部 Weak 引用供原生回调定位；不跨线程共享托盘或快捷键管理器。回调如在 UI 借用期间重入，会排队并在借用结束后通过消息唤醒重试。工作线程仅持有配置快照、取消标记、结果通道和唤醒 HWND 的整数标识。设置窗口隐藏时无须绘制，也能响应快捷键、托盘和任务完成事件；这一路径不依赖隐藏窗口的 redraw。

## 文本事务

1. 记录 foreground HWND 与 focused HWND，拒绝标准 Edit 类的密码样式。
2. 等待快捷键释放，确认焦点仍在原位置。
3. 创建 message-only 窗口作为剪贴板所有者，打开剪贴板，**提前实体化复制**各 HGLOBAL 格式；不依赖失效的延迟渲染对象。
4. 写入独特哨兵，注入 Ctrl+A、Ctrl+C，轮询文字与哨兵的差异；超过 900ms 则失败，不读取旧剪贴板当输入。
5. 恢复备份；拒绝空白输入、超长输入。网络请求期间保留原输入框的选区。
6. 用 `GetLastInputInfo` 记录捕获后的活动时间及 HWND，API 返回后检查两者。检测到键鼠输入或焦点变化则只保留结果。
7. 再次备份此刻剪贴板，写入输出，检查目标仍未变化，注入 Ctrl+V。等待目标消费剪贴板后恢复。

每个输入组合由一次 SendInput 调用发送，包含完整按下／释放序列；部分失败时尝试释放 Ctrl 和字母键。无 Enter、无自动提交、无强制激活目标窗口。`SendInput` 成功只代表事件进入输入队列，无法证明目标已执行替换，因此界面使用“已发送替换”的表述。

剪贴板备份通过 RAII 收尾，显式恢复失败会报告错误；若全局序列号已变，认为其他程序或用户更新了剪贴板，不覆盖该新内容。复制期间第三方抢占、应用异步粘贴超过等待时长等场景仍可能失败。支持 Unicode/HTML/RTF/DIB/文件列表等 HGLOBAL 格式，存在 DIB 时跳过其冗余 GDI bitmap 句柄；未知或无法锁定的格式在操作前拒绝，特殊 GDI、palette、metafile 不提供通用序列化。

`GetLastInputInfo` 是保守保护：移动鼠标也可能停止回填；不检测无键鼠输入的程序化修改，也不能消除最后检查与实际注入之间的竞态。浏览器焦点 HWND 常是渲染容器，不能等同 DOM 输入框。后续增强需要 UI Automation 或应用专用集成。

## API 合约

```
POST {base_url}/chat/completions
Authorization: Bearer <Windows Credential Manager 中的 Key>
Content-Type: application/json

{"model":"...","stream":false,"messages":[
  {"role":"system","content":"可编辑的改写规则"},
  {"role":"user","content":"输入框原文"}
]}
```

接受 `choices[0].message.content` 字符串。空内容、缺少结果、非标准 JSON、`finish_reason=length/content_filter` 均拒绝替换。响应上限 1 MiB；连接超时 10s，总请求超时默认 30s，可设 5–120s。不流式写入目标，不自动重试，不跟随重定向，避免 Key 跟随到意外目标。仅远程 HTTPS／本机 HTTP。

## 配置与生命周期

配置先写临时文件再 rename；加载损坏时回退默认并显示设置，原损坏文件在用户保存前保留。Key 由 keyring windows-native 后端存储，不参与序列化。快捷键修改先注册新组合，再持久化，成功后解除旧组合；失败撤销新注册。Key 存储和 JSON 存储不具有跨系统原子事务：极少数 JSON 写入失败场景，Key 可能已经更新，状态会明确报告保存失败。

命名为 `Local\FluentKey.SingleInstance` 的 mutex 防止同一登录会话重复启动。第二次启动直接退出，不会唤醒第一个窗口；从托盘打开设置。关闭设置隐藏窗口；退出发生在后台任务完成收尾之后。未注册系统服务。

`start_at_login` 默认 false，旧 JSON 通过 serde 默认值兼容。开机启动在保存设置时写入或删除 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 的 `FluentKey` 值（REG_SZ）。命令将当前 EXE 绝对路径完整加引号，以 UTF-16 保存，支持中文和空格路径，超过 Run 命令的 260 字符限制则拒绝。用户登录后由 Windows 启动；已有配置时按原生命周期隐藏到托盘。

打开程序时根据当前可执行路径的实际注册值初始化开关，避免旧配置文件重新启用用户移除的启动项。Windows 任务管理器的 StartupApproved 禁用状态是另一个系统级控制，本实现不会覆盖它。启动项修改与凭据、JSON 保存组合执行：先快照原注册值，修改启动项；后续持久化失败则恢复原类型及内容，恢复失败会显示明确错误。只写 FluentKey 自身命名值，不写 HKLM 或要求提权。自动测试使用隔离的测试子键，不触碰真实 Run 键。

## 参考接口

- [Microsoft SendInput](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput)：输入注入受 UIPI 完整性级别约束。
- [Microsoft GetLastInputInfo](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getlastinputinfo)：当前会话最近输入信息。
- [eframe run_native](https://docs.rs/eframe/0.29.1/eframe/fn.run_native.html)
- [global-hotkey](https://docs.rs/global-hotkey/0.6.4/global_hotkey/)
- [tray-icon](https://docs.rs/tray-icon/0.19.3/tray_icon/)

依赖版本通过 Cargo.lock 固定；升级后应重新执行自动检查和 Windows 手动验收。
