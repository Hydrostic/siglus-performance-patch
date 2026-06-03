use std::{
    ffi::c_void,
    panic::{catch_unwind, AssertUnwindSafe},
    ptr,
};

use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

use crate::debug_log;

use crate::omv_player::OmvPlayer;

type TstrAssignFn = unsafe extern "thiscall" fn(*mut c_void, *const u16, u32) -> i32;
const TSTR_ASSIGN_OFFSET: usize = 0x531E70 - 0x400000;

fn player_from_ptr<'a>(player: *const OmvPlayer) -> Option<&'a OmvPlayer> {
    if player.is_null() {
        None
    } else {
        Some(unsafe { &*player })
    }
}

unsafe fn main_module_offset(offset: usize) -> Option<*mut c_void> {
    let module = unsafe { GetModuleHandleW(ptr::null()) };
    if module.is_null() {
        None
    } else {
        Some((module as usize + offset) as *mut c_void)
    }
}

unsafe fn tstr_str_from_ptr(value: *const c_void) -> Result<String, String> {
    if value.is_null() {
        return Err("path is null".to_string());
    }

    let base = value.cast::<u8>();
    let len = unsafe { ptr::read_unaligned(base.add(16).cast::<u32>()) } as usize;
    let chars = if len <= 7 {
        unsafe { std::slice::from_raw_parts(base.cast::<u16>(), len) }
    } else {
        let heap = unsafe { ptr::read_unaligned(base.cast::<*const u16>()) };
        if heap.is_null() {
            return Err("path heap is null".to_string());
        }
        unsafe { std::slice::from_raw_parts(heap, len) }
    };

    Ok(String::from_utf16_lossy(chars))
}

fn ffi_bool(body: impl FnOnce() -> Result<bool, String>) -> bool {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            debug_log(&format!("siglus_hook: movie player ffi error: {error}"));
            false
        }
        Err(_) => {
            debug_log("siglus_hook: panic inside movie player ffi");
            false
        }
    }
}

macro_rules! catch_panic {
    ($body:block, $default_result:expr) => {
        match catch_unwind(AssertUnwindSafe(|| $body)) {
            Ok(result) => result,
            Err(_) => {
                debug_log(&format!("siglus_hook: panic in omv_hook.rs:{})", line!()));
                return $default_result;
            }
        }
    };
    ($body:block) => {
        catch_panic!($body, ())
    };
}
#[unsafe(no_mangle)]
pub unsafe extern "thiscall" fn spp_movie_player_destroy(player: *mut OmvPlayer) {
    if player.is_null() {
        debug_log("siglus_hook: movie player destroy skipped player=<null>");
        return;
    }
    catch_panic!({
        let boxed_player = unsafe { Box::from_raw(player) };
        std::mem::drop(boxed_player);
    });
    debug_log(&format!(
        "siglus_hook: movie player destroy player=0x{:08X}",
        player as usize,
    ));
}
#[unsafe(no_mangle)]
pub extern "system" fn spp_movie_player_new() -> *mut OmvPlayer {
    catch_panic!({ Box::into_raw(OmvPlayer::new_boxed()) }, ptr::null_mut())
}

#[unsafe(no_mangle)]
pub unsafe extern "thiscall" fn spp_movie_player_init(
    player: *mut OmvPlayer,
    path: *const c_void,
    loop_enabled: i32,
) -> bool {
    if player.is_null() {
        debug_log("siglus_hook: movie player init skipped player=<null>");
        return false;
    }
    catch_panic!(
        {
            let player = unsafe { &mut *player };
            let path = if let Ok(path) = unsafe { tstr_str_from_ptr(path) } {
                path
            } else {
                debug_log("siglus_hook: movie player init failed to read path");
                return false;
            };
            player.init(&path, loop_enabled != 0);
            debug_log(&format!(
                "siglus_hook: movie player init ok path={} loop_enabled={} player=0x{:08X}",
                path, loop_enabled, player as *const OmvPlayer as usize,
            ));
            true
        },
        false
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "thiscall" fn spp_movie_player_get_last_error(
    out_tstr: *mut c_void,
    player: *const OmvPlayer,
) -> bool {
    ffi_bool(|| {
        if out_tstr.is_null() {
            debug_log("siglus_hook: movie player get_last_error skipped out_tstr=<null>");
            return Ok(false);
        }
        if player.is_null() {
            debug_log("siglus_hook: movie player get_last_error skipped player=<null>");
            return Ok(false);
        }
        let player = player_from_ptr(player).ok_or_else(|| "player is null".to_string())?;
        let mut error: Vec<u16> = player.last_error().encode_utf16().collect();
        let len =
            u32::try_from(error.len()).map_err(|_| "last error string is too long".to_string())?;
        error.push(0);
        let assign = unsafe {
            main_module_offset(TSTR_ASSIGN_OFFSET)
                .ok_or_else(|| "GetModuleHandleW(NULL) failed".to_string())?
        };
        let assign: TstrAssignFn = unsafe { std::mem::transmute(assign) };
        unsafe {
            assign(out_tstr, error.as_ptr(), len);
        }

        Ok(true)
    })
}

#[unsafe(no_mangle)]
pub extern "thiscall" fn spp_movie_player_check_error(_player: *const OmvPlayer) -> i32 {
    1
}

#[unsafe(no_mangle)]
pub extern "thiscall" fn spp_movie_player_end_loop(player: *const OmvPlayer) {
    if player.is_null() {
        debug_log("siglus_hook: movie player end_loop skipped player=<null>");
        return;
    }
    catch_panic!({
        let player = player_from_ptr(player).unwrap();
        player.end_loop();
        debug_log(&format!(
            "siglus_hook: movie player end_loop player=0x{:08X}",
            player as *const OmvPlayer as usize,
        ));
    });
}

#[unsafe(no_mangle)]
pub extern "thiscall" fn spp_movie_player_is_playing(player: *const OmvPlayer) -> bool {
    if player.is_null() {
        debug_log("siglus_hook: movie player is_playing skipped player=<null>");
        return false;
    }
    catch_panic!(
        {
            let player = player_from_ptr(player).unwrap();
            player.is_playing()
        },
        false
    )
}

extern "system" fn spp_movie_player_total_time_ms_impl(player: *const OmvPlayer) -> i32 {
    if player.is_null() {
        debug_log("siglus_hook: movie player total_time_ms skipped player=<null>");
        return 0;
    }
    catch_panic!(
        {
            let player = player_from_ptr(player).unwrap();
            let total_time = player.get_total_time() as i32;
            // debug_log(&format!(
            //     "siglus_hook: movie player total_time_ms player=0x{:08X} total_time_ms={}",
            //     player as *const OmvPlayer as usize,
            //     total_time,
            // ));
            total_time
        },
        0
    )
}

#[unsafe(no_mangle)]
#[unsafe(naked)]
pub unsafe extern "thiscall" fn spp_movie_player_total_time_ms(_player: *const OmvPlayer) -> i32 {
    core::arch::naked_asm!(
        "push ecx",
        "call {impl_fn}",
        "mov ecx, eax",
        "ret",
        impl_fn = sym spp_movie_player_total_time_ms_impl,
    );
}

#[unsafe(no_mangle)]
pub unsafe extern "thiscall" fn spp_movie_player_get_size(
    player: *const OmvPlayer,
    out_width: *mut u32,
    out_height: *mut u32,
) -> bool {
    ffi_bool(|| {
        if out_width.is_null() || out_height.is_null() {
            return Err("size output pointer is null".to_string());
        }

        let player = player_from_ptr(player).ok_or_else(|| "player is null".to_string())?;
        let (width, height) = player.get_size();
        unsafe {
            *out_width = width;
            *out_height = height;
        }
        Ok(true)
    })
}

#[unsafe(no_mangle)]
pub extern "fastcall" fn spp_movie_player_seek(player: *const OmvPlayer, target_ms: i32) -> bool {
    ffi_bool(|| {
        let player = player_from_ptr(player).ok_or_else(|| "player is null".to_string())?;
        Ok(player.seek(i64::from(target_ms)))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "thiscall" fn spp_movie_player_fill_buffer(
    player: *const OmvPlayer,
    time_ms: i32,
    buffer: *mut u8,
    pitch: i32,
) -> bool {
    ffi_bool(|| {
        if buffer.is_null() {
            return Err("buffer is null".to_string());
        }
        let player = player_from_ptr(player).ok_or_else(|| "player is null".to_string())?;
        Ok(player.fill_buffer_at(i64::from(time_ms), buffer, pitch))
    })

    // let log_count = FILL_BUFFER_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    // if log_count < 120 {
    //     let state = player.inner.state.lock().unwrap();
    //     debug_log(&format!(
    //         "siglus_hook: fill_buffer player=0x{:08X} time_ms={} result={} pitch={} buffer=0x{:08X} current_frame={} queue_len={} media_position_ms={} status={:?}",
    //         player as *const OmvPlayer as usize,
    //         time_ms,
    //         result,
    //         pitch,
    //         buffer as usize,
    //         state.current_frame.is_some(),
    //         player.inner.queue.len(),
    //         state.media_position_ms,
    //         state.status,
    //     ));
    // }
}
#[unsafe(no_mangle)]
pub extern "thiscall" fn spp_movie_player_is_rgb(player: *const OmvPlayer) -> i32 {
    if player.is_null() {
        debug_log("siglus_hook: movie player is_rgb skipped player=<null>");
        return 0;
    }
    let player = player_from_ptr(player).unwrap();
    if player.is_rgb() {
        0
    } else {
        1
    }
}
#[unsafe(no_mangle)]
pub extern "thiscall" fn spp_movie_player_check_need_update(
    player: *const OmvPlayer,
    time_ms: i32,
    update_by_force: i32,
) -> bool {
    if player.is_null() {
        debug_log("siglus_hook: movie player check_need_update skipped player=<null>");
        return false;
    }
    let player = player_from_ptr(player).unwrap();
    player.check_need_update_at(i64::from(time_ms), update_by_force != 0)
    // ffi_bool(|| {
    //     let player = player_from_ptr(player).ok_or_else(|| "player is null".to_string())?;
    //     let result = player
    //         .check_need_update_at(i64::from(time_ms), update_by_force != 0);

    // let log_count = CHECK_NEED_UPDATE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    // if log_count < 120 {
    //     let state = player.inner.state.lock().unwrap();
    //     debug_log(&format!(
    //         "siglus_hook: check_need_update player=0x{:08X} time_ms={} force={} result={} status={:?} current_frame={} queue_len={} media_position_ms={} pending_seek_ms={} error={}",
    //         player as *const OmvPlayer as usize,
    //         time_ms,
    //         update_by_force,
    //         result,
    //         state.status,
    //         state.current_frame.is_some(),
    //         player.inner.queue.len(),
    //         state.media_position_ms,
    //         state.pending_seek_ms
    //             .map(|value| value.to_string())
    //             .unwrap_or_else(|| "none".to_string()),
    //         state.error.as_deref().unwrap_or(""),
    //     ));
    // }

    //     Ok(result)
    // })
}
