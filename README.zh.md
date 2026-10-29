# dsh-computer-use

中文 | [English](README.md)

让 DeepSeek Harness 的 Agent 真正看见并操作 Windows 桌面：截屏、鼠标、键盘、窗口与剪贴板。核心能力由随包提供的 Rust 原生 helper 完成，不依赖浏览器或外部自动化程序。

## 安装

```bash
dsh plugin --profile web add @restregular/dsh-computer-use
```

桌面版也可以在「插件 → 插件管理器」里安装 `@restregular/dsh-computer-use`，然后在插件详情里启用。首次调用工具时，插件会把原生 helper 复制到 `%LOCALAPPDATA%\dsh-computer-use\` 并从那里启动，避免插件安装在低完整性目录时无法注入输入。

要求：Windows 10/11 x64，DSH 0.2.1 或更新。

## 三个工具

| 工具 | 作用 |
|---|---|
| `computer_screenshot` | 截取整个虚拟桌面、单个显示器、某个窗口或屏幕区域，返回图片和坐标元数据 |
| `computer_action` | 鼠标移动/点击/双击/右击/拖拽/滚轮，键盘按键/组合键/文本输入，等待，剪贴板读写 |
| `computer_windows` | 列出顶层窗口（id、标题、进程、bounds、状态、前台），聚焦窗口或最小化/最大化/还原 |

### 坐标规则

- `computer_screenshot` 返回的图片可能被缩小；`computer_action` 的默认坐标就是**这张图片的像素坐标**。
- 工具结果里的 `capture` 是真实桌面矩形，`image` 是模型看到的图片尺寸，插件按比例把图片坐标映射回桌面。
- 没有先截图就传图片坐标会报 `SCREENSHOT_REQUIRED`；确实要用绝对桌面像素时传 `coordinate_space: "screen"`。
- 多个 Agent/会话共享同一块桌面，插件不做跨会话协调。

### 推荐循环

1. `computer_windows` 找到目标窗口；
2. `computer_screenshot` 观察，必要时带 `window_id` 只看目标窗口；
3. `computer_action` 执行一个动作（聚焦窗口后先点击输入框，再 `type`）；
4. 再截一张图确认结果。点击成功不等于任务成功。

## 安全

- 输入是真实发生的，**不能撤销**；只能中断、释放按键并重新观察。
- 急停热键默认 `Ctrl+Alt+Shift+F12`：按下后 helper 立即进入 PAUSED、释放所有按键，后续输入报 `PAUSED`；再按一次恢复。模型不能自行恢复。
- 默认拦截 `Win+L`、`Ctrl+Alt+Del`、`Win+U`，可通过 `input.blockedCombos` 调整。
- 可选进程允许/拒绝列表：`safety.allowedProcesses` / `safety.blockedProcesses`。
- 锁屏、UAC 安全桌面会报 `SECURE_DESKTOP`；非提权 helper 向提权窗口注入会被 UIPI 拦截，返回 `INPUT_BLOCKED`/失败信息。
- 截图会进入 DSH 会话附件库，可能包含屏幕上的敏感信息；插件不会把 base64 写进会话日志。
- `input.requireApproval: true` 可在每个会话第一次输入前请求用户确认（当前会话的 approval policy 为 never 时会被拒绝）。

## 配置

在 `cordis.patch.yml` 里覆盖插件行；所有字段都有默认值：

```yaml
- id: dsh-computer-use
  config:
    screenshot:
      format: jpeg        # png | jpeg
      jpegQuality: 85
      maxEdge: 1200       # 0 = 保留原始尺寸
      includeCursor: true
      captureMethod: auto # auto | gdi | printwindow
    input:
      typeIntervalMs: 0
      dragSteps: 24
      maxHoldMs: 5000
      blockedCombos: ["win+l", "ctrl+alt+del", "win+u"]
      requireApproval: false
      maxTextLength: 20000
    safety:
      hotkey: "ctrl+alt+shift+f12"
      allowedProcesses: []
      blockedProcesses: []
    timeoutMs: 15000
    idleStopMs: 300000
    debug:
      logHelper: false
```

| 字段 | 默认 | 说明 |
|---|---|---|
| `enabled` | true | 关闭后不注册工具 |
| `helperPath` | 空 | 指定 helper exe；默认使用包内 helper 的 LOCALAPPDATA 副本 |
| `timeoutMs` | 15000 | 单次 helper 请求超时 |
| `idleStopMs` | 300000 | 空闲后停止 helper；0 表示常驻 |
| `screenshot.maxEdge` | 1600 | 编码前最长边，越小越省图片 token |
| `input.typeIntervalMs` | 0 | 逐字输入间隔 |
| `input.dragSteps` | 24 | 拖拽插值步数 |
| `input.maxHoldMs` | 5000 | 单次按住上限 |
| `safety.hotkey` | ctrl+alt+shift+f12 | 急停热键 |
| `safety.allowedProcesses` | [] | 非空时只允许这些进程名 |
| `safety.blockedProcesses` | [] | 这些进程名永远拒绝 |

## 开发

```bash
npm run build:native        # cargo build --release，并把 exe 放到 native/bin/win32-x64
npm test                    # Node 单元测试
node scripts/smoke.mjs      # 直接测试 helper：截图、窗口、鼠标、剪贴板
node scripts/notepad-test.mjs "$LOCALAPPDATA/dsh-computer-use/helper-test.exe"  # 输入端到端测试
```

Rust helper 位于 `native/`，使用 `windows-sys`、`image`、`serde_json`。它用一行一个 JSON 的协议与 Host 通信，一次只处理一个请求；Host 在超时/取消时杀掉 helper，重启后先发送 `input.release_all`。

## 已知限制

- 仅 Windows x64；ARM64 Windows 通过 x64 模拟运行，未单独验证。
- UIA 语义树/元素操作尚未实现（计划 v0.2）；当前依赖截图 + 坐标。
- GDI 对部分 GPU/DRM 窗口可能截到黑屏；窗口截图会尝试 `PrintWindow` 并回退到屏幕矩形。Windows Graphics Capture 是后续方案。
- 未签名的原生 exe 在部分机器上可能触发 SmartScreen/杀软提示。
- 多个会话或独立 DSH 进程共享同一桌面，无法自动协调操作。
- 某些游戏/受保护应用使用 Raw Input，可能忽略 `SendInput`。

## License

MIT。Rust 依赖的许可证见 Cargo.lock 与各 crate 的 LICENSE；发布包只包含编译后的 helper。
