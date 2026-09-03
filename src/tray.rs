#[cfg(windows)]
mod platform {
    use std::{
        mem::size_of,
        os::windows::process::CommandExt,
        path::{Path, PathBuf},
        process::{Child, Command},
        ptr::{null, null_mut},
        sync::mpsc,
        thread::{self, JoinHandle},
        time::Duration,
    };

    use anyhow::{Context, Result, bail};
    use tokio_util::sync::CancellationToken;
    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CREATESTRUCTW, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
                DestroyMenu, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetCursorPos,
                GetMessageW, GetWindowLongPtrW, HWND_MESSAGE, IDI_APPLICATION, IsWindow, LoadIconW,
                MF_DISABLED, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage,
                RegisterClassW, SW_HIDE, SetForegroundWindow, SetWindowLongPtrW, ShowWindow,
                TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP,
                WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_LBUTTONUP, WM_NCCREATE, WM_NCDESTROY, WM_NULL,
                WM_RBUTTONUP, WNDCLASSW,
            },
        },
    };

    use crate::settings_ui::{SettingsContext, show_settings};

    const WINDOW_CLASS: &str = "JellyfinMpvTrayWindow";
    const WM_TRAY: u32 = WM_APP + 1;
    const MENU_STATUS: usize = 1000;
    const MENU_OPEN_LOG: usize = 1001;
    const MENU_EXIT: usize = 1002;
    const MENU_SETTINGS: usize = 1003;
    const ICON_ID: u32 = 1;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

    pub struct TrayIcon {
        hwnd: HWND,
        thread: Option<JoinHandle<()>>,
    }

    struct TrayState {
        shutdown: CancellationToken,
        menu: isize,
        log_path: PathBuf,
        log_processes: Vec<Child>,
        settings: SettingsContext,
        settings_open: bool,
    }

    impl Drop for TrayState {
        fn drop(&mut self) {
            terminate_processes(&mut self.log_processes);
        }
    }

    impl TrayIcon {
        pub fn start(
            shutdown: CancellationToken,
            tooltip: String,
            log_path: PathBuf,
            settings: SettingsContext,
        ) -> Result<Self> {
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let thread = thread::Builder::new()
                .name("jellyfin-mpv-tray".to_owned())
                .spawn(move || {
                    run_message_loop(shutdown, &tooltip, log_path, settings, &ready_tx);
                })
                .context("创建托盘消息线程失败")?;
            let hwnd = ready_rx
                .recv_timeout(Duration::from_secs(5))
                .context("等待托盘图标初始化超时")?
                .map_err(anyhow::Error::msg)?;
            Ok(Self {
                hwnd: hwnd as HWND,
                thread: Some(thread),
            })
        }

        pub fn close(mut self) {
            self.close_inner();
        }

        fn close_inner(&mut self) {
            if let Some(thread) = self.thread.take() {
                if !thread.is_finished() {
                    unsafe {
                        PostMessageW(self.hwnd, WM_CLOSE, 0, 0);
                    }
                }
                let _ = thread.join();
            }
            self.hwnd = null_mut();
        }

        #[cfg(test)]
        pub fn request_exit(&self) {
            unsafe {
                PostMessageW(self.hwnd, WM_COMMAND, MENU_EXIT, 0);
            }
        }
    }

    impl Drop for TrayIcon {
        fn drop(&mut self) {
            self.close_inner();
        }
    }

    fn run_message_loop(
        shutdown: CancellationToken,
        tooltip: &str,
        log_path: PathBuf,
        settings: SettingsContext,
        ready: &mpsc::SyncSender<Result<isize, String>>,
    ) {
        match unsafe { create_tray_window(shutdown, tooltip, log_path, settings) } {
            Ok(hwnd) => {
                let _ = ready.send(Ok(hwnd as isize));
                unsafe {
                    let mut message: MSG = std::mem::zeroed();
                    while GetMessageW(&raw mut message, null_mut(), 0, 0) > 0 {
                        TranslateMessage(&raw const message);
                        DispatchMessageW(&raw const message);
                    }
                }
            }
            Err(error) => {
                let _ = ready.send(Err(error.to_string()));
            }
        }
    }

    unsafe fn create_tray_window(
        shutdown: CancellationToken,
        tooltip: &str,
        log_path: PathBuf,
        settings: SettingsContext,
    ) -> Result<HWND> {
        let instance = unsafe { GetModuleHandleW(null()) };
        if instance.is_null() {
            bail!("无法取得程序模块句柄");
        }
        let class_name = wide(WINDOW_CLASS);
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            ..unsafe { std::mem::zeroed() }
        };
        unsafe { RegisterClassW(&raw const window_class) };

        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            bail!("创建托盘菜单失败");
        }
        let status = wide("Jellyfin MPV Cast 已连接");
        let open_log = wide("打开日志终端");
        let settings_label = wide("设置...");
        let exit = wide("退出");
        unsafe {
            AppendMenuW(menu, MF_STRING | MF_DISABLED, MENU_STATUS, status.as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(menu, MF_STRING, MENU_SETTINGS, settings_label.as_ptr());
            AppendMenuW(menu, MF_STRING, MENU_OPEN_LOG, open_log.as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(menu, MF_STRING, MENU_EXIT, exit.as_ptr());
        }

        let state = Box::new(TrayState {
            shutdown,
            menu: menu as isize,
            log_path,
            log_processes: Vec::new(),
            settings,
            settings_open: false,
        });
        let state_ptr = Box::into_raw(state);
        let title = wide("Jellyfin MPV Cast");
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                instance,
                state_ptr.cast(),
            )
        };
        if hwnd.is_null() {
            unsafe {
                drop(Box::from_raw(state_ptr));
                DestroyMenu(menu);
            }
            bail!("创建托盘消息窗口失败");
        }

        let mut icon: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        icon.cbSize =
            u32::try_from(size_of::<NOTIFYICONDATAW>()).expect("NOTIFYICONDATAW size fits in u32");
        icon.hWnd = hwnd;
        icon.uID = ICON_ID;
        icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        icon.uCallbackMessage = WM_TRAY;
        icon.hIcon = unsafe { LoadIconW(null_mut(), IDI_APPLICATION) };
        copy_wide(&mut icon.szTip, tooltip);
        if unsafe { Shell_NotifyIconW(NIM_ADD, &raw const icon) } == 0 {
            unsafe { DestroyWindow(hwnd) };
            bail!("向 Windows 通知区域添加图标失败");
        }
        unsafe { ShowWindow(hwnd, SW_HIDE) };
        Ok(hwnd)
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_NCCREATE {
            let create = lparam as *const CREATESTRUCTW;
            let state = unsafe { (*create).lpCreateParams.cast::<TrayState>() };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };
            return 1;
        }

        let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState };
        let is_tray_click = u32::try_from(lparam)
            .is_ok_and(|message| matches!(message, WM_RBUTTONUP | WM_LBUTTONUP));
        match message {
            WM_TRAY if is_tray_click => {
                if !state.is_null() {
                    unsafe { show_menu(hwnd, (*state).menu as _) };
                }
                0
            }
            WM_COMMAND if wparam & 0xffff == MENU_EXIT => {
                if !state.is_null() {
                    unsafe { (*state).shutdown.cancel() };
                }
                unsafe { DestroyWindow(hwnd) };
                0
            }
            WM_COMMAND if wparam & 0xffff == MENU_OPEN_LOG => {
                if !state.is_null() {
                    match unsafe { open_log_terminal(&(*state).log_path) } {
                        Ok(process) => unsafe { (*state).log_processes.push(process) },
                        Err(error) => tracing::error!(%error, "打开日志终端失败"),
                    }
                }
                0
            }
            WM_COMMAND if wparam & 0xffff == MENU_SETTINGS => {
                if !state.is_null() {
                    if unsafe { (*state).settings_open } {
                        return 0;
                    }
                    unsafe { (*state).settings_open = true };
                    let context = unsafe { (*state).settings.clone() };
                    if let Err(error) = show_settings(context) {
                        tracing::error!(%error, "打开设置窗口失败");
                    }
                    if unsafe { IsWindow(hwnd) } == 0 {
                        unsafe { PostQuitMessage(0) };
                    } else {
                        unsafe { (*state).settings_open = false };
                    }
                }
                0
            }
            WM_CLOSE => {
                unsafe { DestroyWindow(hwnd) };
                0
            }
            WM_DESTROY => {
                remove_icon(hwnd);
                if !state.is_null() {
                    unsafe { DestroyMenu((*state).menu as _) };
                }
                unsafe { PostQuitMessage(0) };
                0
            }
            WM_NCDESTROY => {
                if !state.is_null() {
                    unsafe {
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                        drop(Box::from_raw(state));
                    }
                }
                unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
            }
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }

    unsafe fn show_menu(hwnd: HWND, menu: isize) {
        let mut point = POINT { x: 0, y: 0 };
        unsafe {
            GetCursorPos(&raw mut point);
            SetForegroundWindow(hwnd);
            TrackPopupMenu(
                menu as _,
                TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                point.x,
                point.y,
                0,
                hwnd,
                null(),
            );
            PostMessageW(hwnd, WM_NULL, 0, 0);
        }
    }

    fn remove_icon(hwnd: HWND) {
        let mut icon: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        icon.cbSize =
            u32::try_from(size_of::<NOTIFYICONDATAW>()).expect("NOTIFYICONDATAW size fits in u32");
        icon.hWnd = hwnd;
        icon.uID = ICON_ID;
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &raw const icon);
        }
    }

    fn open_log_terminal(log_path: &Path) -> Result<Child> {
        Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoExit",
                "-Command",
                "$Host.UI.RawUI.WindowTitle='Jellyfin MPV 日志'; $utf8 = New-Object System.Text.UTF8Encoding($false); [Console]::InputEncoding=$utf8; [Console]::OutputEncoding=$utf8; $OutputEncoding=$utf8; Get-Content -LiteralPath $env:JELLYFIN_MPV_LOG_FILE -Encoding UTF8 -Tail 200 -Wait",
            ])
            .env("JELLYFIN_MPV_LOG_FILE", log_path)
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
            .context("启动 PowerShell 日志终端失败")
    }

    pub(super) fn terminate_processes(processes: &mut Vec<Child>) {
        for mut process in processes.drain(..) {
            if process.try_wait().ok().flatten().is_some() {
                continue;
            }
            match process.kill() {
                Ok(()) => {
                    let _ = process.wait();
                }
                Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {}
                Err(error) => tracing::warn!(%error, pid = process.id(), "关闭日志终端失败"),
            }
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn copy_wide<const N: usize>(target: &mut [u16; N], value: &str) {
        for (target, source) in target
            .iter_mut()
            .take(N.saturating_sub(1))
            .zip(value.encode_utf16())
        {
            *target = source;
        }
    }
}

#[cfg(windows)]
pub use platform::TrayIcon;

#[cfg(all(test, windows))]
mod tests {
    use std::{
        os::windows::process::CommandExt,
        process::Command,
        sync::{Arc, RwLock},
        time::Duration,
    };

    use tokio_util::sync::CancellationToken;

    use super::{TrayIcon, platform::terminate_processes};
    use crate::{
        credential_store::SavedLogin,
        settings_ui::SettingsContext,
        skip_settings::{SkipSettings, SkipSettingsStore},
    };

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    #[test]
    fn creates_and_removes_windows_tray_icon() {
        let shutdown = CancellationToken::new();
        let (connection_updates, _) = tokio::sync::mpsc::unbounded_channel();
        let settings = SettingsContext {
            login: Arc::new(RwLock::new(SavedLogin {
                server: "http://localhost:8096/".to_owned(),
                username: "test".to_owned(),
                password: "test".to_owned(),
            })),
            skip_settings: Arc::new(SkipSettingsStore::for_test(
                std::env::temp_dir().join("jellyfin-mpv-tray-test-settings.json"),
                SkipSettings::default(),
            )),
            connection_updates,
        };
        let tray = TrayIcon::start(
            shutdown.clone(),
            "Jellyfin MPV Cast 测试".to_owned(),
            std::env::temp_dir().join("jellyfin_mpv-test.log"),
            settings,
        )
        .unwrap();
        tray.request_exit();
        for _ in 0..20 {
            if shutdown.is_cancelled() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(shutdown.is_cancelled());
        tray.close();
    }

    #[test]
    fn terminates_tracked_log_processes() {
        let child = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut processes = vec![child];

        terminate_processes(&mut processes);

        assert!(processes.is_empty());
        assert!(
            !powershell_process_exists(pid),
            "被跟踪的日志终端进程仍在运行"
        );
    }

    fn powershell_process_exists(pid: u32) -> bool {
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-Command",
                &format!("if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}"),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .is_ok_and(|status| status.success())
    }
}

#[cfg(not(windows))]
pub struct TrayIcon;

#[cfg(not(windows))]
impl TrayIcon {
    #[allow(clippy::unnecessary_wraps)]
    pub fn start(
        _shutdown: tokio_util::sync::CancellationToken,
        _tooltip: String,
        _log_path: std::path::PathBuf,
        _settings: crate::settings_ui::SettingsContext,
    ) -> anyhow::Result<Self> {
        Ok(Self)
    }

    #[allow(clippy::unused_self)]
    pub const fn close(self) {}
}
