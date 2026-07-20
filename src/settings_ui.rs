use std::sync::{Arc, RwLock};

use anyhow::Result;
use tokio::sync::mpsc;

use crate::{credential_store::SavedLogin, skip_settings::SkipSettingsStore};

#[derive(Clone)]
pub struct SettingsContext {
    pub login: Arc<RwLock<SavedLogin>>,
    pub skip_settings: Arc<SkipSettingsStore>,
    pub connection_updates: mpsc::UnboundedSender<SavedLogin>,
}

pub fn show_settings(context: SettingsContext) -> Result<()> {
    platform::show(context)
}

#[cfg(windows)]
mod platform {
    use std::{mem::size_of, ptr::null_mut};

    use anyhow::{Result, bail};
    use windows_sys::Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::{COLOR_WINDOW, DEFAULT_GUI_FONT, GetStockObject, HBRUSH, UpdateWindow},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, BS_PUSHBUTTON,
            CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
            ES_AUTOHSCROLL, ES_PASSWORD, GWLP_USERDATA, GetMessageW, GetSystemMetrics,
            GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, IDC_ARROW, IsDialogMessageW,
            IsWindow, LoadCursorW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MSG, MessageBoxW,
            RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, SendMessageW, SetForegroundWindow,
            SetWindowLongPtrW, ShowWindow, TranslateMessage, WM_CLOSE, WM_COMMAND, WM_NCCREATE,
            WM_SETFONT, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_OVERLAPPED, WS_SYSMENU,
            WS_TABSTOP, WS_VISIBLE,
        },
    };

    use super::SettingsContext;
    use crate::{
        credential_store::{SavedLogin, save_login},
        login_ui::validate_input,
        skip_settings::SkipSettings,
    };

    const WINDOW_CLASS: &str = "JellyfinMpvSettingsWindow";
    const ID_SERVER: usize = 1101;
    const ID_USERNAME: usize = 1102;
    const ID_PASSWORD: usize = 1103;
    const ID_INTRO: usize = 1201;
    const ID_OUTRO: usize = 1202;
    const ID_RECAP: usize = 1203;
    const ID_PREVIEW: usize = 1204;
    const ID_COMMERCIAL: usize = 1205;
    const ID_SAVE: usize = 1;
    const ID_CANCEL: usize = 2;
    const BST_UNCHECKED: u32 = 0;
    const BST_CHECKED: u32 = 1;

    struct SettingsState {
        context: SettingsContext,
        server: HWND,
        username: HWND,
        password: HWND,
        intro: HWND,
        outro: HWND,
        recap: HWND,
        preview: HWND,
        commercial: HWND,
    }

    impl SettingsState {
        fn new(context: SettingsContext) -> Self {
            Self {
                context,
                server: null_mut(),
                username: null_mut(),
                password: null_mut(),
                intro: null_mut(),
                outro: null_mut(),
                recap: null_mut(),
                preview: null_mut(),
                commercial: null_mut(),
            }
        }
    }

    pub fn show(context: SettingsContext) -> Result<()> {
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            if instance.is_null() {
                bail!("无法获取程序模块句柄");
            }

            let class_name = wide(WINDOW_CLASS);
            let window_class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: (COLOR_WINDOW + 1) as usize as HBRUSH,
                lpszClassName: class_name.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&window_class);

            let state = Box::new(SettingsState::new(context));
            let state_ptr = Box::into_raw(state);
            let title = wide("Jellyfin MPV 设置");
            let width = 520;
            let height = 430;
            let x = (GetSystemMetrics(SM_CXSCREEN) - width) / 2;
            let y = (GetSystemMetrics(SM_CYSCREEN) - height) / 2;
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
                x.max(0),
                y.max(0),
                width,
                height,
                null_mut(),
                null_mut(),
                instance,
                state_ptr.cast(),
            );
            if hwnd.is_null() {
                drop(Box::from_raw(state_ptr));
                bail!("无法创建设置窗口");
            }

            if let Err(error) = create_controls(hwnd, instance, &mut *state_ptr) {
                DestroyWindow(hwnd);
                drop(Box::from_raw(state_ptr));
                return Err(error);
            }

            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
            SetForegroundWindow(hwnd);

            let mut message: MSG = std::mem::zeroed();
            while IsWindow(hwnd) != 0 {
                let status = GetMessageW(&mut message, null_mut(), 0, 0);
                if status == -1 {
                    DestroyWindow(hwnd);
                    drop(Box::from_raw(state_ptr));
                    bail!("读取设置窗口消息失败");
                }
                if status == 0 {
                    break;
                }
                if IsDialogMessageW(hwnd, &message) == 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }

            if IsWindow(hwnd) != 0 {
                DestroyWindow(hwnd);
            }
            drop(Box::from_raw(state_ptr));
            Ok(())
        }
    }

    unsafe fn create_controls(
        hwnd: HWND,
        instance: HINSTANCE,
        state: &mut SettingsState,
    ) -> Result<()> {
        let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
        let login = state
            .context
            .login
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let skips = state.context.skip_settings.snapshot();

        unsafe { create_label(hwnd, instance, "服务器地址", 24, 24, 88, 22, font) }?;
        state.server = unsafe {
            create_edit(
                hwnd,
                instance,
                ID_SERVER,
                &login.server,
                118,
                20,
                370,
                false,
                font,
            )
        }?;
        unsafe { create_label(hwnd, instance, "用户名", 24, 68, 88, 22, font) }?;
        state.username = unsafe {
            create_edit(
                hwnd,
                instance,
                ID_USERNAME,
                &login.username,
                118,
                64,
                370,
                false,
                font,
            )
        }?;
        unsafe { create_label(hwnd, instance, "密码", 24, 112, 88, 22, font) }?;
        state.password = unsafe {
            create_edit(
                hwnd,
                instance,
                ID_PASSWORD,
                &login.password,
                118,
                108,
                370,
                true,
                font,
            )
        }?;

        unsafe {
            create_label(
                hwnd,
                instance,
                "自动跳过以下 Jellyfin 媒体分段：",
                24,
                158,
                350,
                24,
                font,
            )
        }?;
        state.intro = unsafe {
            create_checkbox(
                hwnd,
                instance,
                ID_INTRO,
                "片头 (Intro)",
                34,
                192,
                skips.intro,
                font,
            )
        }?;
        state.outro = unsafe {
            create_checkbox(
                hwnd,
                instance,
                ID_OUTRO,
                "片尾 (Outro)",
                264,
                192,
                skips.outro,
                font,
            )
        }?;
        state.recap = unsafe {
            create_checkbox(
                hwnd,
                instance,
                ID_RECAP,
                "前情回顾 (Recap)",
                34,
                228,
                skips.recap,
                font,
            )
        }?;
        state.preview = unsafe {
            create_checkbox(
                hwnd,
                instance,
                ID_PREVIEW,
                "预告 (Preview)",
                264,
                228,
                skips.preview,
                font,
            )
        }?;
        state.commercial = unsafe {
            create_checkbox(
                hwnd,
                instance,
                ID_COMMERCIAL,
                "广告 (Commercial)",
                34,
                264,
                skips.commercial,
                font,
            )
        }?;

        unsafe {
            create_label(
                hwnd,
                instance,
                "服务器或登录信息发生变化时，当前播放会停止并重新连接。",
                24,
                310,
                464,
                22,
                font,
            )
        }?;
        unsafe {
            create_button(
                hwnd,
                instance,
                ID_SAVE,
                "保存",
                304,
                346,
                BS_DEFPUSHBUTTON as u32,
                font,
            )
        }?;
        unsafe {
            create_button(
                hwnd,
                instance,
                ID_CANCEL,
                "取消",
                408,
                346,
                BS_PUSHBUTTON as u32,
                font,
            )
        }?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn create_label(
        parent: HWND,
        instance: HINSTANCE,
        text: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        font: *mut core::ffi::c_void,
    ) -> Result<HWND> {
        unsafe {
            create_control(
                parent,
                instance,
                "STATIC",
                text,
                WS_CHILD | WS_VISIBLE,
                x,
                y,
                width,
                height,
                0,
                font,
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn create_edit(
        parent: HWND,
        instance: HINSTANCE,
        id: usize,
        text: &str,
        x: i32,
        y: i32,
        width: i32,
        password: bool,
        font: *mut core::ffi::c_void,
    ) -> Result<HWND> {
        let mut style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32;
        if password {
            style |= ES_PASSWORD as u32;
        }
        unsafe {
            create_control(
                parent, instance, "EDIT", text, style, x, y, width, 26, id, font,
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn create_checkbox(
        parent: HWND,
        instance: HINSTANCE,
        id: usize,
        text: &str,
        x: i32,
        y: i32,
        checked: bool,
        font: *mut core::ffi::c_void,
    ) -> Result<HWND> {
        let checkbox = unsafe {
            create_control(
                parent,
                instance,
                "BUTTON",
                text,
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                x,
                y,
                210,
                26,
                id,
                font,
            )
        }?;
        unsafe {
            SendMessageW(
                checkbox,
                BM_SETCHECK,
                if checked { BST_CHECKED } else { BST_UNCHECKED } as WPARAM,
                0,
            );
        }
        Ok(checkbox)
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn create_button(
        parent: HWND,
        instance: HINSTANCE,
        id: usize,
        text: &str,
        x: i32,
        y: i32,
        button_style: u32,
        font: *mut core::ffi::c_void,
    ) -> Result<HWND> {
        unsafe {
            create_control(
                parent,
                instance,
                "BUTTON",
                text,
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | button_style,
                x,
                y,
                80,
                30,
                id,
                font,
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn create_control(
        parent: HWND,
        instance: HINSTANCE,
        class: &str,
        text: &str,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        id: usize,
        font: *mut core::ffi::c_void,
    ) -> Result<HWND> {
        let class = wide(class);
        let text = wide(text);
        let control = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                text.as_ptr(),
                style,
                x,
                y,
                width,
                height,
                parent,
                id as *mut core::ffi::c_void,
                instance,
                null_mut(),
            )
        };
        if control.is_null() {
            bail!("无法创建设置窗口控件");
        }
        unsafe { SendMessageW(control, WM_SETFONT, font as WPARAM, 1) };
        Ok(control)
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_NCCREATE {
            let create = lparam as *const CREATESTRUCTW;
            if !create.is_null() {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*create).lpCreateParams as isize);
                }
            }
        }

        let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState };
        match message {
            WM_COMMAND if !state.is_null() => {
                let id = wparam & 0xffff;
                if id == ID_SAVE {
                    unsafe { save(hwnd, &mut *state) };
                    return 0;
                }
                if id == ID_CANCEL {
                    unsafe { DestroyWindow(hwnd) };
                    return 0;
                }
            }
            WM_CLOSE => {
                unsafe { DestroyWindow(hwnd) };
                return 0;
            }
            _ => {}
        }
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    unsafe fn save(hwnd: HWND, state: &mut SettingsState) {
        let input = match validate_input(
            unsafe { control_text(state.server) },
            unsafe { control_text(state.username) },
            unsafe { control_text(state.password) },
        ) {
            Ok(input) => input,
            Err(error) => {
                unsafe { show_error(hwnd, &error.to_string()) };
                return;
            }
        };
        let login = SavedLogin {
            server: input.server.to_string(),
            username: input.username,
            password: input.password,
        };
        let skips = SkipSettings {
            intro: unsafe { is_checked(state.intro) },
            outro: unsafe { is_checked(state.outro) },
            recap: unsafe { is_checked(state.recap) },
            preview: unsafe { is_checked(state.preview) },
            commercial: unsafe { is_checked(state.commercial) },
        };

        if let Err(error) = save_login(&login) {
            unsafe { show_error(hwnd, &format!("保存服务器登录信息失败：{error}")) };
            return;
        }
        if let Err(error) = state.context.skip_settings.update(skips) {
            unsafe { show_error(hwnd, &format!("保存跳过设置失败：{error}")) };
            return;
        }

        let previous = {
            let mut current = state
                .context
                .login
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous = current.clone();
            *current = login.clone();
            previous
        };
        let connection_changed = login != previous;

        let text = wide(if connection_changed {
            "设置已保存，正在重新连接 Jellyfin。"
        } else {
            "设置已保存。"
        });
        let title = wide("Jellyfin MPV 设置");
        unsafe {
            MessageBoxW(
                hwnd,
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONINFORMATION,
            );
            DestroyWindow(hwnd);
        }
        if connection_changed {
            let _ = state.context.connection_updates.send(login);
        }
    }

    unsafe fn show_error(hwnd: HWND, message: &str) {
        let text = wide(message);
        let title = wide("设置有误");
        unsafe {
            MessageBoxW(hwnd, text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
        }
    }

    unsafe fn is_checked(hwnd: HWND) -> bool {
        unsafe { SendMessageW(hwnd, BM_GETCHECK, 0, 0) as u32 == BST_CHECKED }
    }

    unsafe fn control_text(hwnd: HWND) -> String {
        let length = unsafe { GetWindowTextLengthW(hwnd) };
        let mut buffer = vec![0u16; length.max(0) as usize + 1];
        let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
        String::from_utf16_lossy(&buffer[..copied.max(0) as usize])
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    const _: () = assert!(size_of::<WPARAM>() == size_of::<usize>());
}

#[cfg(not(windows))]
mod platform {
    use anyhow::{Result, bail};

    use super::SettingsContext;

    pub fn show(_context: SettingsContext) -> Result<()> {
        bail!("当前平台没有设置窗口")
    }
}
