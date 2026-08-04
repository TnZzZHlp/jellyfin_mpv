# jellyfin_mpv

一个单服务器、外部 mpv 的 Jellyfin 投屏接收端。程序只提供简单的登录窗口，不提供媒体库界面；连接后在后台注册为可远程控制的播放设备。

第一版支持：

- Windows 上可在登录窗口中输入地址、用户名和明文密码，也可使用命令行参数登录；
- 通过 `/Sessions/Capabilities/Full` 注册 Video/Audio 播放和远程控制能力；
- 连接 Jellyfin WebSocket，并自动重连、响应 KeepAlive；
- Access Token 被服务端注销或失效后，自动用用户名和密码重新登录并重试请求；
- 接收播放、暂停、继续、停止、跳转、设置音量、音量增减、静音/取消静音、播放中切换音轨、全屏切换和倍速；
- 支持多条目播放队列、自动连播、上一首/下一首、循环和随机播放；MPV 画面左上角可点击选择自动、指定码率或指定分辨率上限，播放中会重新请求 PlaybackInfo，并在同一 MPV 进程内尽量保持当前位置、暂停状态、音量、静音和倍速；
- 加载 Jellyfin 选中的外部或文本型内嵌字幕，并支持播放中远程切换/关闭字幕；
- 读取 Jellyfin 原生 `MediaSegments`，按设置自动跳过片头、片尾、前情回顾、预告或广告；
- 在 mpv 进度条悬停时显示 Jellyfin Trickplay 缩略图，兼容新版内置 OSC 和 uosc/thumbfast 协议；
- 请求 `/Items/{itemId}/PlaybackInfo`，优先让 mpv 直接播放，必要时使用 Jellyfin 返回的转码流；
- 为每次播放启动独立的外部 mpv，通过 JSON IPC 控制；
- mpv 窗口标题显示当前 Jellyfin 媒体名称；
- mpv 播放窗口可按设置以窗口化、最大化或全屏模式启动，并可在开始播放时前置一次窗口；
- Windows 默认不显示控制台，只在通知区域显示托盘图标；托盘菜单可打开实时日志终端或安全退出；
- Windows 下只允许一个接收端实例运行，重复启动会显示提示，避免相同设备的登录 Token 互相失效；
- 每 5 秒上报播放进度；播放结束、mpv 退出、切换媒体或程序退出时上报停止。

## 依赖

- Rust stable；
- `mpv`，并确保其在 `PATH` 中，或通过 `--mpv-path` 指定；
- Windows 编译 Rust MSVC 目标时，需要 Visual Studio C++ Build Tools 和 Windows SDK。

Windows 上若出现 `cannot open input file 'kernel32.lib'`，请在 Visual Studio Installer 中为当前安装添加“使用 C++ 的桌面开发”和 Windows 10/11 SDK。也可使用 MinGW 工具链：

```powershell
rustup toolchain install stable-x86_64-pc-windows-gnu --profile minimal
scoop install gcc
cargo +stable-x86_64-pc-windows-gnu build --release
```

## 运行

Windows 上直接运行且未提供完整登录参数时，会显示登录窗口：

```powershell
cargo run --release
```

窗口中的密码为明文输入（显示为掩码）。首次登录成功后，地址、用户名和密码会保存到当前 Windows 用户的凭据管理器；下次不带参数启动时会自动登录并只显示托盘图标。保存的登录信息失效或服务器无法连接时会重新显示登录窗口。提供部分登录参数时也会显示窗口，并优先用参数覆盖保存值。

也可以完全通过 PowerShell 参数启动；此时不会显示登录窗口：

```powershell
$passwordBase64 = [Convert]::ToBase64String(
    [Text.Encoding]::UTF8.GetBytes("你的密码")
)
cargo run --release -- `
    --server "https://jellyfin.example.com" `
    --username "你的用户名" `
    --password $passwordBase64 `
    --device-id "living-room-tv"
```

Linux/macOS：

```sh
export JELLYFIN_SERVER='https://jellyfin.example.com'
export JELLYFIN_USERNAME='你的用户名'
export JELLYFIN_PASSWORD="$(printf '%s' '你的密码' | base64)"
export JELLYFIN_DEVICE_ID='living-room-tv'
cargo run --release
```

程序运行后，该设备会出现在 Jellyfin 客户端的“播放到”设备列表中。`JELLYFIN_DEVICE_ID` 应在重启后保持不变；未设置时会根据主机名生成。可用 `--device-name` 修改列表中的显示名称。

Windows 上连接成功后会显示托盘图标。单击或右键图标可打开菜单：

- “设置...”可修改 Jellyfin 服务器地址、用户名、密码、自动跳过的分段类型及播放窗口选项；
- “打开日志终端”会启动独立的 PowerShell 窗口，显示最近 200 行并实时跟踪新日志；
- “退出”会停止当前 mpv、向 Jellyfin 上报停止并关闭后台任务。

日志持续写入 `%LOCALAPPDATA%\jellyfin_mpv\jellyfin_mpv.log`，时间戳使用 Windows 当前系统时区并包含 UTC 偏移。关闭日志终端不会退出投屏程序。

无人值守运行时可通过环境变量提供用户名和密码：

```powershell
$env:JELLYFIN_SERVER = "http://jellyfin.local:8096"
$env:JELLYFIN_USERNAME = "你的用户名"
$env:JELLYFIN_PASSWORD = [Convert]::ToBase64String(
    [Text.Encoding]::UTF8.GetBytes("你的密码")
)
$env:JELLYFIN_DEVICE_ID = "living-room-tv"
cargo run --release
```

`--password` 和 `JELLYFIN_PASSWORD` 的值必须是标准 Base64 编码；程序会将其解码为 UTF-8 密码后调用 Jellyfin 登录接口。Base64 只是编码，不是加密，参数和环境变量仍应按敏感信息保护。

无人值守启动时可加 `--no-ui`（或设置 `JELLYFIN_NO_UI=true`），这样缺少登录参数会立即报错，不会等待窗口输入。

生成 Base64 时必须使用不带 BOM 的 UTF-8。程序会自动移除密码解码结果末尾连续的 `CR`/`LF` 换行；仍建议使用上例以确保编码结果明确。

常用参数：

```text
--server <URL>              Jellyfin 根地址；环境变量 JELLYFIN_SERVER
--username <USERNAME>       Jellyfin 用户名；环境变量 JELLYFIN_USERNAME
--password <PASSWORD>       Base64 编码密码；环境变量 JELLYFIN_PASSWORD
--no-ui                     缺少登录参数时不显示窗口，直接报错
--device-name <NAME>        设备显示名称；环境变量 JELLYFIN_DEVICE_NAME
--device-id <ID>            稳定设备 ID；环境变量 JELLYFIN_DEVICE_ID
--mpv-path <PATH>           mpv 路径；环境变量 MPV_PATH
--mpv-arg <ARG>             额外 mpv 参数，可重复
```

例如禁用本机 mpv 配置并指定全屏：

```powershell
cargo run --release -- --mpv-arg=--no-config --mpv-arg=--fullscreen
```

默认播放模式为最大化，开始播放时默认不前置窗口。可在托盘菜单的“设置...”中改为窗口化、最大化或全屏，并选择是否在开始播放时将窗口置于前台；保存后会在下次播放生效。`--mpv-arg` 中的 `--window-maximized` 或 `--fullscreen` 仍可覆盖窗口模式设置。

设置 `RUST_LOG=jellyfin_mpv=debug` 可输出调试日志。通过登录窗口登录成功后，登录信息保存在 Windows 凭据管理器的 `jellyfin_mpv/login` 项中，不会写入明文配置文件或日志；完全通过命令行或环境变量登录不会改写该凭据。为了支持 Token 失效后自动登录，程序运行期间会在内存中保留解码后的密码。登录后取得的 Access Token 不会写入日志，也不会放入 mpv 命令行或媒体 URL；程序通过 mpv IPC 设置 HTTP 请求头，并在 Token 更新后同步更新 mpv。

## 跳过媒体分段

播放媒体时，程序会通过 `/MediaSegments/{itemId}` 请求 Jellyfin 10.10 及以上版本的原生媒体分段。托盘菜单的“设置...”页面可分别勾选片头 (`Intro`)、片尾 (`Outro`)、前情回顾 (`Recap`)、预告 (`Preview`) 和广告 (`Commercial`)；默认只勾选片头和片尾。勾选项保存到 `%LOCALAPPDATA%\jellyfin_mpv\settings.json` 并立即影响当前及后续播放。

播放进入已勾选的有效分段后会自动跳到其结束时间并立即上报新进度；同一分段每次播放只自动跳过一次，因此手动回看不会反复触发。续播点或手动跳转已经越过的分段也不会在回看时自动跳过。

分段数据需要先由 Jellyfin 媒体分段提供器生成；没有分段数据时保持普通播放。Jellyfin 10.9 及更早版本返回 404 时会自动兼容，不影响播放。

## 播放窗口

托盘菜单的“设置...”页面提供“默认播放模式”（窗口化、最大化、全屏）以及“开始播放时将窗口置于前台”选项。前置操作只会在 mpv 成功加载该次媒体后执行一次，不会让窗口持续保持最前。选项与跳过分段设置一起保存到 `%LOCALAPPDATA%\jellyfin_mpv\settings.json`。

## 播放质量

播放窗口左上角始终显示“质量 · …”按钮；鼠标点击它即可打开 Jellyfin MPV 质量菜单。也可按 `F9` 打开，使用 `↑`/`↓` 选择、`Enter` 应用，`Esc` 或 `F9` 关闭。菜单提供“自动（不限制）”、多个码率上限以及 2160p、1440p、1080p、720p、480p、360p 分辨率上限。

选择码率会请求 Jellyfin 使用对应的最大流媒体码率；选择分辨率会通过设备播放配置限制视频宽高，源视频超过该上限时 Jellyfin 会生成相应尺寸的转码流。切换质量会在当前 MPV 进程中替换媒体 URL，从当前播放位置继续，保留暂停、音量、静音、倍速、音轨和字幕选择；它不会主动暂停或关闭播放窗口。新流仍需要重新连接和初始化解码器，因此网络或服务端转码较慢时可能出现很短的缓冲。选择“自动”会清除本地质量限制，恢复 Jellyfin/mpv 的正常直放和选流逻辑。

## 进度条缩略图

程序读取媒体项目的 Jellyfin Trickplay 清单，播放开始后以最多两个并发请求在后台预取全部压缩雪碧图。鼠标先悬停到尚未预取的区域时会直接复用或发起该图片的下载，不必等待整个预取队列。雪碧图使用当前登录会话鉴权，Access Token 不会写入图片 URL、mpv 参数或临时文件。

Jellyfin 服务器需要先在媒体库设置中启用并生成 Trickplay 图片。mpv 可使用支持 OSC Preview API 的新版内置 OSC，也可以使用 uosc 等兼容 thumbfast 消息协议的界面；程序会为每次播放自动加载桥接脚本，无需另行安装 `thumbfast.lua`。

压缩 JPEG 缓存在当前播放的临时目录，内存只保留当前解码的一张雪碧图，并把正在显示的单帧临时转换为 BGRA 覆盖层；播放结束后会删除桥接脚本、JPEG 缓存和像素临时目录。没有 Trickplay 数据、图片下载失败或 mpv 界面不支持预览请求时保持普通播放，不影响播放控制。

## 修改服务器设置

打开托盘菜单中的“设置...”，可修改服务器地址、用户名、密码和播放窗口选项。登录信息仍保存到 Windows 凭据管理器的 `jellyfin_mpv/login` 项，不会写入 `settings.json`。服务器或登录信息发生变化时，程序会停止当前播放、关闭旧 WebSocket 并连接新服务器；如果连接失败，会保留托盘和设置入口以便再次修改。

## 状态流

```text
Jellyfin WebSocket 命令
          │
          ▼
    串行播放状态机 ────── 每 5 秒 ─────▶ Jellyfin HTTP 进度上报
          │
          ▼
   外部 mpv JSON IPC ─── 播放结束 ────▶ Jellyfin HTTP 停止上报
```

当前仍不处理 `PlayTrailers`、多服务器、发现服务器和本地媒体库界面。`PlayTrailers` 需要额外处理远程 URL 和预告片队列；PGS 等图片型内嵌字幕仍由原始媒体或 Jellyfin 转码流处理，不单独提取加载。
