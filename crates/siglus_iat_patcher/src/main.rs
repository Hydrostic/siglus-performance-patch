#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod pe_patch;

#[cfg(windows)]
fn main() {
    gui::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("siglus_iat_patcher is only available on Windows.");
}

#[cfg(windows)]
mod gui {
    use std::cell::RefCell;
    use std::ffi::OsStr;
    use std::mem::{size_of, zeroed};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::COLOR_WINDOW;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BS_PUSHBUTTON, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW,
        ES_AUTOVSCROLL, ES_MULTILINE, ES_READONLY, ES_WANTRETURN, GetMessageW, HMENU, IDC_ARROW,
        LoadCursorW, MB_ICONERROR, MB_ICONINFORMATION, MSG, MessageBoxW, PostQuitMessage,
        RegisterClassW, SW_SHOW, SendMessageW, SetWindowTextW, ShowWindow, TranslateMessage,
        WM_COMMAND, WM_CREATE, WM_DESTROY, WNDCLASSW, WS_BORDER, WS_CHILD, WS_OVERLAPPEDWINDOW,
        WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
    };

    use crate::pe_patch;

    const ID_BROWSE: usize = 1001;
    const ID_PATCH: usize = 1002;
    const ID_UNPATCH: usize = 1003;

    thread_local! {
        static APP: RefCell<Option<AppState>> = const { RefCell::new(None) };
    }

    struct AppState {
        exe_path: Option<PathBuf>,
        path_edit: HWND,
        status_text: HWND,
        log_edit: HWND,
        log: String,
    }

    pub fn run() {
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let class_name = wide("SiglusIatPatcherWindow");
            let window_class = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(window_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: std::ptr::null_mut(),
                hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                hbrBackground: (COLOR_WINDOW + 1) as _,
                lpszMenuName: std::ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };
            RegisterClassW(&window_class);

            let title = wide("Siglus IAT Patcher");
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                720,
                390,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null_mut(),
            );
            if hwnd.is_null() {
                message_box(
                    std::ptr::null_mut(),
                    "Failed to create the Siglus IAT Patcher window.",
                    "Siglus IAT Patcher",
                    MB_ICONERROR,
                );
                return;
            }

            ShowWindow(hwnd, SW_SHOW);

            let mut message: MSG = zeroed();
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_CREATE => {
                create_controls(hwnd);
                0
            }
            WM_COMMAND => {
                let id = wparam & 0xFFFF;
                match id {
                    ID_BROWSE => browse(hwnd),
                    ID_PATCH => run_patch(hwnd),
                    ID_UNPATCH => run_unpatch(hwnd),
                    _ => {}
                }
                0
            }
            WM_DESTROY => {
                APP.with(|app| *app.borrow_mut() = None);
                unsafe {
                    PostQuitMessage(0);
                }
                0
            }
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }

    fn create_controls(hwnd: HWND) {
        unsafe {
            let static_class = wide("STATIC");
            let edit_class = wide("EDIT");
            let button_class = wide("BUTTON");

            create_child(
                hwnd,
                &static_class,
                "Target executable:",
                16,
                18,
                130,
                24,
                0,
                0,
            );
            let path_edit = create_child(
                hwnd,
                &edit_class,
                "",
                150,
                16,
                455,
                24,
                WS_BORDER | ES_READONLY as u32,
                0,
            );
            create_child(
                hwnd,
                &button_class,
                "Browse...",
                615,
                15,
                88,
                28,
                WS_TABSTOP | BS_PUSHBUTTON as u32,
                ID_BROWSE,
            );
            create_child(
                hwnd,
                &button_class,
                "Patch",
                150,
                56,
                120,
                32,
                WS_TABSTOP | BS_PUSHBUTTON as u32,
                ID_PATCH,
            );
            create_child(
                hwnd,
                &button_class,
                "Unpatch",
                282,
                56,
                120,
                32,
                WS_TABSTOP | BS_PUSHBUTTON as u32,
                ID_UNPATCH,
            );
            let status_text = create_child(
                hwnd,
                &static_class,
                "Choose a game executable to inspect.",
                16,
                106,
                686,
                58,
                0,
                0,
            );
            let log_edit = create_child(
                hwnd,
                &edit_class,
                "",
                16,
                175,
                686,
                165,
                WS_BORDER
                    | WS_VSCROLL
                    | ES_MULTILINE as u32
                    | ES_AUTOVSCROLL as u32
                    | ES_READONLY as u32
                    | ES_WANTRETURN as u32,
                0,
            );

            APP.with(|app| {
                *app.borrow_mut() = Some(AppState {
                    exe_path: None,
                    path_edit,
                    status_text,
                    log_edit,
                    log: String::new(),
                });
            });
        }
    }

    unsafe fn create_child(
        parent: HWND,
        class_name: &[u16],
        text: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        extra_style: u32,
        id: usize,
    ) -> HWND {
        let text = wide(text);
        unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                text.as_ptr(),
                WS_CHILD | WS_VISIBLE | extra_style,
                x,
                y,
                width,
                height,
                parent,
                id as HMENU,
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null_mut(),
            )
        }
    }

    fn browse(hwnd: HWND) {
        if let Some(path) = open_exe_dialog(hwnd) {
            APP.with(|app| {
                if let Some(state) = app.borrow_mut().as_mut() {
                    state.exe_path = Some(path.clone());
                    set_text(state.path_edit, &path.display().to_string());
                }
            });
            refresh_status();
        }
    }

    fn run_patch(hwnd: HWND) {
        let exe_path = selected_exe_path();
        let Some(exe_path) = exe_path else {
            show_info(hwnd, "Choose an executable first.");
            return;
        };

        match pe_patch::patch(&exe_path) {
            Ok(()) => {
                append_log(&format!("Patched {}", exe_path.display()));
                show_info(hwnd, "Patch completed.");
            }
            Err(error) => {
                append_log(&format!("Patch failed: {error}"));
                show_error(hwnd, &format!("Patch failed:\n{error}"));
            }
        }
        refresh_status();
    }

    fn run_unpatch(hwnd: HWND) {
        let exe_path = selected_exe_path();
        let Some(exe_path) = exe_path else {
            show_info(hwnd, "Choose an executable first.");
            return;
        };

        match pe_patch::unpatch(&exe_path) {
            Ok(()) => {
                append_log(&format!("Restored {}", exe_path.display()));
                show_info(hwnd, "Unpatch completed.");
            }
            Err(error) => {
                append_log(&format!("Unpatch failed: {error}"));
                show_error(hwnd, &format!("Unpatch failed:\n{error}"));
            }
        }
        refresh_status();
    }

    fn refresh_status() {
        let exe_path = selected_exe_path();
        let Some(exe_path) = exe_path else {
            return;
        };

        let message = match pe_patch::status(&exe_path) {
            Ok(status) => {
                format!(
                    "Selected: {}\n{}\nHook DLL: {} ({})\nBackup: {} ({})",
                    status.exe_path.display(),
                    if status.patched {
                        "Status: patched"
                    } else {
                        "Status: not patched"
                    },
                    status.dll_path.display(),
                    if status.dll_exists {
                        "found"
                    } else {
                        "missing"
                    },
                    status.backup_path.display(),
                    if status.backup_exists {
                        "found"
                    } else {
                        "missing"
                    },
                ) + &format!("\n{}", status.machine_summary())
            }
            Err(error) => format!("Status: failed to inspect executable\n{error}"),
        };

        APP.with(|app| {
            if let Some(state) = app.borrow().as_ref() {
                set_text(state.status_text, &message);
            }
        });
    }

    fn selected_exe_path() -> Option<PathBuf> {
        APP.with(|app| {
            app.borrow()
                .as_ref()
                .and_then(|state| state.exe_path.clone())
        })
    }

    fn append_log(message: &str) {
        APP.with(|app| {
            if let Some(state) = app.borrow_mut().as_mut() {
                state.log.push_str(message);
                state.log.push_str("\r\n");
                set_text(state.log_edit, &state.log);
                let len = state.log.encode_utf16().count();
                unsafe {
                    SendMessageW(state.log_edit, 0x00B1, len, len as LPARAM);
                }
            }
        });
    }

    fn open_exe_dialog(hwnd: HWND) -> Option<PathBuf> {
        unsafe {
            let mut buffer = [0u16; 4096];
            let filter = wide("Executable files (*.exe)\0*.exe\0All files (*.*)\0*.*\0");
            let mut open_file_name: OPENFILENAMEW = zeroed();
            open_file_name.lStructSize = size_of::<OPENFILENAMEW>() as u32;
            open_file_name.hwndOwner = hwnd;
            open_file_name.lpstrFilter = filter.as_ptr();
            open_file_name.lpstrFile = buffer.as_mut_ptr();
            open_file_name.nMaxFile = buffer.len() as u32;
            open_file_name.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST;

            if GetOpenFileNameW(&mut open_file_name) == 0 {
                return None;
            }

            let len = buffer
                .iter()
                .position(|ch| *ch == 0)
                .unwrap_or(buffer.len());
            Some(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len])))
        }
    }

    fn show_info(hwnd: HWND, message: &str) {
        unsafe {
            message_box(hwnd, message, "Siglus IAT Patcher", MB_ICONINFORMATION);
        }
    }

    fn show_error(hwnd: HWND, message: &str) {
        unsafe {
            message_box(hwnd, message, "Siglus IAT Patcher", MB_ICONERROR);
        }
    }

    unsafe fn message_box(hwnd: HWND, message: &str, title: &str, flags: u32) {
        let message = wide(message);
        let title = wide(title);
        unsafe {
            MessageBoxW(hwnd, message.as_ptr(), title.as_ptr(), flags);
        }
    }

    fn set_text(hwnd: HWND, text: &str) {
        unsafe {
            let text = wide(text);
            SetWindowTextW(hwnd, text.as_ptr());
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }
}
