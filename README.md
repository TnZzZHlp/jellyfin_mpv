# jellyfin_mpv

一个单服务器、外部 mpv 的 Jellyfin 投屏接收端。程序只提供简单的登录窗口，不提供媒体库界面；连接后在后台注册为可远程控制的播放设备。

第一版支持：

- Windows 上可在登录窗口中输入地址、用户名和明文密码，也可使用命令行参数登录；
- 通过 `/Sessions/Capabilities/Full` 注册 Video/Audio 播放和远程控制能力；
- 连接 Jellyfin WebSocket，并自动重连、响应 KeepAlive；
- Access Token 被服务端注销或失效后，自动用用户名和密码重新登录并重试请求；
- 接收播放、暂停、继续、停止、跳转、设置音量、静音和取消静音；
- 加载 Jellyfin 选中的外部或文本型内嵌字幕，并支持播放中远程切换/关闭字幕；
- 获取 Jellyfin 服务器备用字体，并下载媒体中的 TTF/OTF/TTC/OTC/WOFF/WOFF2 字体附件，供 ASS/SSA 字幕使用；
- 请求 `/Items/{itemId}/PlaybackInfo`，优先让 mpv 直接播放，必要时使用 Jellyfin 返回的转码流；
- 为每次播放启动独立的外部 mpv，通过 JSON IPC 控制；
- mpv 窗口默认最大化启动；
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

如需关闭默认最大化，可传入 `--mpv-arg=--window-maximized=no`。

设置 `RUST_LOG=jellyfin_mpv=debug` 可输出调试日志。通过登录窗口登录成功后，登录信息保存在 Windows 凭据管理器的 `jellyfin_mpv/login` 项中，不会写入明文配置文件或日志；完全通过命令行或环境变量登录不会改写该凭据。为了支持 Token 失效后自动登录，程序运行期间会在内存中保留解码后的密码。登录后取得的 Access Token 不会写入日志，也不会放入 mpv 命令行或媒体 URL；程序通过 mpv IPC 设置 HTTP 请求头，并在 Token 更新后同步更新 mpv。

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

程序会调用 Jellyfin `/FallbackFont/Fonts` 获取服务器配置的备用字体，并在进程内缓存；当前媒体的字体附件则通过 `/Videos/{videoId}/{mediaSourceId}/Attachments/{index}` 获取。播放时统一写入系统临时目录，通过 mpv `--sub-fonts-dir` 加载，并在播放结束后删除。单个字体限制为 32 MiB，每次播放最多写入 128 MiB；封面等非字体附件会被忽略。

第一版有意不处理播放队列、上一首/下一首、远程选音轨、多服务器、发现服务器和本地媒体库界面。PGS 等图片型内嵌字幕仍由原始媒体或 Jellyfin 转码流处理，不单独提取加载。
