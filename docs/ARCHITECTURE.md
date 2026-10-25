# 技术架构

## 模块

| 文件 | 职责 |
|---|---|
| `src/main.rs` | eframe/egui 设置窗口、tray-icon 菜单、global-hotkey 注册、单实例互斥、后台任务状态 |
| `src/platform.rs` | Win32 焦点、密码编辑框检查、输入注入、用户操作检测、剪贴板事务 |
| `src/config.rs` | 非敏感配置、默认提示词、URL 校验、系统凭据入口 |
| `src/autostart.rs` | 当前用户登录启动项、带引号的绝对路径、保存失败回滚 |
| `src/api.rs` | OpenAI 兼容 Chat Completions、超时、响应限制、错误归一化 |

主线程驱动 Win32 消息循环、托盘和界面。全局快捷键与托盘菜单使用线程安全的 mpsc 事件入口；每次入队立即 PostMessage 到独立 message-only 窗口，由该窗口所属线程访问控制器，不依赖 eframe 绘制。快捷键回调记录当时的目标焦点，避免延迟处理时读到其他窗口。按快捷键后将配置快照交给后台线程，执行捕获 → 网络请求 → 条件回填，通过通道及原生消息报告结果。最多一个后台任务。

控制器由主线程 Rc/RefCell 持有，线程局部 Weak 引用仅供原生分派窗口访问；其他线程只调用全局 mpsc 发送端。如果 RefCell 正被 UI 或原生重入调用借用，事件留在通道中，并立即另发一次唤醒，窗口隐藏时也能继续重试。旧实现使用回调线程的局部队列且只在 UI update 收尾唤醒，存在后台事件滞留风险。设置窗口隐藏时无须绘制即可响应快捷键、托盘和任务完成。

## 文本事务

1. 记录 foreground HWND 与 focused HWND，拒绝标准 Edit 类的密码样式。
2. 等待快捷键释放，确认焦点仍在原位置。
3. 创建 message-only 窗口作为剪贴板所有者，打开剪贴板，**提前实体化复制**各 HGLOBAL 格式；不依赖失效的延迟渲染对象。
4. 写入独特哨兵并记录剪贴板序列号，注入 Ctrl+A、Ctrl+C；先等序列号改变再读取文字，避免反复 OpenClipboard 与目标应用复制竞争。超过期限则失败，不读取旧剪贴板当输入。
5. 恢复备份；拒绝空白输入、超长输入。网络请求期间保留原输入框的选区。
6. 使用低级键盘／鼠标钩子的单调事件计数及 HWND 判断是否发生用户操作。程序自身 SendInput 带有唯一 dwExtraInfo 标记，不会误取消自己；仅鼠标移动不增加计数，键盘输入、点击、滚动会。钩子不存储按键内容。
7. 再次备份剪贴板，重新 Ctrl+A／Ctrl+C 核对原文。原文一致且活动计数与焦点未变才 Ctrl+V，支持 Electron 在等待期间收起选区。
8. 等待后重新复制输入框，核对实际内容与模型输出；读回不符或复制失败会明确显示未确认替换。成功时右方向键收起核对选区，恢复剪贴板。失败也保留模型结果。

每个输入组合由一次 SendInput 调用发送，包含完整按下／释放序列；部分失败时尝试释放 Ctrl 和字母键。无 Enter、无自动提交、无强制激活目标窗口。`SendInput` 成功只代表事件进入队列；只有最终读回一致才显示“已确认替换”。换行比较统一 CRLF/LF 并忽略编辑器附加的末尾换行，不忽略有意义的空格。

剪贴板备份通过 RAII 收尾，显式恢复失败会报告错误；若全局序列号已变，认为其他程序或用户更新了剪贴板，不覆盖该新内容。复制期间第三方抢占、应用异步粘贴超过等待时长等场景仍可能失败。支持 Unicode/HTML/RTF/DIB/文件列表等 HGLOBAL 格式，存在 DIB 时跳过其冗余 GDI bitmap 句柄；未知或无法锁定的格式在操作前拒绝，特殊 GDI、palette、metafile 不提供通用序列化。

活动计数结合前后文本核对能拦截常见误覆盖，仍不能消除最后检查与实际注入之间的竞态。浏览器焦点 HWND 常是渲染容器，不能等同 DOM 输入框。用户打开设置或切换窗口会阻止回填，不会强制抢回原窗口。后续增强需要 UI Automation 或应用专用集成。

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

配置先写唯一命名的临时文件并 sync_all，再 rename 替换；写入前将现有有效 JSON 保留为 config.json.bak。加载主文件失败时优先恢复备份并持续显示提示，原损坏文件在用户保存前保留；主文件与备份都失败才显示默认设置和明确错误路径。Key 由 keyring windows-native 后端存储，不参与序列化。快捷键修改先注册新组合，再持久化，成功后解除旧组合；失败撤销新注册。Key 存储和 JSON 存储不具有跨系统原子事务：极少数 JSON 写入失败场景，Key 可能已经更新，状态会明确报告保存失败。

命名为 `Local\FluentKey.SingleInstance` 的 mutex 防止同一登录会话重复启动。第二次启动直接退出，不会唤醒第一个窗口；从托盘打开设置。关闭设置隐藏窗口；退出发生在后台任务完成收尾之后。未注册系统服务。

`start_at_login` 默认 false，旧 JSON 通过 serde 默认值兼容。开机启动在保存设置时写入或删除 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 的 `FluentKey` 值（REG_SZ）。命令将当前 EXE 绝对路径完整加引号，以 UTF-16 保存，支持中文和空格路径，超过 Run 命令的 260 字符限制则拒绝。用户登录后由 Windows 启动；已有配置时按原生命周期隐藏到托盘。

打开程序时根据当前可执行路径的实际注册值初始化开关，避免旧配置文件重新启用用户移除的启动项。Windows 任务管理器的 StartupApproved 禁用状态是另一个系统级控制，本实现不会覆盖它。启动项修改与凭据、JSON 保存组合执行：先快照原注册值，修改启动项；后续持久化失败则恢复原类型及内容，恢复失败会显示明确错误。只写 FluentKey 自身命名值，不写 HKLM 或要求提权。自动测试使用隔离的测试子键，不触碰真实 Run 键。

## 参考接口

- [Microsoft SendInput](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput)：输入注入受 UIPI 完整性级别约束。
- [Microsoft LowLevelMouseProc](https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelmouseproc)：区分鼠标移动、点击和滚动。
- [eframe run_native](https://docs.rs/eframe/0.29.1/eframe/fn.run_native.html)
- [global-hotkey](https://docs.rs/global-hotkey/0.6.4/global_hotkey/)
- [tray-icon](https://docs.rs/tray-icon/0.19.3/tray_icon/)

依赖版本通过 Cargo.lock 固定；升级后应重新执行自动检查和 Windows 手动验收。
