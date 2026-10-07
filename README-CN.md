# Ullage：一屏看清所有 AI 订阅的剩余额度

语言: [English](README.md) · [简体中文](README-CN.md)

一个本地守护进程与 CLI。它定时查询 Claude、ChatGPT、Grok、Cursor、OpenCode Go、Devin、codex2api 和 sub2api 的订阅用量，把每个窗口的剩余额度和重置时间汇总到同一个终端视图里。

同时订阅多家 AI 编码工具时，每家的用量都在各自的网页后台，窗口（5 小时、每周、每月）和单位（百分比、token、美元）各不相同。开启 Ullage 后，一条命令就能看清：**还剩多少、什么时候恢复、哪个账户快用完了**。

> 💡 凭据默认存放在平台凭据库，配置文件不含密钥。守护进程默认只通过本机私有控制通道通信，不监听任何 TCP 端口。

---

## 为什么需要 Ullage？

**常见做法：**

> 逐个登录各家网页后台查看用量。Claude 看 5 小时和每周额度，ChatGPT 看每周额度，Cursor 看月度额度和美元消耗。有几个账户，就要重复几遍。

**用 Ullage：**

```console

$ ullage tui

╭─ claude  max_20x  personal ────────────╮
│ 5h      remains 91%   3h05m ┄━━━━━━━━━ │
│ weekly  used up     ◦◉◉◉◉◉◉ ┄┄┄┄┄┄┄┄┄┄ │
│ fable   remains 25% ◆ 12d ◆ ┄┄┄┄┄┄┄━━━ │
╰────────────────────────────────────────╯

```

每个订阅一个方框。每一行是一个窗口：剩余比例、距离重置的时间、十格进度条。用尽的窗口显示 `used up`。

---

## 核心特性

- **八家提供方，多账户**：同一提供方可以添加多个账户，凭据和快照按账户隔离。
- **后台定时探测**：守护进程按账户定时查询提供方（默认每 300 秒一次）。`show` 和 `tui` 只读已保存的快照，不打扰提供方。
- **终端看板**：`ullage tui` 显示进度条和重置倒计时。终端变窄时逐级省略字段，手机大小的终端仍能看到额度什么时候恢复。
- **凭据安全**：凭据存放在 macOS Keychain、Windows Credential Manager 或 Linux Secret Service。账户标签、授权 URL 等个人信息默认脱敏输出。
- **可编程**：支持 JSON 输出。可选开启 HTTP API，客户端经一次性配对码配对后，按设备令牌访问。

---

## 安装

命令名是 `ullage`。crates.io 包名是 `ullage-cli`。

**Homebrew**（macOS 和 Linux）

```sh
brew install dualface/tap/ullage
```

Linux 需要先安装 [Homebrew on Linux](https://docs.brew.sh/Homebrew-on-Linux)。命令会按当前系统和 CPU 安装 GitHub Release 里的预编译二进制，然后执行 `ullage daemon install` 和 `ullage daemon start`。

**WinGet**（Windows）

```powershell
winget install Dualface.Ullage
```

> 目前还在等待审核，winget 审核极其缓慢，建议直接下载二进制使用。

安装包把 `ullage.exe` 放到 `%LOCALAPPDATA%\Ullage`，把该目录加入用户 `PATH`，然后执行 `ullage daemon install` 和 `ullage daemon start`。安装后请开一个新终端，以便 `PATH` 生效。GitHub Release 里的 zip 仍是便携版，不会注册计划任务。

**GitHub Releases**

从 [Releases](https://github.com/dualface/ullage-cli/releases) 下载对应系统的压缩包。

**从 git 安装**（Rust 1.85 或更新）

```sh
cargo install --git https://github.com/dualface/ullage-cli --locked ullage-cli
```

**从本仓库构建**

```sh
cargo build --release -p ullage-cli
```

二进制位于 `target/release/ullage`。若希望用户级服务命令能在固定位置找到它，把它放到 `PATH` 上。

## 快速上手

1. 登录一个账户。命令会引导你选择提供方并完成授权（详见下文「认证」）：

   ```sh
   ullage auth login
   ```

2. 查看全部账户的用量：

   ```sh
   ullage show --all
   ```

3. 打开终端看板。按 `v` 切换排列方式，按 `q` 退出：

   ```sh
   ullage tui
   ```

通过 Homebrew 或 WinGet 安装时，守护进程已自动安装并启动。用其他方式安装时，先执行 `ullage daemon install`（详见下文「守护进程生命周期」）。

---

## 实战配合：与 Kander 协同

[Kander](https://github.com/dualface/kander) 是多 Agent 看板调度工具。它调度的执行与审核 Agent（Claude Code、Codex、Cursor、Grok、Devin、OpenCode 等）分别消耗不同的订阅额度。

- **派卡前看额度**：用 `ullage tui` 看一眼各订阅的剩余额度，把大卡派给额度充足的 Agent。
- **避开窗口上限**：5 小时窗口快用完时，看重置倒计时，决定先等还是换一个 Agent。
- **多账户一屏看完**：同一提供方的多个账户分开显示，不用逐个登录后台。

---

以下是完整参考。

## 认证

交互式登录需要终端（stdin 和 stderr）。它会选择提供方、创建或复用账户、打印授权 URL、等待提供方要求的回调值（或轮询设备码流程）、校验已存储的凭据，然后询问账户标签：

```sh
ullage auth login
```

不需要内部账户 ID。各提供方会说明该粘贴什么：

- Claude：完整回调 URL 或 `code#state`
- ChatGPT：`code` 查询值
- Grok：设备码流程，没有可粘贴的内容。打开页面并自行完成
- Cursor：浏览器登录，没有可粘贴的内容。打开页面并自行完成。`--method api-token` 保留旧路径：在 cursor.com/dashboard 创建 User API Key，然后无回显地输入
- OpenCode：在 opencode.ai/auth 签发的 OpenCode Go API key
- Devin：浏览器登录，没有可粘贴的内容。`--method api-token` 可改为粘贴 Devin API key
- codex2api：网关 base URL、admin key 与上游账号 id 或 email，空格分隔（`base_url admin_key upstream_ref`）
- sub2api：一行 `base_url admin_key upstream_ref`——网关 base URL（HTTPS
  或环回 HTTP）、网关设置里生成的 admin API key、上游账号 id 或 name——
  无回显地输入

登录之后，`ullage show --all` 看起来像这样：

```console
$ ullage show --all
==== claude - max_20x ====
5h             remains 91%      -  3h -  [-#########]
weekly         used up          ------*  [----------]
fable          remains 25%      ------*  [-------###]

==== chatgpt - pro ====
weekly         used up          ----***  [----------]
```

## 数据路径

默认路径：

| 平台    | 配置                                                                    | 状态                                                                                                             | 控制                                   |
| ------- | ----------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- | -------------------------------------- |
| Linux   | `$XDG_CONFIG_HOME/ullage/config.json` 或 `~/.config/ullage/config.json` | `$XDG_STATE_HOME/ullage/state.json` 或 `~/.local/state/ullage/state.json`；已配对设备在该文件旁的 `devices.json` | `$XDG_RUNTIME_DIR/ullage/control.sock` |
| macOS   | `~/Library/Application Support/Ullage/config.json`                      | `~/Library/Application Support/Ullage/state.json`；已配对设备在该文件旁的 `devices.json`                         | `$TMPDIR/ullage-<uid>/control.sock`    |
| Windows | `%APPDATA%\Ullage\config.json`                                          | `%LOCALAPPDATA%\Ullage\state.json`；已配对设备在该文件旁的 `devices.json`                                        | `\\.\pipe\ullage-<user-scope>`         |

覆盖项：`ULLAGE_CONFIG_FILE`、`ULLAGE_STATE_FILE`、`ULLAGE_CONTROL_SOCKET`（Unix）、`ULLAGE_CONTROL_PIPE`（Windows）、`ULLAGE_TUI_STATE_FILE`（TUI 记住的排列方式，默认是状态文件旁边的 `tui.json`）。

可选的文件凭据目录，仅在 `credentials.file_fallback` 为 true 且原生存储不可用时使用：Linux 为 `$XDG_DATA_HOME/ullage/credentials` 或 `~/.local/share/ullage/credentials`；macOS 为 `~/Library/Application Support/Ullage/credentials`；Windows 为 `%LOCALAPPDATA%\Ullage\credentials`。

## 配置

版本 1 的 JSON。密钥材料不属于该 schema。文件缺失时，Ullage 使用内置提供方端点和空账户列表。

加载时拒绝：

- 未知字段
- 重复账户 ID
- 零时长
- 符号链接
- 大于 1 MiB 的文件

```json
{
  "version": 1,
  "daemon": {
    "maximum_concurrency": 8,
    "default_provider_concurrency": 2,
    "provider_limits": []
  },
  "providers": {},
  "credentials": {
    "file_fallback": false
  },
  "http": {
    "enabled": false,
    "bind": "127.0.0.1:7878",
    "allowed_origins": [],
    "probe_min_interval_seconds": 60
  },
  "accounts": [
    {
      "id": "claude-work",
      "provider": "claude",
      "label": "work",
      "enabled": true,
      "interval_seconds": 300,
      "timeout_seconds": 30,
      "jitter_seconds": 5,
      "backoff_initial_seconds": 30,
      "backoff_maximum_seconds": 1800
    }
  ]
}
```

`provider` 必须是 `claude`、`chatgpt`、`grok`、`cursor`、`opencode`、
`devin`、`codex2api` 或 `sub2api` 之一。提供方的 OAuth 和计费端点编译
进二进制，不能在这里改写；codex2api 与 sub2api 网关例外——其 base URL
来自登录时粘贴的凭据。

### 凭据

`credentials.file_fallback` 默认关闭。此时 Ullage 只使用平台凭据库（macOS Keychain、Windows Credential Manager 或 Linux Secret Service）。没有 Secret Service 的机器上，认证会以明确错误失败；加上 `--diagnose` 再跑，可以看到本机没有 Secret Service，以及把 `credentials.file_fallback` 设为 `true` 即可启用文件回退。

开关打开后，只要原生后端可用，Ullage 仍优先用它。只有原生后端报告自己不可用时，Ullage 才会创建上面的平台文件凭据目录。

- 该目录中的凭据以明文存储。任何能读当前用户文件的进程都能读到它们。
- 目录会建成私有（`0700` / 当前用户 DACL）。
- 权限检查失败会停止守护进程，而不是悄悄降级。
- 旧配置若省略 `credentials` 对象，仍按开关关闭加载。

### HTTP 绑定

`http.enabled` 默认 false。关闭时守护进程不监听任何 TCP 端口。

启用后，`http.bind` 接受 `auto:<port>`，或一个明确的回环、Tailscale 或私有局域网地址：

| 值                      | 行为                                     |
| ----------------------- | ---------------------------------------- |
| `auto:7878`             | 发现所有合格的本机地址并在每个地址上监听 |
| `<tailscale-ipv4>:7878` | 单个 Tailscale 地址                      |
| `<lan-ipv4>:7878`       | 单个私有局域网地址                       |

通配、链路本地、组播和公网地址会拒绝启动，并指出 `http.bind`。

在 `auto` 模式下，启动时每个监听地址打一行 `http.bind listening <addr> (<class>)`。若一开始没有 Tailscale 或局域网地址，发现会重试最多 60 秒，然后以回环启动。非回环绑定失败会打警告并跳过。若 HTTP 初始化整体失败（包括回环绑定失败），daemon 仍会继续运行，只提供控制套接字；错误会写入 daemon 的 stderr（由 CLI 启动的 daemon 会将其捕获到每用户的 `daemon-error-*.log`）。

## HTTP API

认证、账户变更和工作区控制仍走私有控制套接字。HTTP 服务器只提供查询和配对。

### 路由

| 方法   | 路径                                           | 说明                                                |
| ------ | ---------------------------------------------- | --------------------------------------------------- |
| `POST` | `/v1/pair`                                     | 配对；不需要 Bearer 令牌                            |
| `GET`  | `/v1/status`                                   |                                                     |
| `GET`  | `/v1/providers`                                |                                                     |
| `GET`  | `/v1/accounts`                                 |                                                     |
| `GET`  | `/v1/accounts/{id}`                            |                                                     |
| `GET`  | `/v1/usage[?account={id}]`                     | 全部账户或单个账户的缓存快照；不联系提供方          |
| `GET`  | `/v1/usage[?account={id}]&metric={display-name}` | 可选的保留列表过滤                                |
| `POST` | `/v1/accounts/{id}/probe[?wait=true\|false]`   | 受 `http.probe_min_interval_seconds`（默认 60）限制 |

`/v1/usage` 的 `account` 可选；空值或重复的 `account` 返回 `400 bad_request`。`wait` 默认为 `true`：探测执行完毕后返回 `200` 和结果。`wait=false` 启动探测后立即返回 `202`。除 `/v1/pair` 外，每条路由还接受 `diagnose=0|1`（见[错误](#错误)）。路由不接受的查询键、重复的键或格式错误的值返回 `400 bad_request`。

重复 `metric=` 会保留多个显示名的并集。这是一次性保留列表，与持久化的每账户 `metrics` 字段相反，后者点名要隐藏的行。

- 名字先去掉首尾空白，然后精确匹配，只对 ASCII 字母不区分大小写，并忽略该行所属窗口。重复的名字会合并。
- 只有显示行会被过滤。像 `limit_reached` 这类隐藏簿记仍会到达客户端，因此已达上限仍然可见。
- 合法但未知的名字返回 `200` 且没有可见测量。
- 空或只含空白的名字、超过 128 个字符的名字、超过 64 个名字，或含控制字符、双向格式字符的名字返回 `400 invalid_metric`。百分号解码后不是合法 UTF-8 的值返回 `400 bad_request`。
- `metric` 只在 `/v1/usage` 上接受；其他路由以 `400 bad_request` 拒绝。
- 同一账户在最小间隔内的探测请求返回 `429 rate_limited`，`Retry-After` 为剩余秒数（至少 1），两种 `wait` 模式都适用。只有已存在的账户受此限制；探测未知账户返回 `404`。

### 认证与配对

请求按以下顺序检查：`Host`、`OPTIONS`、`/v1/pair`、Bearer 令牌、请求体，最后是路由、方法和查询参数。因此：

- 缺失或不被允许的 `Host` 最先得到 `403`，`OPTIONS` 和配对也不例外。
- `OPTIONS` 不需要令牌。已知路径返回 `204`，未知路径返回 `404`。
- `/v1/pair` 上的任何方法都跳过令牌检查；只允许 `POST`。
- 其他请求都需要 `Authorization: Bearer <device_token>`。scheme 区分大小写。失败返回 `401` 并带 `WWW-Authenticate: Bearer`。
- 令牌在路由匹配之前检查，所以未认证地请求未知路径或使用错误方法，得到的是 `401`，不是 `404` 或 `405`。

配对请求（`Content-Type: application/json`，不带查询字符串）：

```json
{ "pair_code": "ABC-DEF", "device_name": "client-host" }
```

响应是 `{"device_id":"...","device_token":"...","device_name":"..."}`：设备 ID、净化后的名称，以及 256 位 base64url 设备令牌（43 个字符，无填充）。该响应是唯一一次暴露原始令牌的时机。净化后为空的名称变为 `unknown`；净化后超过 64 个字符的名称返回 `400 bad_request`。

配对码规则：

- 六个字符，字母表 `23456789ABCDEFGHJKMNPQRSTVWXYZ`
- 输入不区分大小写
- 连字符只允许出现在展示位置，或整段省略
- 300 秒后过期
- 只能成功一次；生成新码会替换待用的码并清零其失败计数
- 五次校验失败后作废；无法解析的码也计为一次失败
- 每个源 IP 每秒一次；超出返回 `429` 和 `Retry-After`。尝试在校验请求体之前计数，所以格式错误的请求同样占用这一次。限流表最多同时跟踪 4096 个地址；表满时新地址得到 `429`。

### 设备记录

`devices.json` 存在状态文件旁边，仅当前用户可访问（`0600` / 受保护 DACL）。已吊销的设备仍留在文件中，带 `revoked_at` 标记，不能再通过认证。每条记录包含：

- 12 字符设备 ID
- 净化后的名称
- SHA-256 令牌哈希
- 创建时间
- 最后见到时间

从不包含原始令牌。认证会哈希出示的令牌，并与每条记录做恒定时间比较，不提前返回；只有有效记录能匹配。最后见到时间的写入限制为每台设备每 60 秒一次。损坏或不安全的设备文件会拒绝守护进程启动，且从不原地修复。遗留的 `http-token` 文件会被忽略，不会自动删除。

### Host、CORS 与传输

接受的 `Host` 值：`127.0.0.1:<port>`、`localhost:<port>` 以及实际监听地址。任意 IPv6 监听还会启用 `[::1]:<port>`。匹配不区分大小写；不带端口的 `Host` 会被拒绝。

`http.allowed_origins` 默认为空：

- 匹配的来源会回显并带 `Vary: Origin`，错误响应也一样。预检请求还会得到 `Access-Control-Allow-Methods: GET, POST, OPTIONS` 和 `Access-Control-Allow-Headers: Authorization, Content-Type`。
- 不匹配的来源没有 CORS 头。
- 配置中的 `*` 来源会被拒绝，加载配置和服务器绑定时都会检查。
- 服务器从不返回 `Access-Control-Allow-Origin: *` 或
  `Access-Control-Allow-Credentials: true`。

每个响应都带 `Cache-Control: no-store`、`X-Content-Type-Options: nosniff` 和 `Referrer-Policy: no-referrer`。每个监听地址最多服务 256 个连接；多出的连接会被立即关闭。请求头必须在 10 秒内到达。

远程访问可用直接的 Tailscale 或局域网地址，或 SSH 隧道。Ullage 不提供 TLS。Tailscale 流量由 WireGuard 加密，但局域网流量及其设备令牌是明文。

### 错误

响应体保持脱敏。`?diagnose=1` 只给 `/v1/usage` 和探测的提供方错误加上 `diagnostic` 字段；其他响应不变。大于 1 MiB 的请求体、大于 4 KiB 的配对体，或 10 秒内未读完的请求体会被拒绝，且不影响其他连接。

错误响应体有两种形状。HTTP 层产生的错误是 `{"error":"<kind>"}`。守护进程返回的错误是 `{"version":...,"request_id":"...","kind":"...","detail":...}`，请求诊断时另带 `diagnostic`。协议版本不匹配是 `{"version":...,"request_id":"...","error":"protocol_mismatch","supported_version":...}`。

| 条件                                                        | 状态                                               |
| ----------------------------------------------------------- | -------------------------------------------------- |
| 缺失或不被允许的 `Host`                                     | `403 forbidden`                                    |
| 缺失或无效的 Bearer 令牌                                    | `401` 并带 `WWW-Authenticate: Bearer`              |
| 未知路由                                                    | `404 not_found`                                    |
| 方法错误                                                    | `405 method_not_allowed` 并带 `Allow`              |
| 路径编码或查询参数错误                                      | `400 bad_request`                                  |
| metric 名字错误                                             | `400 invalid_metric`                               |
| 协议版本不匹配                                              | `400 protocol_mismatch`                            |
| 不支持的命令                                                | `400`                                              |
| 请求体超出大小限制                                          | `413 payload_too_large`                            |
| 请求体读取超时                                              | `408 request_timeout`                              |
| 探测冷却期内                                                | `429 rate_limited` 并带 `Retry-After`              |
| `AccountNotFound`、`AccountSelectorNotFound`、未知账户或提供方 | `404`                                           |
| `AuthenticationInvalid`                                     | `409`                                              |
| 提供方 `RateLimited`                                        | `429` 并带 `Retry-After`（提供方给的值，否则为 `1`） |
| `Timeout`                                                   | `504`                                              |
| `Storage`、设备存储故障或其他守护进程错误                   | `500`                                              |

配对使用 `403`、`405`、`408`、`413`、`429`、`500 storage`、`401 pair_code_invalid` 和 `400 bad_request`。带查询字符串、`Content-Type` 不是 `application/json`、请求体无法解析或名称过长时，配对返回 `400 bad_request`。

## 守护进程生命周期

分离进程。命令在守护进程就绪后返回：

```sh
ullage daemon run
```

用户级服务（仅当前登录会话；不是系统管理员服务）：

```sh
ullage daemon install
ullage daemon start
ullage daemon status
ullage daemon stop
ullage daemon uninstall
```

`brew install` 和 `brew upgrade` 会自行执行 `ullage daemon install` 和 `ullage daemon start`，把 LaunchAgent（或 systemd 用户单元）钉到当前 Cellar keg 路径。`winget install Dualface.Ullage` 通过 Inno 安装包的 `[Run]` 条目做同样的事，把当前用户的计划任务钉到 `%LOCALAPPDATA%\Ullage\ullage.exe`。

`install` 注册启动项并启动守护进程；若服务已安装，会先停止运行中的守护进程并重写启动项。`uninstall` 停止守护进程后只删除启动项。配置、凭据、快照和日志会留下。`status` 在已认证的本机端点可达时报告守护进程在线；否则区分已安装但已停止，与未安装。在线表格输出包含 `CREDENTIAL_BACKEND`（`linux_secret_service`、`macos_keychain`、`windows_credential_manager`、`file_fallback` 或 `other_platform`）。JSON 在 `payload.credential_backend` 上使用相同标识符。

Linux 使用 systemd 用户单元，macOS 使用 LaunchAgent，Windows 使用当前用户的任务计划程序任务。平台路径和权限细节见 `docs/architecture.md`。

## 设备

用私有本机控制通道配对并管理 HTTP API 客户端：

```sh
ullage device pair
ullage device list
ullage device revoke <device-id>
```

`device pair` 打印一次性配对码及其过期时间，然后提示你在客户端输入 HTTP API 地址和该码。配对码 300 秒后过期；再生成一个会立即作废上一个。`device list` 只显示设备 ID、名称、创建时间和最后见到时间。从不打印设备令牌或令牌哈希。`device revoke` 立即生效，且不询问确认。

## 账户

```sh
ullage provider list
ullage account add claude --label work
ullage account list
ullage account show <account-id>
ullage account enable <account-id>
ullage account disable <account-id>
ullage account label <account-id> [label]
ullage account metrics <account-id> [metric]...
ullage account remove <account-id>
```

同一提供方可有多个账户。凭据和快照按账户隔离。`account label` 原地重命名账户；省略标签则清空。交互式登录在认证成功后也会询问标签。

`account list` 有 `METRICS` 列，`account show` 有 `METRICS` 行，未存储任何内容时都显示 `-`。`account metrics` 替换已存储的隐藏列表；省略全部名字则清空。这些已存储的名字会从该账户的可读摘要中隐藏，因此 `--metric` 和 HTTP `metric=` 仍是一次性保留列表，点名要显示的行。名字按精确、不区分大小写的显示名匹配，并忽略该行所属窗口。非法名字以退出码 `64` 结束，且不联系守护进程。

## 探测与查看

```sh
ullage probe <account-id>
ullage probe <account-id> --no-wait
ullage show <account-id>
ullage show <account-id> --metric usage
ullage show --all
ullage show --all --no-metric-filter
ullage tui
ullage tui --vertical
```

`probe` 查询提供方并持久化快照。`show` 读取已持久化的快照，不调用提供方。

`tui` 将全部已持久化快照读入 alternate screen 视图。每个订阅是一个圆角方框，上框线里嵌着 `提供方  套餐  账户`，框与框之间空一行；行的读法与 `show` 完全一致，并额外给出每个窗口距离重置的等待时间：

```console
╭─ claude  max_20x  personal ────────────╮
│ 5h      remains 91%   3h05m ┄━━━━━━━━━ │
│ weekly  used up     ◦◉◉◉◉◉◉ ┄┄┄┄┄┄┄┄┄┄ │
│ fable   remains 25% ◆ 12d ◆ ┄┄┄┄┄┄┄━━━ │
╰────────────────────────────────────────╯
```

## JSON 结构

成功的 JSON 是带标签的 `ControlResult`。紧凑的
`ullage --output json show <account>` 形如：

```json
{
  "result": "snapshots",
  "payload": [
    {
      "account_id": "claude-work",
      "usage": {
        "outcome": "complete",
        "data": {
          "provider": "claude",
          "account_label": "[redacted]",
          "plan": "pro",
          "subscription_expires_at": null,
          "observed_at": "2026-08-27T12:00:00Z",
          "windows": [
            {
              "window": { "kind": "five_hours" },
              "resets_at": "2026-08-27T17:00:00Z",
              "measurements": [
                {
                  "name": "tokens",
                  "used": 12.0,
                  "limit": 100.0,
                  "unit": { "kind": "tokens" }
                }
              ]
            }
          ]
        }
      },
      "last_success_at": "2026-08-27T12:00:00Z",
      "stale": false,
      "last_error": null,
      "last_error_at": null,
      "metrics": []
    }
  ]
}
```

### 用量字段

| 字段                                 | 规则                                                                              |
| ------------------------------------ | --------------------------------------------------------------------------------- |
| `window.kind`                        | `five_hours`、`weekly`、`monthly`，或 `{"kind":"other","id":"...","label":"..."}` |
| 缺失的 5h 或 weekly 窗口             | 省略；从不填合成零                                                                |
| `unit.kind`                          | `requests`、`tokens`、`percent`、`credits`、`{"kind":"currency","code":"..."}`，或 `{"kind":"other","id":"...","label":"..."}` |
| `limit`                              | 始终存在；供应商未报告上限时为 `null`                                             |
| `resets_at`、`plan`、`account_label` | 未知时为 `null`。非 null 的 `account_label` 在没有 `--reveal` 时为 `[redacted]`   |
| `subscription_expires_at`            | 提供方没有到期时间时为 `null`                                                     |
| `"outcome":"partial"`                | 带 `failures`，即 `{"scope":"...","message":"..."}` 列表；没有 `--diagnose` 时两个值都是 `[redacted]`。CLI 退出码 `2` 表示部分成功 |
| `last_error`                         | `null`、字符串（`network`、`authentication_invalid`、`protocol_incompatible`、`unsupported_capability`、`timeout`、`cancelled`、`provider_not_found`、`account_not_found`、`storage`），或 `{"rate_limited":{"retry_after_seconds":<数字或 null>}}` |
| `metrics`                            | 始终存在。账户已保存的、在可读摘要中隐藏的显示名列表；它不过滤 JSON               |

`probe` 返回 `{"result":"probe","payload":{"account_id":"...","usage":...,"metrics":[...]}}`，`usage` 形状相同。其他命令使用其他 `result` 标签，例如 `daemon_status`、`providers`、`accounts`、`account`、`auth_challenge`、`auth_state`、`workspaces` 和 `workspace`。

只有一个命令输出的 JSON 不是 `ControlResult`：守护进程未运行时，`daemon status` 输出 `{"service":"installed","status":"stopped"}`（或 `"not_installed"`），退出码 `0`。

JSON 和 pretty-json 始终携带这份原始 `ControlResult`。`--raw` 不改变它们的结构或字节，因此基于该 schema 的解析器无论是否传递该标志都能继续工作。

### 设备命令

同样的带标签形状。设备列表载荷不含令牌或令牌哈希字段。

| 命令                    | 结果                                                                     |
| ----------------------- | ------------------------------------------------------------------------ |
| `device pair`           | `{"result":"pair_code","payload":{"code":"ABC-DEF","expires_at":"..."}}` |
| `device list`           | `{"result":"devices","payload":[{"id":"...","name":"...","created_at":"...","last_seen_at":"..."}]}` |
| `device revoke`（成功） | `{"result":"ack"}`                                                       |

### 错误

错误写到 stderr，只有一个例外：`auth` 命令报告凭据无效时，在 stdout 输出普通的 `{"result":"auth_state",...}` 结果，退出码 `3`。没有 `--diagnose` 时其 `reason` 为 `[redacted]`。

| 输出               | 形状                                                                    |
| ------------------ | ----------------------------------------------------------------------- |
| 表格，运行时错误   | `error: <kind>`，有消息和提示时随后输出消息和 `hint:` 行                |
| 表格，解析错误     | clap 自己的消息（见下文）                                               |
| JSON / pretty-json | `{ "status": "error", "error": { "kind": "usage", "message": "..." } }` |

正在运行的守护进程比 CLI 旧时，stderr 第一行是 `warning: the running daemon (...) is older than this CLI (...)` 提示，在错误或 JSON 信封之前。

运行时错误示例：

```json
{ "status": "error", "error": { "kind": "timeout" } }
```

- 有解析错误文本时由 `message` 携带。
- `hint` 为以下 kind 提供静态说明：`daemon_unavailable`、`provider_registry_error`、`account_not_found`、`account_selector_not_found`、`invalid_account_metrics`、`invalid_control_socket`，以及因控制字符被拒的 `usage`。表格输出以 `hint:` 行打印同样的文本。
- `detail` 只在使用 `--diagnose` 时出现，携带认证和探测失败时提供方自己的错误文本。表格输出以 `detail:` 行打印。
- 省略的字段不序列化。
- JSON 从不包含 ANSI 颜色序列。

解析错误打印 clap 自己的消息，输出前会脱敏，从不回显终端控制字符。JSON 输出中它们变为 kind `usage`，文本放在 `message`。

- 缺少子命令显示该层的完整帮助。
- 未知标志、缺少参数和非法枚举值会带参数名，并在可用时给出 did-you-mean 建议或允许值。

含有不允许的控制字符的参数会以 kind `usage` 拒绝，没有 `message`，只有静态 `hint`。对于 `--method`、`--account` 这类已识别的取值选项，hint 会点出选项名；位置参数和其他标志使用通用消息。

| 情况                                | 退出码 | 去向                              |
| ----------------------------------- | ------ | --------------------------------- |
| `--help`、`-h`、`help`、`--version` | `0`    | stdout                            |
| 成功                                | `0`    | stdout                            |
| 其他失败                            | `1`    | stderr                            |
| `probe` 或 `show` 有部分快照        | `2`    | stdout                            |
| 凭据无效                            | `3`    | stderr；`auth_state` 结果为 stdout |
| 网络故障                            | `4`    | stderr                            |
| 守护进程不可用                      | `5`    | stderr                            |
| 与守护进程协议不匹配                | `6`    | stderr                            |
| 解析和用法错误、非法 metric 名字    | `64`   | stderr                            |

## 真实凭据测试

本发行没有真实凭据测试。`cargo test` 从不向供应商发送用户凭据或付费模型请求。若后续套件加入在线冒烟，必须显式选择加入，不得记录账户用量数字，也不得发送付费模型请求。

## 文档

- `docs/architecture.md` — crate 图、存储、托管和安全边界
- `docs/development.md` — 提供方扩展、供应商 DTO 兼容性、安全和发布前检查

## 安全

漏洞请发到 dualface@gmail.com。见 [`SECURITY.md`](SECURITY.md)。

## 许可证

MIT。见 [`LICENSE`](LICENSE)。

---

## 作者的其他项目

以下是 Ullage 作者 [dualface](https://github.com/dualface) 的其他项目：

- [Kander](https://github.com/dualface/kander)：规则驱动的多 Agent 看板调度，内置独立审核与交付门禁。
- [ste-zh](https://github.com/dualface/ste-zh)：让 Agent 按 ASD-STE100 原则用中文汇报结果，结论先行、状态词固定、写明是否验证。
- [QuickTUI](https://quicktui.ai/)：手机上的完整终端，适用于任何编码 Agent。自托管直连，单台主机免费。
