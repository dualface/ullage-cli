# Ullage

语言: [English](README.md) · [简体中文](README-CN.md)

Ullage 是本地守护进程和 CLI，用来查看 Claude、ChatGPT、Grok 和 Cursor 的订阅用量。单个 `ullage` 二进制同时托管守护进程，并通过本机私有控制套接字或命名管道与之通信。凭据默认放在平台凭据库中；配置文件不含密钥。

## 安装

命令名是 `ullage`。crates.io 包名是 `ullage-cli`。

**Homebrew**（macOS 和 Linux）

```sh
brew install dualface/tap/ullage
```

Linux 需要先安装 [Homebrew on Linux](https://docs.brew.sh/Homebrew-on-Linux)。公式会按当前系统和 CPU 安装 GitHub Release 里的预编译二进制，然后执行 `ullage daemon install` 和 `ullage daemon start`。

**WinGet**（Windows）

```powershell
winget install Dualface.Ullage
```

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

登录之后，`ullage show --all` 看起来像这样：

```console
$ ullage show --all
==== claude - max_20x ====
updated <1m ago
5h             remains 91%      resets in 3h37m   [-#########]
weekly         used up          resets in 41h07m  [----------]
fable          remains 25%      resets in 41h07m  [-------###]

==== chatgpt - pro ====
updated <1m ago
weekly-Codex   used up          resets in 3d05h   [----------]
Reset          credits 0
Balance        credits 0
```

## 数据路径

默认路径：

| 平台    | 配置                                                                    | 状态                                                                                                             | 控制                                   |
| ------- | ----------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- | -------------------------------------- |
| Linux   | `$XDG_CONFIG_HOME/ullage/config.json` 或 `~/.config/ullage/config.json` | `$XDG_STATE_HOME/ullage/state.json` 或 `~/.local/state/ullage/state.json`；已配对设备在该文件旁的 `devices.json` | `$XDG_RUNTIME_DIR/ullage/control.sock` |
| macOS   | `~/Library/Application Support/Ullage/config.json`                      | `~/Library/Application Support/Ullage/state.json`；已配对设备在该文件旁的 `devices.json`                         | `$TMPDIR/ullage-<uid>/control.sock`    |
| Windows | `%APPDATA%\Ullage\config.json`                                          | `%LOCALAPPDATA%\Ullage\state.json`；已配对设备在该文件旁的 `devices.json`                                        | `\\.\pipe\ullage-<user-scope>`         |

覆盖项：`ULLAGE_CONFIG_FILE`、`ULLAGE_STATE_FILE`、`ULLAGE_CONTROL_SOCKET`（Unix）、`ULLAGE_CONTROL_PIPE`（Windows）。

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

`provider` 必须是 `claude`、`chatgpt`、`grok`、`cursor`、`opencode`、`devin` 或 `codex2api` 之一。提供方的 OAuth 和计费端点编译进二进制，不能在这里改写；codex2api 网关例外——其 base URL 来自登录时粘贴的凭据。

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
| `GET`  | `/v1/usage?account={id}`                       | 缓存快照；不联系提供方                              |
| `GET`  | `/v1/usage?account={id}&metric={display-name}` | 可选的保留列表过滤                                  |
| `POST` | `/v1/accounts/{id}/probe?wait=false`           | 受 `http.probe_min_interval_seconds`（默认 60）限制 |

重复 `metric=` 会保留多个显示名的并集。这是一次性保留列表，与持久化的每账户 `metrics` 字段相反，后者点名要隐藏的行。

- 匹配按显示名精确、不区分大小写，并忽略该行所属窗口。
- 只有显示行会被过滤。像 `limit_reached` 这类隐藏簿记仍会到达客户端，因此已达上限仍然可见。
- 合法但未知的名字返回 `200` 且没有可见测量。
- 空、过长、数量过多或含控制字符的名字返回 `400 invalid_metric`。
- `metric` 只在 `/v1/usage` 上接受；其他路由以 `400 bad_request` 拒绝。
- 同一账户在最小间隔内的探测请求返回 `429` 和 `Retry-After`。

### 认证与配对

除 `POST /v1/pair` 和 `OPTIONS` 外，每条路由都需要
`Authorization: Bearer <device_token>`。

配对请求：

```json
{ "pair_code": "ABC-DEF", "device_name": "client-host" }
```

响应是设备 ID、净化后的名称，以及 256 位 base64url 设备令牌。该响应是唯一一次暴露原始令牌的时机。

配对码规则：

- 六个字符，字母表 `23456789ABCDEFGHJKMNPQRSTVWXYZ`
- 输入不区分大小写
- 连字符只允许出现在展示位置，或整段省略
- 300 秒后过期
- 只能成功一次；下一次生成的码会替换它
- 五次校验失败后作废
- 每个源 IP 每秒一次；超出返回 `429` 和 `Retry-After`

### 设备记录

`devices.json` 存在状态文件旁边，仅当前用户可访问（`0600` / 受保护 DACL）。每条有效记录包含：

- 12 字符设备 ID
- 净化后的名称
- SHA-256 令牌哈希
- 创建时间
- 最后见到时间

从不包含原始令牌。认证会哈希出示的令牌，并与每条有效记录做恒定时间比较，不提前返回。最后见到时间的写入限制为每台设备每 60 秒一次。损坏或不安全的设备文件会拒绝守护进程启动，且从不原地修复。遗留的 `http-token` 文件会被忽略，不会自动删除。

### Host、CORS 与传输

接受的 `Host` 值：`127.0.0.1:<port>`、`localhost:<port>` 以及实际监听地址。任意 IPv6 监听还会启用 `[::1]:<port>`。

`http.allowed_origins` 默认为空：

- 匹配的来源会回显并带 `Vary: Origin`。
- 不匹配的来源没有 CORS 头。
- 服务器从不返回 `Access-Control-Allow-Origin: *` 或
  `Access-Control-Allow-Credentials: true`。

远程访问可用直接的 Tailscale 或局域网地址，或 SSH 隧道。Ullage 不提供 TLS。Tailscale 流量由 WireGuard 加密，但局域网流量及其设备令牌是明文。

### 错误

除非设置 `?diagnose=1`，响应体保持脱敏。大于 1 MiB 的请求体、大于 4 KiB 的配对体，或读超时仍未完成的请求体会被拒绝，且不影响其他连接。

| 条件                       | 状态                     |
| -------------------------- | ------------------------ |
| 缺失或无效的 Bearer 令牌   | `401`                    |
| 未知路由                   | `404`                    |
| 非法参数                   | `400`                    |
| `AccountNotFound`          | `404`                    |
| `AuthenticationInvalid`    | `409`                    |
| 提供方或探测 `RateLimited` | `429` 并带 `Retry-After` |
| `Timeout`                  | `504`                    |
| `Storage`                  | `500`                    |

配对另外使用 `400 bad_request`、`401 pair_code_invalid`、`405`、`413` 和 `429`。

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

`install` / `uninstall` 只管理启动项。配置、凭据、快照和日志会留下。`status` 在已认证的本机端点可达时报告守护进程在线；否则区分已安装但已停止，与未安装。在线表格输出包含 `CREDENTIAL_BACKEND`（`linux_secret_service`、`macos_keychain`、`windows_credential_manager`、`file_fallback` 或 `other_platform`）。JSON 在 `payload.credential_backend` 上使用相同标识符。

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
```

`probe` 查询提供方并持久化快照。`show` 读取已持久化的快照，不调用提供方。

`--metric <display-name>` 只保留显示名精确匹配的摘要行，忽略大小写和该行所属窗口；重复该标志则保留多个名字的并集。`--no-metric-filter` 本次调用忽略账户已存储的过滤器。两个标志都不给时，可读摘要会隐藏各账户已存储 `account.metrics` 列表点名的行：`show --all` 遵循每个账户自己的列表，`probe` 应用它所查询账户的已存储列表。非法度量名以退出码 `64` 结束，且不联系守护进程。两个标志只影响可读摘要：`--raw` 和 JSON 输出保留全部测量。

当过滤器导致没有可显示的行，但该账户仍有可摘要的度量时，账户标题和 `updated` 行会保留，一行 `! no rows match the metric filter: <names>` 会列出过滤器中的名字，过期、上限和部分失败提示仍会打印；不会回退到原始表。没有可摘要测量的账户仍和以前一样回退到原始输出。

表格输出默认是可读摘要：每个可用测量一行，包含窗口、度量、剩余配额、窗口重置时间，以及作为最后一列的十格进度条。用尽的行显示 `used up` 而不是 `remains 0%`，后者看起来像空测量；不足百分之零点五的行显示 `remains <1%`，而不是向下取整成同一个零。重置时间在两天内用小时和分钟（`resets in 33h30m`），因为 `in 1d` 看不出等待是 25 小时还是 47 小时；超过两天后改用天。

两类提供方簿记永远不会变成摘要行。状态布尔值 `allowed`、`limit_reached`、`has_credits`、`unlimited`、`on_demand_enabled` 和 `enabled` 作为行被隐藏，但作为状态不会丢：已达上限变成 `! limit reached` 行，不计量的额度变成 `credits unlimited` 行，关闭的功能在它所作用的数量上标 `(off)`，若提供方没有报告该数量则给出单独的 `disabled` 行。Cursor 的 `included_spend` 和 `bonus_spend` 无条件排除在映射之外，即使没有 `total_spend` 也一样——它们不是状态，只是 `total_spend` 已经报告的金额的第二套拆分。

这两类仍出现在原始表中。用 `--raw` 可看到；不给该标志时，若映射后没有任何测量存活，账户块会带说明回退到原始表，并仍带过期、上限和部分失败提示。

```text
==== claude - pro ====
updated 2m ago
5h           usage  remains 97%  resets in 3h56m  [##########]
weekly       usage  remains 89%  resets in 5d15h  [-#########]
Weekly Opus  usage  remains 89%  resets in 5d15h  [-#########]
```

全局输出标志：`--output table|json|pretty-json`、`--color auto|always|never`、`--raw` 和 `--reveal`。`--color` 默认为 `auto`：当 stdout 是终端且 `NO_COLOR` 未设置或为空时给表格上色。JSON 和 pretty-json 输出从不上色。`--raw` 用未翻译的提供方表（`WINDOW`、`MEASUREMENT`、`USED`、`LIMIT`、`UNIT`、`RESETS_AT`）替换摘要；它只影响表格输出，对 `--output json` 和 `--output pretty-json` 是空操作。不给 `--reveal` 时，账户标签、授权 URI、流程 ID 以及类似个人值会替换成 `[redacted]`。`--raw` 不改变哪些内容被脱敏。即使给了 `--reveal`，错误细节仍保持脱敏。

`--diagnose`（或 `ULLAGE_DIAGNOSE=1`）会在 `show` 和 `probe` 上显示已脱敏的部分失败范围和类别。认证和探测命令失败时，还会让守护进程附带提供方自己的错误文本。默认错误输出仍是稳定的 kind。未显式开启时，守护进程响应上的诊断信息会被视为无效并拒绝。

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
      "last_error_at": null
    }
  ]
}
```

### 用量字段

| 字段                      | 规则                                                                              |
| ------------------------- | --------------------------------------------------------------------------------- |
| `window.kind`             | `five_hours`、`weekly`、`monthly`，或 `{"kind":"other","id":"...","label":"..."}` |
| 缺失的 5h 或 weekly 窗口  | 省略；从不填合成零                                                                |
| `limit`                   | 供应商未报告上限时省略或为 `null`                                                 |
| `subscription_expires_at` | 提供方没有到期时间时为 `null`                                                     |
| `"outcome":"partial"`     | 带 `failures`。CLI 退出码 `2` 表示部分成功                                        |

JSON 和 pretty-json 始终携带这份原始 `ControlResult`。`--raw` 不改变它们的结构或字节，因此基于该 schema 的解析器无论是否传递该标志都能继续工作。

### 设备命令

同样的带标签形状。设备列表载荷不含令牌或令牌哈希字段。

| 命令                    | 结果                                                                     |
| ----------------------- | ------------------------------------------------------------------------ |
| `device pair`           | `{"result":"pair_code","payload":{"code":"ABC-DEF","expires_at":"..."}}` |
| `device list`           | `{"result":"devices","payload":[...]}`                                   |
| `device revoke`（成功） | `{"result":"ack"}`                                                       |

### 错误

错误写到 stderr。

| 输出               | 形状                                                                    |
| ------------------ | ----------------------------------------------------------------------- |
| 表格               | 第一行 `error: <kind>`                                                  |
| JSON / pretty-json | `{ "status": "error", "error": { "kind": "usage", "message": "..." } }` |

运行时错误示例：

```json
{ "status": "error", "error": { "kind": "timeout" } }
```

- 有解析错误文本时由 `message` 携带。
- `hint` 为选定的运行时 kind 提供静态说明（例如 `daemon_unavailable` 或
  `provider_registry_error`）。CLI 能在不联系守护进程的情况下给出修复建议时，表格输出也会加一行 `hint:`。
- 省略的字段不序列化。
- JSON 从不包含 ANSI 颜色序列。

解析错误打印 clap 自己的消息，输出前会脱敏，从不回显终端控制字符：

- 缺少子命令显示该层的完整帮助。
- 未知标志、缺少参数和非法枚举值会带参数名，并在可用时给出 did-you-mean 建议或允许值。
- 像 `--method` 或 `--account` 这类已识别选项名会出现在 hint 中。
- 位置参数和未识别标志改用通用静态消息。

| 情况                                | 退出码 | 去向   |
| ----------------------------------- | ------ | ------ |
| `--help`、`-h`、`help`、`--version` | `0`    | stdout |
| 解析和用法错误                      | `64`   | stderr |

## 真实凭据测试

本发行没有真实凭据测试。`cargo test` 从不向供应商发送用户凭据或付费模型请求。若后续套件加入在线冒烟，必须显式选择加入，不得记录账户用量数字，也不得发送付费模型请求。

## 文档

- `docs/architecture.md` — crate 图、存储、托管和安全边界
- `docs/development.md` — 提供方扩展、供应商 DTO 兼容性、安全和发布前检查

## 安全

漏洞请发到 dualface@gmail.com。见 [`SECURITY.md`](SECURITY.md)。

## 许可证

MIT。见 [`LICENSE`](LICENSE)。

## 作者

[dualface](https://x.com/dualface)

- [QuickTUI](https://quicktui.ai/) — 面向 iPhone、iPad 和浏览器的 tmux/herdr 远程终端，让你在手机上操控 Mac 上的 Agent。
- [Kander](https://github.com/dualface/kander/) — 用看板调度多个 AI Agent 的任务编排工具。
