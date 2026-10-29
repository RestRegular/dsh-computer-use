# dsh-computer-use

中文 | [English](README.md)

让 DeepSeek Harness 的 Agent 真正看见并操作 Windows 桌面：截屏、鼠标、键盘、窗口与剪贴板。所有操作先经过一次明确的人工授权；授权后进入「接管」状态——半透明蒙版覆盖整个屏幕、物理鼠标键盘被丢弃，直到本轮任务结束。核心能力由随包提供的 Rust 原生 helper 完成，不依赖浏览器或外部自动化程序。

## 安装

```bash
dsh plugin --profile web add @restregular/dsh-computer-use
```

桌面版也可以在「插件 → 插件管理器」里安装 `@restregular/dsh-computer-use`，然后在插件详情里启用。首次调用工具时，插件会把原生 helper 复制到 `%LOCALAPPDATA%\\dsh-computer-use\\` 并从那里启动，避免插件安装在低完整性目录时无法注入输入。

要求：Windows 10/11 x64，DSH 0.2.1 或更新。

## 三个工具

| 工具 | 作用 |
|---|---|
| `computer_screenshot` | 截取整个虚拟桌面、单个显示器、某个窗口或屏幕区域，返回图片和坐标元数据 |
| `computer_action` | 鼠标移动/点击/双击/右击/拖拽/滚轮，键盘按键/组合键/文本输入，等待，剪贴板读写，`release` 交还控制 |
| `computer_windows` | 列出顶层窗口（id、标题、进程、bounds、状态、前台），聚焦窗口或最小化/最大化/还原 |

### 坐标规则

- `computer_screenshot` 返回的图片可能被缩小；`computer_action` 的默认坐标就是**这张图片的像素坐标**。
- 工具结果里的 `capture` 是真实桌面矩形，`image` 是模型看到的图片尺寸，插件按比例把图片坐标映射回桌面。
- 没有先截图就传图片坐标会报 `SCREENSHOT_REQUIRED`；确实要用绝对桌面像素时传 `coordinate_space: "screen"`。
- 多个 Agent/会话共享同一块桌面；同一时刻只允许一个 Agent 持有接管。

### 推荐循环

1. `computer_windows` 找到目标窗口；
2. `computer_screenshot` 观察，必要时带 `window_id` 只看目标窗口；
3. `computer_action` 执行一个动作（聚焦窗口后先点击输入框，再 `type`）；
4. 再截一张图确认结果。点击成功不等于任务成功。

## 授权与接管

1. **授权。** 每个回合的第一次鼠标/键盘动作都会走 DSH 审批弹窗。这要求会话的审批策略是 `ask`；如果是 `never`，审批会被自动拒绝，Agent 无法操作（fail closed）。`consent.scope: session` 可以让一次授权覆盖整个会话；`consent.requireApproval: false` 可以完全关闭授权。
2. **接管。** 授权通过后，helper 会在真正注入输入之前启动接管：
   - 半透明蒙版覆盖整个虚拟桌面，顶部提示条显示当前状态和急停热键；
   - 安装 `WH_KEYBOARD_LL` / `WH_MOUSE_LL` 低级钩子，默认丢弃所有**物理**鼠标和键盘事件，用户无法在 Agent 工作时操作电脑；
   - Agent 通过 `SendInput` 注入的输入带有 `injected` 标志，会正常穿过钩子，因此蒙版不会妨碍 Agent 自己操作；
   - 蒙版设置了 `WDA_EXCLUDEFROMCAPTURE`，模型的截图里看不到蒙版（老版本 Windows 不支持时，截图瞬间会短暂隐藏蒙版）；
   - 蒙版鼠标穿透、不抢键盘焦点。
3. **结束。** 回合结束（`agent/turn-stopping`）、Agent 主动调用 `computer_action` 的 `action: "release"`、Agent 被 dispose、急停热键、helper 退出时，控制权都会交还用户。`takeover.idleEndMs` 可设置空闲多久后自动释放。
4. **急停。** 接管期间按急停热键（默认 `Ctrl+Alt+Shift+F12`）会立即结束接管、恢复用户输入、让 helper 进入 PAUSED 并撤销本回合授权。正在执行的动作以 `TAKEOVER_ENDED` 中止，后续输入报 `PAUSED`。模型不能自行恢复：需要用户再按一次热键解除 PAUSED，下一次输入还会重新请求授权。
5. **关键按键。** 急停热键的所有组成键始终原样透传、不会被丢弃（helper 的轮询线程必须能看到它们）。`Ctrl+Alt+Del` 是安全注意序列，低级钩子无法拦截；锁屏/UAC 安全桌面也不受影响——用户始终能夺回机器。
6. **同一时刻一个接管者。** 其他 Agent 的输入会返回 `TAKEOVER_BUSY`，直到控制被释放。

在手机上通过 `dsh-pocket` 等通道审批也可以；蒙版本身始终画在被控制的 Windows 桌面上。

## 安全

- 输入是真实发生的，**不能撤销**；只能中断、释放按键并重新观察。
- 授权与接管默认开启。`input.requireApproval` 是 `consent.requireApproval` 的过时别名，仍然兼容。
- 急停热键（默认 `Ctrl+Alt+Shift+F12`）：按下后 helper 立即进入 PAUSED、释放所有按键，后续输入报 `PAUSED`；再按一次恢复。模型不能自行恢复。
- 接管期间蒙版会丢弃物理鼠标键盘输入。其它软件注入的输入同样带 `injected` 标志，和 Agent 自己的输入无法区分。
- 默认拦截 `Win+L`、`Ctrl+Alt+Del`、`Win+U`，可通过 `input.blockedCombos` 调整。
- 可选进程允许/拒绝列表：`safety.allowedProcesses` / `safety.blockedProcesses`。
- 锁屏、UAC 安全桌面会报 `SECURE_DESKTOP`；非提权 helper 向提权窗口注入会被 UIPI 拦截，返回失败信息。
- 截图会进入 DSH 会话附件库，可能包含屏幕上的敏感信息；插件不会把 base64 写进会话日志。

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
      maxTextLength: 20000
    consent:
      requireApproval: true
      scope: turn         # turn | session
    takeover:
      enabled: true
      opacity: 0.18       # 0.05 .. 0.9
      banner: true
      title: ""           # 留空使用本地化默认标题
      hint: ""            # 留空使用带热键的本地化默认提示
      idleEndMs: 0        # 0 = 回合结束时释放接管
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
| `idleStopMs` | 300000 | 空闲后停止 helper；0 表示常驻（接管期间会临时保活） |
| `screenshot.maxEdge` | 1600 | 编码前最长边，越小越省图片 token |
| `input.typeIntervalMs` | 0 | 逐字输入间隔 |
| `input.dragSteps` | 24 | 拖拽插值步数 |
| `input.maxHoldMs` | 5000 | 单次按住上限 |
| `consent.requireApproval` | true | 第一次输入动作前请求授权；`input.requireApproval` 仍可作为别名 |
| `consent.scope` | turn | `turn` 每回合重新授权，`session` 一次授权用到 Agent dispose |
| `takeover.enabled` | true | Agent 操作时覆盖蒙版并丢弃物理输入 |
| `takeover.opacity` | 0.18 | 蒙版不透明度，0.05–0.9 |
| `takeover.banner` | true | 显示带急停热键的提示条 |
| `takeover.title` / `hint` | 空 | 提示条文案；留空用本地化默认值 |
| `takeover.idleEndMs` | 0 | 空闲多久后释放接管；0 表示回合结束才释放 |
| `safety.hotkey` | ctrl+alt+shift+f12 | 急停与终止接管热键 |
| `safety.allowedProcesses` | [] | 非空时只允许这些进程名 |
| `safety.blockedProcesses` | [] | 这些进程名永远拒绝 |

## 开发

```bash
npm run build:native        # cargo build --release，并把 exe 放到 native/bin/win32-x64
npm test                    # Node 单元测试
node scripts/smoke.mjs      # 直接测试 helper：截图、窗口、鼠标、剪贴板
node scripts/notepad-test.mjs "$LOCALAPPDATA/dsh-computer-use/helper-test.exe"  # 输入端到端测试
node scripts/takeover-test.mjs "$LOCALAPPDATA/dsh-computer-use/helper-test.exe"  # 蒙版 + 钩子 + 急停
```

原生测试请在可弃置的桌面上运行，并先把 exe 复制到 `%LOCALAPPDATA%`：从低完整性工作区直接启动的 helper 能截图，但注入输入会被拒绝。接管测试会验证蒙版、截图排除、注入输入穿过钩子、Host 主动释放以及急停路径。

Rust helper 位于 `native/`，使用 `windows-sys`、`image`、`serde_json`。它用一行一个 JSON 的协议与 Host 通信，一次只处理一个请求；Host 在超时/取消时杀掉 helper，重启后先发送 `input.release_all`。接管蒙版和钩子运行在独立线程的消息循环里；helper 退出时 Windows 会自动移除钩子和蒙版。

## 已知限制

- 仅 Windows x64；ARM64 Windows 通过 x64 模拟运行，未单独验证。
- UIA 语义树/元素操作尚未实现；当前依赖截图 + 坐标。
- GDI 对部分 GPU/DRM 窗口可能截到黑屏；窗口截图会尝试 `PrintWindow` 并回退到屏幕矩形。Windows Graphics Capture 是后续方案。
- 未签名的原生 exe 在部分机器上可能触发 SmartScreen/杀软提示。
- 多个会话共享同一桌面；接管只在单个 DSH 进程内协调，多个 DSH 进程之间不协调。
- 某些游戏/受保护应用使用 Raw Input，可能忽略 `SendInput`。
- 低级钩子只作用于交互桌面；UAC/锁屏等安全桌面不受影响——这也是 `Ctrl+Alt+Del` 始终留给用户的原因。

## License

MIT。Rust 依赖的许可证见 Cargo.lock 与各 crate 的 LICENSE；发布包只包含编译后的 helper。
