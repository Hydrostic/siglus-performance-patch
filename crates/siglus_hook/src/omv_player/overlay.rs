use std::{
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{
        BeginPaint, DrawTextW, EndPaint, FillRect, GetStockObject, SelectObject, SetBkMode,
        SetTextColor, ANSI_FIXED_FONT, DT_LEFT, DT_NOPREFIX, DT_TOP, DT_WORDBREAK, PAINTSTRUCT,
        TRANSPARENT, WHITE_BRUSH,
    },
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, VK_MENU, VK_X},
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetWindowLongPtrW,
            LoadCursorW, PeekMessageW, PostMessageW, PostQuitMessage, PostThreadMessageW,
            RegisterClassW, SetWindowLongPtrW, ShowWindow, TranslateMessage, CREATESTRUCTW,
            CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, IDC_ARROW, MSG, PM_NOREMOVE,
            SW_HIDE, SW_SHOW, WM_APP, WM_DESTROY, WM_NCCREATE, WM_PAINT, WM_QUIT, WNDCLASSW,
            WS_OVERLAPPEDWINDOW,
        },
    },
};

use crate::omv_player::looper::{LoopQueueStats, LoopQueuesStats};

pub(crate) const ENABLE_STATS_WINDOW: bool = true;

const WINDOW_CLASS_NAME: &str = "SiglusOmvStatsWindow";
const WINDOW_TITLE: &str = "OMV Stats";
const WM_STATS_UPDATED: u32 = WM_APP + 1;
const WM_STATS_VISIBILITY_CHANGED: u32 = WM_APP + 2;

pub(crate) struct StatsWindow {
    state: Arc<StatsWindowState>,
    thread: Option<JoinHandle<()>>,
}

pub(crate) struct OverlayStats {
    pub target_ms: i64,
    pub playback_state: &'static str,
    pub frame_pts_ms: Option<i64>,
    pub frame_duration_ms: Option<i64>,
    pub loop_enabled: bool,
    pub loop_duration_ms: i64,
    pub pitch: usize,
    pub queues: Option<LoopQueuesStats>,
}

struct StatsWindowState {
    text: Mutex<String>,
    hwnd: AtomicIsize,
    thread_id: AtomicU32,
    shutdown: AtomicBool,
    visible: AtomicBool,
    hotkey_was_down: AtomicBool,
}

impl StatsWindow {
    pub(crate) fn new() -> Self {
        let state = Arc::new(StatsWindowState {
            text: Mutex::new("waiting for OMV stats...".to_string()),
            hwnd: AtomicIsize::new(0),
            thread_id: AtomicU32::new(0),
            shutdown: AtomicBool::new(false),
            visible: AtomicBool::new(false),
            hotkey_was_down: AtomicBool::new(false),
        });
        let thread_state = state.clone();
        let thread = std::thread::Builder::new()
            .name("omv-stats-window".to_string())
            .spawn(move || run_window_thread(thread_state))
            .map_err(|error| {
                crate::debug_log(&format!("siglus_hook: stats window thread failed: {error}"));
            })
            .ok();

        Self { state, thread }
    }

    pub(crate) fn update(&self, stats: &OverlayStats) {
        self.update_hotkey();
        if let Ok(mut text) = self.state.text.lock() {
            *text = stats.to_text();
        }

        if !self.state.visible.load(Ordering::Relaxed) {
            return;
        }

        let hwnd = self.state.hwnd.load(Ordering::Relaxed);
        if hwnd != 0 {
            unsafe {
                PostMessageW(hwnd as HWND, WM_STATS_UPDATED, 0, 0);
            }
        }
    }

    fn update_hotkey(&self) {
        let alt_down = key_is_down(VK_MENU);
        let x_down = key_is_down(VK_X);
        let hotkey_down = alt_down && x_down;
        let was_down = self.state.hotkey_was_down.swap(hotkey_down, Ordering::Relaxed);
        if hotkey_down && !was_down {
            let visible = !self.state.visible.load(Ordering::Relaxed);
            self.state.visible.store(visible, Ordering::Relaxed);
            let hwnd = self.state.hwnd.load(Ordering::Relaxed);
            if hwnd != 0 {
                unsafe {
                    PostMessageW(hwnd as HWND, WM_STATS_VISIBILITY_CHANGED, 0, 0);
                }
            }
        }
    }
}

impl Drop for StatsWindow {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::Relaxed);
        let hwnd = self.state.hwnd.load(Ordering::Relaxed);
        let thread_id = self.state.thread_id.load(Ordering::Relaxed);
        if hwnd != 0 {
            unsafe {
                PostMessageW(
                    hwnd as HWND,
                    windows_sys::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                    0,
                    0,
                );
            }
        } else if thread_id != 0 {
            unsafe {
                PostThreadMessageW(thread_id, WM_QUIT, 0, 0);
            }
        }

        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

impl OverlayStats {
    fn to_text(&self) -> String {
        let mut lines = vec![
            "OMV debug".to_string(),
            format!(
                "state={} target={} frame={} dur={}",
                self.playback_state,
                fmt_ms(Some(self.target_ms)),
                fmt_ms(self.frame_pts_ms),
                fmt_ms(self.frame_duration_ms)
            ),
            format!(
                "loop={} duration={} pitch={}",
                if self.loop_enabled { "on" } else { "off" },
                fmt_ms(Some(self.loop_duration_ms)),
                self.pitch
            ),
        ];

        if let Some(queues) = self.queues {
            push_queue_lines(&mut lines, "cur", queues.current);
            if let Some(next) = queues.next {
                push_queue_lines(&mut lines, "next", next);
            } else {
                lines.push("next: none".to_string());
            }
        } else {
            lines.push("queues: locked".to_string());
        }

        lines.join("\r\n")
    }
}

fn run_window_thread(state: Arc<StatsWindowState>) {
    state
        .thread_id
        .store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    let mut queue_probe = unsafe { std::mem::zeroed::<MSG>() };
    unsafe {
        PeekMessageW(&mut queue_probe, ptr::null_mut(), 0, 0, PM_NOREMOVE);
    }

    let class_name = wide_null(WINDOW_CLASS_NAME);
    let window_title = wide_null(WINDOW_TITLE);
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let wnd_class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(stats_window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: ptr::null_mut(),
        hCursor: unsafe { LoadCursorW(ptr::null_mut(), IDC_ARROW) },
        hbrBackground: unsafe { GetStockObject(WHITE_BRUSH) as _ },
        lpszMenuName: ptr::null(),
        lpszClassName: class_name.as_ptr(),
    };

    unsafe {
        RegisterClassW(&wnd_class);
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            520,
            260,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            Arc::as_ptr(&state).cast(),
        );

        if hwnd.is_null() {
            crate::debug_log("siglus_hook: stats window create failed");
            return;
        }

        state.hwnd.store(hwnd as isize, Ordering::Relaxed);
        ShowWindow(
            hwnd,
            if state.visible.load(Ordering::Relaxed) {
                SW_SHOW
            } else {
                SW_HIDE
            },
        );
        if state.shutdown.load(Ordering::Relaxed) {
            PostMessageW(
                hwnd,
                windows_sys::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                0,
                0,
            );
        }

        let mut message = std::mem::zeroed::<MSG>();
        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

unsafe extern "system" fn stats_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create = lparam as *const CREATESTRUCTW;
            if !create.is_null() {
                let state = unsafe { (*create).lpCreateParams as *const StatsWindowState };
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as _);
                }
            }
            1
        }
        WM_STATS_VISIBILITY_CHANGED => {
            let state = window_state(hwnd);
            let visible = state.is_some_and(|state| state.visible.load(Ordering::Relaxed));
            unsafe {
                ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
            }
            0
        }
        WM_STATS_UPDATED => {
            unsafe {
                windows_sys::Win32::Graphics::Gdi::InvalidateRect(hwnd, ptr::null(), 1);
            }
            0
        }
        WM_PAINT => {
            unsafe {
                paint_stats(hwnd);
            }
            0
        }
        WM_DESTROY => {
            let state = window_state(hwnd);
            if let Some(state) = state {
                state.hwnd.store(0, Ordering::Relaxed);
            }
            unsafe {
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

unsafe fn paint_stats(hwnd: HWND) {
    let mut paint = unsafe { std::mem::zeroed::<PAINTSTRUCT>() };
    let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
    unsafe {
        FillRect(hdc, &paint.rcPaint, GetStockObject(WHITE_BRUSH) as _);
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, rgb(20, 20, 20));
        let old_font = SelectObject(hdc, GetStockObject(ANSI_FIXED_FONT));

        let text = window_state(hwnd)
            .and_then(|state| state.text.lock().ok().map(|text| text.clone()))
            .unwrap_or_else(|| "stats window state unavailable".to_string());
        let text = wide_null(&text);
        let mut rect = RECT {
            left: 12,
            top: 12,
            right: 500,
            bottom: 240,
        };
        DrawTextW(
            hdc,
            text.as_ptr(),
            -1,
            &mut rect,
            DT_LEFT | DT_TOP | DT_NOPREFIX | DT_WORDBREAK,
        );

        if !old_font.is_null() {
            SelectObject(hdc, old_font);
        }
        EndPaint(hwnd, &paint);
    }
}

fn window_state(hwnd: HWND) -> Option<&'static StatsWindowState> {
    let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const StatsWindowState;
    if state.is_null() {
        None
    } else {
        Some(unsafe { &*state })
    }
}

fn push_queue_lines(lines: &mut Vec<String>, label: &str, stats: LoopQueueStats) {
    lines.push(format!(
        "{} frames={} {} next={} range={}..{}",
        label,
        stats.frames.len,
        fmt_bytes(stats.frames.cached_bytes),
        fmt_ms(stats.frames.next_pts_ms),
        fmt_ms(stats.frames.first_pts_ms),
        fmt_ms(stats.frames.last_pts_ms)
    ));
    lines.push(format!(
        "{} packets={} {} range={}..{}",
        label,
        stats.packets.len,
        fmt_bytes(stats.packets.cached_bytes),
        fmt_ms(stats.packets.first_pts_ms),
        fmt_ms(stats.packets.last_pts_ms)
    ));
}

fn fmt_ms(value: Option<i64>) -> String {
    value.map_or_else(|| "-".to_string(), |value| format!("{value}ms"))
}

fn fmt_bytes(value: usize) -> String {
    const MIB: f32 = 1024.0 * 1024.0;
    format!("{:.1}MiB", value as f32 / MIB)
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn key_is_down(key: u16) -> bool {
    unsafe { GetAsyncKeyState(i32::from(key)) & i16::MIN != 0 }
}

fn rgb(red: u8, green: u8, blue: u8) -> u32 {
    u32::from(red) | (u32::from(green) << 8) | (u32::from(blue) << 16)
}
