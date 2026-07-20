#[cfg(windows)]
mod platform {
    use std::{
        mem::size_of,
        os::windows::process::CommandExt,
        path::{Path, PathBuf},
        process::Command,
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
                GetMessageW, GetWindowLongPtrW, HWND_MESSAGE, IDI_APPLICATION, LoadIconW,
                MF_DISABLED, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage,
                RegisterClassW, SW_HIDE, SetForegroundWindow, SetWindowLongPtrW, ShowWindow,
                TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP,
                WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_LBUTTONUP, WM_NCCREATE, WM_NCDESTROY, WM_NULL,
                WM_RBUTTONUP, WNDCLASSW,
            },
        },
    };

    const WINDOW_CLASS: &str = "JellyfinMpvTrayWindow";
    const WM_TRAY: u32 = WM_APP + 1;
    const MENU_STATUS: usize = 1000;
    const MENU_OPEN_LOG: usize = 1001;
    const MENU_EXIT: usize = 1002;
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
    }

    impl TrayIcon {
        pub fn start(
            shutdown: CancellationToken,
            tooltip: String,
            log_path: PathBuf,
        ) -> Result<Self> {
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let thread = thread::Builder::new()
                .name("jellyfin-mpv-tray".to_owned())
                .spawn(move || run_message_loop(shutdown, tooltip, log_path, ready_tx))
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
        tooltip: String,
        log_path: PathBuf,
        ready: mpsc::SyncSender<Result<isize, String>>,
    ) {
        match unsafe { create_tray_window(shutdown, &tooltip, log_path) } {
            Ok(hwnd) => {
                let _ = ready.send(Ok(hwnd as isize));
                unsafe {
                    let mut message: MSG = std::mem::zeroed();
                    while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
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
        unsafe { RegisterClassW(&window_class) };

        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            bail!("创建托盘菜单失败");
        }
        let status = wide("Jellyfin MPV Cast 已连接");
        let open_log = wide("打开日志终端");
        let exit = wide("退出");
        unsafe {
            AppendMenuW(menu, MF_STRING | MF_DISABLED, MENU_STATUS, status.as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(menu, MF_STRING, MENU_OPEN_LOG, open_log.as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(menu, MF_STRING, MENU_EXIT, exit.as_ptr());
        }

        let state = Box::new(TrayState {
            shutdown,
            menu: menu as isize,
            log_path,
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
        icon.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        icon.hWnd = hwnd;
        icon.uID = ICON_ID;
        icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        icon.uCallbackMessage = WM_TRAY;
        icon.hIcon = unsafe { LoadIconW(null_mut(), IDI_APPLICATION) };
        copy_wide(&mut icon.szTip, tooltip);
        if unsafe { Shell_NotifyIconW(NIM_ADD, &icon) } == 0 {
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
            let state = unsafe { (*create).lpCreateParams as *mut TrayState };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };
            return 1;
        }

        let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState };
        match message {
            WM_TRAY if lparam as u32 == WM_RBUTTONUP || lparam as u32 == WM_LBUTTONUP => {
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
                if !state.is_null()
                    && let Err(error) = unsafe { open_log_terminal(&(*state).log_path) }
                {
                    tracing::error!(%error, "打开日志终端失败");
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
            GetCursorPos(&mut point);
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
        icon.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        icon.hWnd = hwnd;
        icon.uID = ICON_ID;
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &icon);
        }
    }

    fn open_log_terminal(log_path: &Path) -> Result<()> {
        Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoExit",
                "-Command",
                "$Host.UI.RawUI.WindowTitle='Jellyfin MPV 日志'; Get-Content -LiteralPath $env:JELLYFIN_MPV_LOG_FILE -Tail 200 -Wait",
            ])
            .env("JELLYFIN_MPV_LOG_FILE", log_path)
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
            .context("启动 PowerShell 日志终端失败")?;
        Ok(())
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
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::TrayIcon;

    #[test]
    fn creates_and_removes_windows_tray_icon() {
        let shutdown = CancellationToken::new();
        let tray = TrayIcon::start(
            shutdown.clone(),
            "Jellyfin MPV Cast 测试".to_owned(),
            std::env::temp_dir().join("jellyfin_mpv-test.log"),
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
}

#[cfg(not(windows))]
pub struct TrayIcon;

#[cfg(not(windows))]
impl TrayIcon {
    pub fn start(
        _shutdown: tokio_util::sync::CancellationToken,
        _tooltip: String,
        _log_path: std::path::PathBuf,
    ) -> anyhow::Result<Self> {
        Ok(Self)
    }

    pub fn close(self) {}
}
