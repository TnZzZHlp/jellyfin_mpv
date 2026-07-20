use anyhow::{Result, bail};
use url::Url;

pub struct LoginDefaults {
    pub server: String,
    pub username: String,
    pub password: String,
}

pub struct LoginInput {
    pub server: Url,
    pub username: String,
    pub password: String,
}

pub fn prompt_login(defaults: LoginDefaults) -> Result<Option<LoginInput>> {
    platform::prompt(defaults)
}

pub fn show_error_message(message: &str) {
    platform::show_error(message);
}

pub(crate) fn validate_input(
    server: String,
    username: String,
    password: String,
) -> Result<LoginInput> {
    let server_text = server.trim();
    if server_text.is_empty() {
        bail!("请输入 Jellyfin 地址");
    }
    let server = Url::parse(server_text).map_err(|_| anyhow::anyhow!("Jellyfin 地址格式不正确"))?;
    if !matches!(server.scheme(), "http" | "https") {
        bail!("Jellyfin 地址必须使用 http 或 https");
    }

    let username = username.trim().to_owned();
    if username.is_empty() {
        bail!("请输入用户名");
    }
    if password.is_empty() {
        bail!("请输入密码");
    }

    Ok(LoginInput {
        server,
        username,
        password,
    })
}

#[cfg(windows)]
mod platform {
    use std::{
        mem::size_of,
        ptr::{null, null_mut},
    };

    use anyhow::{Result, bail};
    use windows_sys::Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::{COLOR_WINDOW, DEFAULT_GUI_FONT, GetStockObject, HBRUSH, UpdateWindow},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_RETURN},
            WindowsAndMessaging::{
                BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CREATESTRUCTW, CreateWindowExW, DefWindowProcW,
                DestroyWindow, DispatchMessageW, ES_AUTOHSCROLL, ES_PASSWORD, GWLP_USERDATA,
                GetMessageW, GetSystemMetrics, GetWindowLongPtrW, GetWindowTextLengthW,
                GetWindowTextW, IDC_ARROW, IsDialogMessageW, LoadCursorW, MB_ICONWARNING, MB_OK,
                MSG, MessageBoxW, PostQuitMessage, RegisterClassW, SM_CXSCREEN, SM_CYSCREEN,
                SW_SHOW, SendMessageW, SetWindowLongPtrW, ShowWindow, TranslateMessage, WM_CLOSE,
                WM_COMMAND, WM_DESTROY, WM_NCCREATE, WM_SETFONT, WNDCLASSW, WS_BORDER, WS_CAPTION,
                WS_CHILD, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
            },
        },
    };

    use super::{LoginDefaults, LoginInput, validate_input};

    const WINDOW_CLASS: &str = "JellyfinMpvLoginWindow";
    const ID_SERVER: usize = 1001;
    const ID_USERNAME: usize = 1002;
    const ID_PASSWORD: usize = 1003;
    const ID_CONNECT: usize = 1;
    const ID_CANCEL: usize = 2;

    struct LoginState {
        defaults: LoginDefaults,
        server: HWND,
        username: HWND,
        password: HWND,
        result: Option<LoginInput>,
    }

    impl LoginState {
        fn new(defaults: LoginDefaults) -> Self {
            Self {
                defaults,
                server: null_mut(),
                username: null_mut(),
                password: null_mut(),
                result: None,
            }
        }
    }

    pub fn prompt(defaults: LoginDefaults) -> Result<Option<LoginInput>> {
        unsafe {
            let instance = GetModuleHandleW(null());
            if instance.is_null() {
                bail!("无法获取程序模块句柄");
            }

            let class_name = wide(WINDOW_CLASS);
            let window_class = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(window_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: null_mut(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: (COLOR_WINDOW + 1) as usize as HBRUSH,
                lpszMenuName: null(),
                lpszClassName: class_name.as_ptr(),
            };
            RegisterClassW(&window_class);

            let state = Box::new(LoginState::new(defaults));
            let state_ptr = Box::into_raw(state);
            let title = wide("Jellyfin MPV 登录");
            let width = 460;
            let height = 282;
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
                bail!("无法创建登录窗口");
            }

            if let Err(error) = create_controls(hwnd, instance, &mut *state_ptr) {
                DestroyWindow(hwnd);
                drop(Box::from_raw(state_ptr));
                return Err(error);
            }

            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
            SetFocus((*state_ptr).server);

            let mut message: MSG = std::mem::zeroed();
            loop {
                let status = GetMessageW(&mut message, null_mut(), 0, 0);
                if status == -1 {
                    DestroyWindow(hwnd);
                    drop(Box::from_raw(state_ptr));
                    bail!("读取登录窗口消息失败");
                }
                if status == 0 {
                    break;
                }
                if message.message == windows_sys::Win32::UI::WindowsAndMessaging::WM_KEYDOWN {
                    if message.wParam == VK_RETURN as usize {
                        submit(hwnd, &mut *state_ptr);
                        continue;
                    }
                    if message.wParam == VK_ESCAPE as usize {
                        DestroyWindow(hwnd);
                        continue;
                    }
                }
                if IsDialogMessageW(hwnd, &message) == 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }

            let state = Box::from_raw(state_ptr);
            Ok(state.result)
        }
    }

    pub fn show_error(message: &str) {
        let text = wide(message);
        let title = wide("Jellyfin MPV 错误");
        unsafe {
            MessageBoxW(
                null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
            );
        }
    }

    unsafe fn create_controls(
        hwnd: HWND,
        instance: HINSTANCE,
        state: &mut LoginState,
    ) -> Result<()> {
        let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };

        unsafe { create_label(hwnd, instance, "服务器地址", 22, 24, 82, 22, font) }?;
        state.server = unsafe {
            create_edit(
                hwnd,
                instance,
                ID_SERVER,
                &state.defaults.server,
                110,
                20,
                320,
                false,
                font,
            )
        }?;

        unsafe { create_label(hwnd, instance, "用户名", 22, 72, 82, 22, font) }?;
        state.username = unsafe {
            create_edit(
                hwnd,
                instance,
                ID_USERNAME,
                &state.defaults.username,
                110,
                68,
                320,
                false,
                font,
            )
        }?;

        unsafe { create_label(hwnd, instance, "密码", 22, 120, 82, 22, font) }?;
        state.password = unsafe {
            create_edit(
                hwnd,
                instance,
                ID_PASSWORD,
                &state.defaults.password,
                110,
                116,
                320,
                true,
                font,
            )
        }?;

        unsafe {
            create_button(
                hwnd,
                instance,
                ID_CONNECT,
                "连接",
                254,
                166,
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
                350,
                166,
                BS_PUSHBUTTON as u32,
                font,
            )
        }?;
        unsafe {
            create_label(
                hwnd,
                instance,
                "登录成功后将保存到 Windows 凭据管理器",
                22,
                210,
                408,
                22,
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
        let class = wide("STATIC");
        let text = wide(text);
        unsafe {
            create_control(
                parent,
                instance,
                class.as_ptr(),
                text.as_ptr(),
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
        let class = wide("EDIT");
        let text = wide(text);
        let mut style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32;
        if password {
            style |= ES_PASSWORD as u32;
        }
        unsafe {
            create_control(
                parent,
                instance,
                class.as_ptr(),
                text.as_ptr(),
                style,
                x,
                y,
                width,
                26,
                id,
                font,
            )
        }
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
        let class = wide("BUTTON");
        let text = wide(text);
        unsafe {
            create_control(
                parent,
                instance,
                class.as_ptr(),
                text.as_ptr(),
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
        class: *const u16,
        text: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        id: usize,
        font: *mut core::ffi::c_void,
    ) -> Result<HWND> {
        let control = unsafe {
            CreateWindowExW(
                0,
                class,
                text,
                style,
                x,
                y,
                width,
                height,
                parent,
                id as *mut core::ffi::c_void,
                instance,
                null(),
            )
        };
        if control.is_null() {
            bail!("无法创建登录窗口控件");
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

        let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut LoginState };
        match message {
            WM_COMMAND if !state_ptr.is_null() => {
                let id = wparam & 0xffff;
                if id == ID_CONNECT {
                    unsafe { submit(hwnd, &mut *state_ptr) };
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
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                return 0;
            }
            _ => {}
        }
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    unsafe fn submit(hwnd: HWND, state: &mut LoginState) {
        let server = unsafe { control_text(state.server) };
        let username = unsafe { control_text(state.username) };
        let password = unsafe { control_text(state.password) };
        match validate_input(server, username, password) {
            Ok(input) => {
                state.result = Some(input);
                unsafe { DestroyWindow(hwnd) };
            }
            Err(error) => {
                let text = wide(&error.to_string());
                let title = wide("输入有误");
                unsafe {
                    MessageBoxW(hwnd, text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONWARNING);
                }
            }
        }
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

    use super::{LoginDefaults, LoginInput};

    pub fn prompt(_defaults: LoginDefaults) -> Result<Option<LoginInput>> {
        bail!("当前平台没有登录窗口，请提供 --server、--username 和 --password")
    }

    pub fn show_error(message: &str) {
        eprintln!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::validate_input;

    #[test]
    fn accepts_valid_login_input() {
        let input = validate_input(
            " http://localhost:8096 ".to_owned(),
            " alice ".to_owned(),
            "secret".to_owned(),
        )
        .unwrap();
        assert_eq!(input.server.as_str(), "http://localhost:8096/");
        assert_eq!(input.username, "alice");
        assert_eq!(input.password, "secret");
    }

    #[test]
    fn rejects_invalid_login_input() {
        assert!(
            validate_input("ftp://localhost".to_owned(), "a".to_owned(), "p".to_owned()).is_err()
        );
        assert!(
            validate_input(
                "http://localhost".to_owned(),
                " ".to_owned(),
                "p".to_owned()
            )
            .is_err()
        );
        assert!(
            validate_input("http://localhost".to_owned(), "a".to_owned(), String::new()).is_err()
        );
    }
}
