# jellyfin_mpv

一个无界面、单服务器、外部 mpv 的 Jellyfin 投屏接收端。程序本身不提供媒体库界面，只在后台注册为可远程控制的播放设备。

第一版支持：

- 使用用户 Access Token 连接一个 Jellyfin 服务器；
- 通过 `/Sessions/Capabilities/Full` 注册 Video/Audio 播放和远程控制能力；
- 连接 Jellyfin WebSocket，并自动重连、响应 KeepAlive；
- 接收播放、暂停、继续、停止、跳转、设置音量、静音和取消静音；
- 请求 `/Items/{itemId}/PlaybackInfo`，优先让 mpv 直接播放，必要时使用 Jellyfin 返回的转码流；
- 为每次播放启动独立的外部 mpv，通过 JSON IPC 控制；
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

PowerShell：

```powershell
cargo run --release -- `
    --server "https://jellyfin.example.com" `
    --username "你的用户名" `
    --device-id "living-room-tv"
# 随后会安全提示输入密码
```

Linux/macOS：

```sh
export JELLYFIN_SERVER='https://jellyfin.example.com'
export JELLYFIN_USERNAME='你的用户名'
export JELLYFIN_PASSWORD='你的密码'
export JELLYFIN_DEVICE_ID='living-room-tv'
cargo run --release
```

程序运行后，该设备会出现在 Jellyfin 客户端的“播放到”设备列表中。`JELLYFIN_DEVICE_ID` 应在重启后保持不变；未设置时会根据主机名生成。可用 `--device-name` 修改列表中的显示名称。

无人值守运行时可通过环境变量提供用户名和密码：

```powershell
$env:JELLYFIN_SERVER = "http://192.168.2.10:8096"
$env:JELLYFIN_USERNAME = "你的用户名"
$env:JELLYFIN_PASSWORD = "你的密码"
$env:JELLYFIN_DEVICE_ID = "living-room-tv"
cargo run --release
```

也可以继续使用某个用户登录后得到的 `AccessToken`：设置 `JELLYFIN_TOKEN` 或传入 `--token`。不能使用“控制台 → API 密钥”中创建的服务器 API Key，因为它不属于任何用户。

常用参数：

```text
--server <URL>              Jellyfin 根地址；环境变量 JELLYFIN_SERVER
--username <USERNAME>       Jellyfin 用户名；环境变量 JELLYFIN_USERNAME
--password <PASSWORD>       Jellyfin 密码；环境变量 JELLYFIN_PASSWORD
--token <TOKEN>             可选的用户 Access Token；与用户名登录二选一
--device-name <NAME>        设备显示名称；环境变量 JELLYFIN_DEVICE_NAME
--device-id <ID>            稳定设备 ID；环境变量 JELLYFIN_DEVICE_ID
--mpv-path <PATH>           mpv 路径；环境变量 MPV_PATH
--mpv-arg <ARG>             额外 mpv 参数，可重复
```

例如禁用本机 mpv 配置并指定全屏：

```powershell
cargo run --release -- --mpv-arg=--no-config --mpv-arg=--fullscreen
```

设置 `RUST_LOG=jellyfin_mpv=debug` 可输出调试日志。Access Token 不会写入日志，也不会放入 mpv 命令行或媒体 URL；程序通过 mpv IPC 设置 HTTP 请求头。

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

第一版有意不处理播放队列、上一首/下一首、远程选音轨/字幕轨、多服务器、发现服务器和任何本地图形界面。
