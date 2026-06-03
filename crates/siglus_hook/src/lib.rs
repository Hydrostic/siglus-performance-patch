#![cfg(windows)]
// mod movie_player;
mod omv_types;
mod omv_player;
mod omv_hook;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::File;
use std::io::BufWriter;
use std::os::windows::fs::MetadataExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;

use minhook::{MH_STATUS, MinHook};
use windows_sys::Win32::Foundation::{BOOL, ERROR_FILE_NOT_FOUND, HINSTANCE, SetLastError, TRUE};
#[allow(unused_imports)]
use windows_sys::Win32::Storage::FileSystem::{
    CopyFileW, GetFileAttributesW, INVALID_FILE_ATTRIBUTES, MOVEFILE_REPLACE_EXISTING, MoveFileExW,
    MoveFileW,
};
use windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows_sys::Win32::System::LibraryLoader::{
    DisableThreadLibraryCalls, GetModuleFileNameW, GetModuleHandleW,
};
use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_sys::Win32::System::Threading::CreateThread;
use windows_sys::core::PCWSTR;

type GetFileAttributesWFn = unsafe extern "system" fn(PCWSTR) -> u32;
type CopyFileWFn = unsafe extern "system" fn(PCWSTR, PCWSTR, BOOL) -> BOOL;
type TnmSaveToFileFn = unsafe extern "fastcall" fn(*const c_void, *const c_void) -> bool;
type TnmPackBufferFn = unsafe extern "fastcall" fn(*const c_void) -> bool;
type TnmCreatePngFromTextureAndSaveToFileFn =
    unsafe extern "fastcall" fn(*const c_void, i32, i32, *const c_void, u32) -> bool;
type ArrayAllocFn = unsafe extern "cdecl" fn(u32) -> u32;
type ArrayReplaceStorageFn = unsafe extern "thiscall" fn(*mut c_void, u32, u32, u32) -> i32;

const TNM_SAVE_TO_FILE_OFFSET: usize = 0x25DE60;
const TNM_PACK_BUFFER_OFFSET: usize = 0x25E120;
const TNM_CREATE_PNG_FROM_TEXTURE_AND_SAVE_TO_FILE_OFFSET: usize = 0x2389F0;
const MOVIE_END_LOOP_PATCH_OFFSET: usize = 0x55C329 - 0x400000;
const MOVIE_IS_PLAYING_ALT_PATCH_OFFSET: usize = 0x5FB8E9 - 0x400000;
const MOVIE_IS_PLAYING_ALT_JNZ_PATCH_OFFSET: usize = 0x5FB8FB - 0x400000;
const MOVIE_IS_PLAYING_PATCH_OFFSET: usize = 0x6035BA - 0x400000;
const MOVIE_IS_PLAYING_JZ_PATCH_OFFSET: usize = 0x6035CC - 0x400000;
const MOVIE_SEEK_NOP_PATCH_OFFSET: usize = 0x55C29A - 0x400000;
const MOVIE_DESTROY_PATCH_OFFSET: usize = 0x6F2523 - 0x400000;
const MOVIE_RESTRUCT_NEW_PATCH_OFFSET: usize = 0x602DAA - 0x400000;
const MOVIE_RESTRUCT_NEW_NOP_1_OFFSET: usize = 0x602EC1 - 0x400000;
const MOVIE_RESTRUCT_NEW_STORE_PATCH_OFFSET: usize = 0x602ECC - 0x400000;
const MOVIE_RESTRUCT_NEW_NOP_2_OFFSET: usize = 0x602F77 - 0x400000;
const MOVIE_RESTRUCT_NEW_NOP_3_OFFSET: usize = 0x602F7E - 0x400000;
const MOVIE_RESTRUCT_INIT_NOP_PRE_OFFSET: usize = 0x602FB9 - 0x400000;
const MOVIE_RESTRUCT_INIT_NOP_OFFSET: usize = 0x602FBE - 0x400000;
const MOVIE_RESTRUCT_INIT_CALL_OFFSET: usize = 0x602FC7 - 0x400000;
const MOVIE_RESTRUCT_LAST_ERROR_CALL_OFFSET: usize = 0x602FDF - 0x400000;
const MOVIE_RESTRUCT_GET_SIZE_PATCH_OFFSET: usize = 0x60308F - 0x400000;
const MOVIE_RESTRUCT_GET_SIZE_NOP_OFFSET: usize = 0x6030BE - 0x400000;
const MOVIE_IS_RGB_PATCH_OFFSET: usize = 0x5FE40D - 0x400000;
const MOVIE_TOTAL_TIME_PATCH_OFFSET: usize = 0x6033E4 - 0x400000;
const MOVIE_CHECK_ERROR_NOP_OFFSET: usize = 0x603418 - 0x400000;
const MOVIE_CHECK_ERROR_PATCH_OFFSET: usize = 0x60341F - 0x400000;
const MOVIE_CHECK_NEED_UPDATE_PATCH_OFFSET: usize = 0x60342C - 0x400000;
const MOVIE_FILL_ECX_PATCH_OFFSET: usize = 0x603478 - 0x400000;
const MOVIE_FILL_FORCE_PATCH_OFFSET: usize = 0x60347D - 0x400000;
const MOVIE_FILL_BUFFER_PATCH_OFFSET: usize = 0x60348C - 0x400000;
const ARRAY_ALLOC_OFFSET: usize = 0x131C90;
const ARRAY_REPLACE_STORAGE_OFFSET: usize = 0x1A4BB0;
const ASYNC_PACK_THRESHOLD: usize = 50 * 1024;
const NORMAL_SAVE_DATA_SIZE_OFFSET: usize = 0x1228;
const NORMAL_SAVE_DATA_OFFSET: usize = NORMAL_SAVE_DATA_SIZE_OFFSET + 4;
const READ_SAVE_DATA_SIZE_OFFSET: usize = 0x8;
const READ_SAVE_DATA_OFFSET: usize = 0x10;
const CONFIG_SAVE_DATA_SIZE_OFFSET: usize = 0x8;
const CONFIG_SAVE_DATA_OFFSET: usize = 0xC;
const GLOBAL_SAVE_DATA_SIZE_OFFSET: usize = 0x8;
const GLOBAL_SAVE_DATA_OFFSET: usize = 0xC;
const TPC_ANGOU_TABLE_OFFSET: usize = 0x6567B0;
const PLACEHOLDER_MAGIC: &[u8; 4] = b"SPP0";
const PLACEHOLDER_HEADER_SIZE: usize = 16;
const PLACEHOLDER_VERSION: u32 = 1;

static ORIGINAL_GET_FILE_ATTRIBUTES_W: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ORIGINAL_COPY_FILE_W: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ORIGINAL_TNM_SAVE_TO_FILE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ORIGINAL_TNM_PACK_BUFFER: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ORIGINAL_TNM_CREATE_PNG_FROM_TEXTURE_AND_SAVE_TO_FILE: AtomicPtr<c_void> =
    AtomicPtr::new(std::ptr::null_mut());
static GET_FILE_ATTRIBUTES_W_HITS: AtomicUsize = AtomicUsize::new(0);
static FILE_ATTRIBUTE_CACHE: OnceLock<Arc<FileAttributeCache>> = OnceLock::new();
static SAVE_TASKS: OnceLock<Mutex<HashMap<String, Arc<SaveSlot>>>> = OnceLock::new();

thread_local! {
    static THREAD_FILE_ATTRIBUTE_CACHE: RefCell<Option<Arc<FileAttributeCache>>> = RefCell::new(None);
    static IN_ASYNC_SAVE_WORKER: RefCell<bool> = RefCell::new(false);
}

struct FileAttributeCache {
    g00_root: PathBuf,
    attributes: HashMap<String, u32>,
}

struct AttributeLookup {
    result: AttributeLookupResult,
}

enum AttributeLookupResult {
    Hit(u32),
    MissingInG00,
    ParentNotG00,
}

#[derive(Clone, Copy)]
struct SaveLayout {
    kind: &'static str,
    data_size_offset: usize,
    data_offset: usize,
}

struct SaveSlot {
    completed: Mutex<bool>,
    completed_cv: Condvar,
}

struct AsyncSaveJob {
    path_key: String,
    file_path: String,
    layout: SaveLayout,
    header: Vec<u8>,
    raw: Vec<u8>,
}

struct ProgramCStr {
    fields: [u32; 6],
    heap: Option<Vec<u16>>,
}

struct ProgramArray {
    fields: [u32; 3],
}

impl ProgramArray {
    fn as_mut_ptr(&mut self) -> *mut c_void {
        self.fields.as_mut_ptr().cast()
    }
}

impl Drop for ProgramArray {
    fn drop(&mut self) {
        unsafe {
            let _ = array_replace_storage(self.as_mut_ptr(), 0, 0, 0);
        }
    }
}

impl ProgramCStr {
    fn new(value: &str) -> Option<Self> {
        let mut wide: Vec<u16> = value.encode_utf16().collect();
        let len = wide.len();
        if len > u32::MAX as usize {
            return None;
        }

        let mut cstr = Self {
            fields: [0; 6],
            heap: None,
        };

        if len <= 7 {
            for (index, ch) in wide.iter().copied().enumerate() {
                let field = index / 2;
                let shift = (index % 2) * 16;
                cstr.fields[field] |= (ch as u32) << shift;
            }
            cstr.fields[4] = len as u32;
            cstr.fields[5] = 7;
        } else {
            wide.push(0);
            cstr.heap = Some(wide);
            cstr.fields[0] = cstr.heap.as_ref()?.as_ptr() as u32;
            cstr.fields[4] = len as u32;
            cstr.fields[5] = len as u32;
        }

        Some(cstr)
    }

    fn as_mut_ptr(&mut self) -> *mut c_void {
        self.fields.as_mut_ptr().cast()
    }

    fn debug_description(&self) -> String {
        let len = self.fields[4];
        let cap = self.fields[5];
        if len <= 7 {
            format!("inline len={len} cap={cap}")
        } else {
            format!(
                "heap ptr=0x{:08X} len={} cap={} storage_u16_len={}",
                self.fields[0],
                len,
                cap,
                self.heap.as_ref().map(|heap| heap.len()).unwrap_or(0),
            )
        }
    }
}

#[unsafe(no_mangle)]
#[allow(non_snake_case)]
pub extern "system" fn DllMain(
    dll_module: HINSTANCE,
    call_reason: u32,
    _reserved: *mut c_void,
) -> BOOL {
    let _ = catch_unwind(AssertUnwindSafe(|| match call_reason {
        DLL_PROCESS_ATTACH => {
            debug_log("siglus_hook: DLL_PROCESS_ATTACH");
            unsafe {
                DisableThreadLibraryCalls(dll_module);
                CreateThread(
                    std::ptr::null(),
                    0,
                    Some(install_hooks_thread),
                    std::ptr::null(),
                    0,
                    std::ptr::null_mut(),
                );
            }
        }
        _ => {}
    }));

    TRUE
}

unsafe extern "system" fn install_hooks_thread(_parameter: *mut c_void) -> u32 {
    match catch_unwind(AssertUnwindSafe(|| unsafe { install_hooks() })) {
        Ok(Ok(())) => {
            debug_log("siglus_hook: hooks installed");
            load_file_attribute_cache();
            0
        }
        Ok(Err(status)) => {
            debug_log(&format!(
                "siglus_hook: failed to install GetFileAttributesW hook: {status:?}"
            ));
            1
        }
        Err(_) => {
            debug_log("siglus_hook: panic while installing hooks");
            2
        }
    }
}

unsafe fn install_hooks() -> Result<(), MH_STATUS> {
    let original_get_file_attributes_w = unsafe {
        MinHook::create_hook(
            GetFileAttributesW as *mut c_void,
            detour_get_file_attributes_w as *mut c_void,
        )?
    };
    ORIGINAL_GET_FILE_ATTRIBUTES_W.store(original_get_file_attributes_w, Ordering::Release);

    let original_copy_file_w = unsafe {
        MinHook::create_hook(CopyFileW as *mut c_void, detour_copy_file_w as *mut c_void)?
    };
    ORIGINAL_COPY_FILE_W.store(original_copy_file_w, Ordering::Release);

    let tnm_save_to_file = unsafe { main_module_offset(TNM_SAVE_TO_FILE_OFFSET)? };
    let original_tnm_save_to_file =
        unsafe { MinHook::create_hook(tnm_save_to_file, detour_tnm_save_to_file as *mut c_void)? };
    ORIGINAL_TNM_SAVE_TO_FILE.store(original_tnm_save_to_file, Ordering::Release);

    let tnm_pack_buffer = unsafe { main_module_offset(TNM_PACK_BUFFER_OFFSET)? };
    let original_tnm_pack_buffer =
        unsafe { MinHook::create_hook(tnm_pack_buffer, detour_tnm_pack_buffer as *mut c_void)? };
    ORIGINAL_TNM_PACK_BUFFER.store(original_tnm_pack_buffer, Ordering::Release);

    let create_png =
        unsafe { main_module_offset(TNM_CREATE_PNG_FROM_TEXTURE_AND_SAVE_TO_FILE_OFFSET)? };
    let original_create_png = unsafe {
        MinHook::create_hook(
            create_png,
            detour_tnm_create_png_from_texture_and_save_to_file as *mut c_void,
        )?
    };
    ORIGINAL_TNM_CREATE_PNG_FROM_TEXTURE_AND_SAVE_TO_FILE
        .store(original_create_png, Ordering::Release);

    unsafe {
        patch_ecx_eax_call(
            MOVIE_END_LOOP_PATCH_OFFSET,
            0x55C331 - 0x55C329 + 1,
            omv_hook::spp_movie_player_end_loop as *const () as usize,
        )?;
        patch_movie_is_playing_alt()?;
        patch_movie_is_playing_alt_jnz()?;
        patch_movie_is_playing()?;
        patch_movie_seek_nop()?;
        patch_movie_destroy()?;
        patch_movie_restruct_new()?;
        patch_movie_restruct_new_followups()?;
        patch_movie_restruct_init_call()?;
        patch_movie_restruct_last_error_call()?;
        patch_movie_restruct_get_size()?;
        patch_movie_is_rgb()?;
        patch_movie_total_time()?;
        patch_movie_check_error()?;
        patch_movie_check_need_update()?;
        patch_movie_fill_ecx()?;
        patch_movie_fill_force()?;
        patch_movie_fill_buffer()?;
    }

    unsafe { MinHook::enable_all_hooks()? };
    Ok(())
}

unsafe fn patch_ecx_eax_call(
    offset: usize,
    patch_len: usize,
    call_target: usize,
) -> Result<(), MH_STATUS> {
    if patch_len < 7 {
        return Err(MH_STATUS::MH_ERROR_MEMORY_ALLOC);
    }

    let patch_address = unsafe { main_module_offset(offset)? };
    let call_site = patch_address as usize + 2;
    let next_instruction = call_site + 5;
    let relative = (call_target as isize)
        .checked_sub(next_instruction as isize)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(MH_STATUS::MH_ERROR_MEMORY_ALLOC)?;

    let mut patch = vec![0x90; patch_len];
    patch[0] = 0x8B;
    patch[1] = 0xC8;
    patch[2] = 0xE8;
    patch[3..7].copy_from_slice(&relative.to_le_bytes());
    unsafe { write_code_patch(patch_address, &patch) }
}

unsafe fn patch_movie_is_playing() -> Result<(), MH_STATUS> {
    let patch_address = unsafe { main_module_offset(MOVIE_IS_PLAYING_PATCH_OFFSET)? };
    let call_target = omv_hook::spp_movie_player_is_playing as *const () as usize;
    let call_site_offset = 2usize;
    let next_instruction = patch_address as usize + call_site_offset + 5;
    let relative = (call_target as isize)
        .checked_sub(next_instruction as isize)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(MH_STATUS::MH_ERROR_MEMORY_ALLOC)?;

    let mut patch = vec![0x90; 0x6035CB - 0x6035BA + 1];
    let code = [
        0x8B, 0xC8, // mov ecx, eax
        0xE8, 0, 0, 0, 0, // call spp_movie_player_is_playing
        0x84, 0xC0, // test al, al
    ];
    patch[..code.len()].copy_from_slice(&code);
    patch[3..7].copy_from_slice(&relative.to_le_bytes());
    unsafe { write_code_patch(patch_address, &patch)? };

    let jz_address = unsafe { main_module_offset(MOVIE_IS_PLAYING_JZ_PATCH_OFFSET)? };
    unsafe { write_code_patch(jz_address, &[0x74]) }
}

unsafe fn patch_movie_is_playing_alt() -> Result<(), MH_STATUS> {
    let patch_address = unsafe { main_module_offset(MOVIE_IS_PLAYING_ALT_PATCH_OFFSET)? };
    let call_target = omv_hook::spp_movie_player_is_playing as *const () as usize;
    let call_site_offset = 2usize;
    let next_instruction = patch_address as usize + call_site_offset + 5;
    let relative = (call_target as isize)
        .checked_sub(next_instruction as isize)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(MH_STATUS::MH_ERROR_MEMORY_ALLOC)?;

    let mut patch = vec![0x90; 0x5FB8FA - 0x5FB8E9 + 1];
    let code = [
        0x8B, 0xC8, // mov ecx, eax
        0xE8, 0, 0, 0, 0, // call spp_movie_player_is_playing
        0x84, 0xC0, // test al, al
    ];
    patch[..code.len()].copy_from_slice(&code);
    patch[3..7].copy_from_slice(&relative.to_le_bytes());
    unsafe { write_code_patch(patch_address, &patch) }
}

unsafe fn patch_movie_is_playing_alt_jnz() -> Result<(), MH_STATUS> {
    let patch_address = unsafe { main_module_offset(MOVIE_IS_PLAYING_ALT_JNZ_PATCH_OFFSET)? };
    unsafe { write_code_patch(patch_address, &[0x75]) }
}

unsafe fn patch_movie_seek_nop() -> Result<(), MH_STATUS> {
    let patch_address = unsafe { main_module_offset(MOVIE_SEEK_NOP_PATCH_OFFSET)? };
    let patch = vec![0x90; 0x55C2A8 - 0x55C29A + 1];
    unsafe { write_code_patch(patch_address, &patch) }
}

unsafe fn patch_movie_destroy() -> Result<(), MH_STATUS> {
    let patch_address = unsafe { main_module_offset(MOVIE_DESTROY_PATCH_OFFSET)? };
    let jump_target = omv_hook::spp_movie_player_destroy as *const () as usize;
    let next_instruction = patch_address as usize + 5;
    let relative = (jump_target as isize)
        .checked_sub(next_instruction as isize)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(MH_STATUS::MH_ERROR_MEMORY_ALLOC)?;

    let mut patch = [0xE9, 0, 0, 0, 0];
    patch[1..5].copy_from_slice(&relative.to_le_bytes());
    unsafe { write_code_patch(patch_address, &patch) }
}

unsafe fn patch_movie_restruct_new() -> Result<(), MH_STATUS> {
    let patch_address = unsafe { main_module_offset(MOVIE_RESTRUCT_NEW_PATCH_OFFSET)? };
    let call_target = omv_hook::spp_movie_player_new as *const () as usize;
    let call_site_offset = 3usize;
    let next_instruction = patch_address as usize + call_site_offset + 5;
    let relative = (call_target as isize)
        .checked_sub(next_instruction as isize)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(MH_STATUS::MH_ERROR_MEMORY_ALLOC)?;

    let mut patch = vec![0x90; 0x602E45 - 0x602DAA + 1];
    let code = [
        0x83, 0xC4, 0x04, // add esp, 4
        0xE8, 0, 0, 0, 0, // call spp_movie_player_new
        0x8B, 0xF8, // mov edi, eax
        0x89, 0xBD, 0x60, 0xFF, 0xFF, 0xFF, // mov [ebp-0A0h], edi
    ];
    patch[..code.len()].copy_from_slice(&code);
    patch[4..8].copy_from_slice(&relative.to_le_bytes());
    unsafe { write_code_patch(patch_address, &patch) }
}

unsafe fn patch_movie_restruct_new_followups() -> Result<(), MH_STATUS> {
    let nop_1_address = unsafe { main_module_offset(MOVIE_RESTRUCT_NEW_NOP_1_OFFSET)? };
    let nop_1 = vec![0x90; 0x602EC7 - 0x602EC1 + 1];
    unsafe { write_code_patch(nop_1_address, &nop_1)? };

    let store_address = unsafe { main_module_offset(MOVIE_RESTRUCT_NEW_STORE_PATCH_OFFSET)? };
    let mut store_patch = vec![0x90; 0x602F48 - 0x602ECC + 1];
    let store_code = [
        0x8B, 0x95, 0x64, 0xFF, 0xFF, 0xFF, // mov edx, [ebp-9Ch]
        0x8B, 0x52, 0x04, // mov edx, [edx+4]
        0x89, 0x55, 0xE4, // mov [ebp-1Ch], edx
        0x8B, 0x95, 0x5C, 0xFF, 0xFF, 0xFF, // mov edx, [ebp-0A4h]
        0x33, 0xFF, // xor edi, edi
        0x8B, 0x85, 0x60, 0xFF, 0xFF, 0xFF, // mov eax, [ebp-0A0h]
        0x89, 0x82, 0xF0, 0x15, 0x00, 0x00, // mov [edx+15F0h], eax
        0x8B, 0x82, 0xF4, 0x15, 0x00, 0x00, // mov eax, [edx+15F4h]
        0x89, 0xBD, 0x60, 0xFF, 0xFF, 0xFF, // mov [ebp-0A0h], edi
        0x8B, 0x4D, 0xE4, // mov ecx, [ebp-1Ch]
        0x89, 0x45, 0xE4, // mov [ebp-1Ch], eax
        0x89, 0x8A, 0xF4, 0x15, 0x00, 0x00, // mov [edx+15F4h], ecx
    ];
    store_patch[..store_code.len()].copy_from_slice(&store_code);
    unsafe { write_code_patch(store_address, &store_patch)? };

    let nop_2_address = unsafe { main_module_offset(MOVIE_RESTRUCT_NEW_NOP_2_OFFSET)? };
    let nop_2 = vec![0x90; 0x602F79 - 0x602F77 + 1];
    unsafe { write_code_patch(nop_2_address, &nop_2)? };

    let nop_3_address = unsafe { main_module_offset(MOVIE_RESTRUCT_NEW_NOP_3_OFFSET)? };
    let nop_3 = vec![0x90; 0x602FA1 - 0x602F7E + 1];
    unsafe { write_code_patch(nop_3_address, &nop_3) }
}

unsafe fn patch_movie_restruct_init_call() -> Result<(), MH_STATUS> {
    let pre_nop_address = unsafe { main_module_offset(MOVIE_RESTRUCT_INIT_NOP_PRE_OFFSET)? };
    unsafe { write_code_patch(pre_nop_address, &[0x8B, 0xC8])? };

    let nop_address = unsafe { main_module_offset(MOVIE_RESTRUCT_INIT_NOP_OFFSET)? };
    unsafe { write_code_patch(nop_address, &[0x90])? };

    let patch_address = unsafe { main_module_offset(MOVIE_RESTRUCT_INIT_CALL_OFFSET)? };
    let call_target = omv_hook::spp_movie_player_init as *const () as usize;
    let next_instruction = patch_address as usize + 5;
    let relative = (call_target as isize)
        .checked_sub(next_instruction as isize)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(MH_STATUS::MH_ERROR_MEMORY_ALLOC)?;

    let mut patch = [0xE8, 0, 0, 0, 0];
    patch[1..5].copy_from_slice(&relative.to_le_bytes());
    unsafe { write_code_patch(patch_address, &patch) }
}

unsafe fn patch_movie_restruct_last_error_call() -> Result<(), MH_STATUS> {
    let patch_address = unsafe { main_module_offset(MOVIE_RESTRUCT_LAST_ERROR_CALL_OFFSET)? };
    let call_target = omv_hook::spp_movie_player_get_last_error as *const () as usize;
    let next_instruction = patch_address as usize + 5;
    let relative = (call_target as isize)
        .checked_sub(next_instruction as isize)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(MH_STATUS::MH_ERROR_MEMORY_ALLOC)?;

    let mut patch = [0xE8, 0, 0, 0, 0];
    patch[1..5].copy_from_slice(&relative.to_le_bytes());
    unsafe { write_code_patch(patch_address, &patch) }
}

unsafe fn patch_movie_restruct_get_size() -> Result<(), MH_STATUS> {
    let patch_address = unsafe { main_module_offset(MOVIE_RESTRUCT_GET_SIZE_PATCH_OFFSET)? };
    let call_target = omv_hook::spp_movie_player_get_size as *const () as usize;
    let call_site_offset = 14usize;
    let next_instruction = patch_address as usize + call_site_offset + 5;
    let relative = (call_target as isize)
        .checked_sub(next_instruction as isize)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(MH_STATUS::MH_ERROR_MEMORY_ALLOC)?;

    let mut patch = vec![0x90; 0x6030BA - 0x60308F + 1];
    let code = [
        0x8B, 0x8F, 0xF0, 0x15, 0x00, 0x00, // mov ecx, [edi+15F0h]
        0x8D, 0x45, 0xE0, // lea eax, [ebp-20h]
        0x50, // push eax
        0x8D, 0x45, 0xDC, // lea eax, [ebp-24h]
        0x50, // push eax
        0xE8, 0, 0, 0, 0, // call spp_movie_player_get_size
        0x8D, 0x4D, 0xB8, // lea ecx, [ebp-48h]
    ];
    patch[..code.len()].copy_from_slice(&code);
    patch[15..19].copy_from_slice(&relative.to_le_bytes());
    unsafe { write_code_patch(patch_address, &patch)? };

    let nop_address = unsafe { main_module_offset(MOVIE_RESTRUCT_GET_SIZE_NOP_OFFSET)? };
    unsafe { write_code_patch(nop_address, &[0x90, 0x90, 0x90]) }
}

unsafe fn patch_movie_total_time() -> Result<(), MH_STATUS> {
    let patch_address = unsafe { main_module_offset(MOVIE_TOTAL_TIME_PATCH_OFFSET)? };
    let call_target = omv_hook::spp_movie_player_total_time_ms as *const () as usize;
    let call_site_offset = 2usize;
    let next_instruction = patch_address as usize + call_site_offset + 5;
    let relative = (call_target as isize)
        .checked_sub(next_instruction as isize)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(MH_STATUS::MH_ERROR_MEMORY_ALLOC)?;

    let mut patch = vec![0x90; 0x6033EB - 0x6033E4 + 1];
    let code = [
        0x8B, 0xC8, // mov ecx, eax
        0xE8, 0, 0, 0, 0, // call spp_movie_player_total_time_ms
        0x90, // nop
    ];
    patch[..code.len()].copy_from_slice(&code);
    patch[3..7].copy_from_slice(&relative.to_le_bytes());
    unsafe { write_code_patch(patch_address, &patch) }
}

unsafe fn patch_movie_is_rgb() -> Result<(), MH_STATUS> {
    let patch_address = unsafe { main_module_offset(MOVIE_IS_RGB_PATCH_OFFSET)? };
    let call_target = omv_hook::spp_movie_player_is_rgb as *const () as usize;
    let call_site_offset = 3usize;
    let next_instruction = patch_address as usize + call_site_offset + 5;
    let relative = (call_target as isize)
        .checked_sub(next_instruction as isize)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(MH_STATUS::MH_ERROR_MEMORY_ALLOC)?;

    let mut patch = vec![0x90; 0x5FE42B - 0x5FE40D + 1];
    let code = [
        0x51, // push ecx
        0x8B, 0xC8, // mov ecx, eax
        0xE8, 0, 0, 0, 0, // call spp_movie_player_is_rgb
        0x59, // pop ecx
        0x85, 0xC0, // test eax, eax
    ];
    patch[..code.len()].copy_from_slice(&code);
    patch[4..8].copy_from_slice(&relative.to_le_bytes());
    unsafe { write_code_patch(patch_address, &patch) }
}

unsafe fn patch_movie_check_error() -> Result<(), MH_STATUS> {
    let nop_address = unsafe { main_module_offset(MOVIE_CHECK_ERROR_NOP_OFFSET)? };
    unsafe { write_code_patch(nop_address, &[0x90, 0x90])? };

    let patch_address = unsafe { main_module_offset(MOVIE_CHECK_ERROR_PATCH_OFFSET)? };
    let call_target = omv_hook::spp_movie_player_check_error as *const () as usize;
    let next_instruction = patch_address as usize + 5;
    let relative = (call_target as isize)
        .checked_sub(next_instruction as isize)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(MH_STATUS::MH_ERROR_MEMORY_ALLOC)?;

    let mut patch = [0xE8, 0, 0, 0, 0];
    patch[1..5].copy_from_slice(&relative.to_le_bytes());
    unsafe { write_code_patch(patch_address, &patch) }
}

unsafe fn patch_movie_check_need_update() -> Result<(), MH_STATUS> {
    let patch_address = unsafe { main_module_offset(MOVIE_CHECK_NEED_UPDATE_PATCH_OFFSET)? };
    let call_target = omv_hook::spp_movie_player_check_need_update as *const () as usize;
    let call_site_offset = 17usize;
    let next_instruction = patch_address as usize + call_site_offset + 5;
    let relative = (call_target as isize)
        .checked_sub(next_instruction as isize)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(MH_STATUS::MH_ERROR_MEMORY_ALLOC)?;

    let mut patch = vec![0x90; 0x603449 - 0x60342C + 1];
    let code = [
        0x80, 0x7D, 0xE8, 0x00, // cmp byte ptr [ebp-18h], 0
        0x0F, 0x95, 0xC0, // setnz al
        0x0F, 0xB6, 0xC0, // movzx eax, al
        0x50, // push eax
        0xFF, 0x75, 0xE4, // push [ebp-1Ch]
        0x8B, 0x4D, 0xEC, // mov ecx, [ebp-14h]
        0xE8, 0, 0, 0, 0, // call spp_movie_player_check_need_update
        0x33, 0xC9, // xor ecx, ecx
        0x84, 0xC0, // test al, al
    ];
    patch[..code.len()].copy_from_slice(&code);
    patch[18..22].copy_from_slice(&relative.to_le_bytes());
    unsafe { write_code_patch(patch_address, &patch) }
}

unsafe fn patch_movie_fill_ecx() -> Result<(), MH_STATUS> {
    let patch_address = unsafe { main_module_offset(MOVIE_FILL_ECX_PATCH_OFFSET)? };
    unsafe { write_code_patch(patch_address, &[0x8B, 0xC8]) }
}

unsafe fn patch_movie_fill_force() -> Result<(), MH_STATUS> {
    let patch_address = unsafe { main_module_offset(MOVIE_FILL_FORCE_PATCH_OFFSET)? };
    let mut patch = vec![0x90; 0x603481 - 0x60347D + 1];
    patch[0] = 0xB0; // mov al, 1
    patch[1] = 0x01;
    unsafe { write_code_patch(patch_address, &patch) }
}

unsafe fn patch_movie_fill_buffer() -> Result<(), MH_STATUS> {
    let patch_address = unsafe { main_module_offset(MOVIE_FILL_BUFFER_PATCH_OFFSET)? };
    let call_target = omv_hook::spp_movie_player_fill_buffer as *const () as usize;
    let call_site_offset = 14usize;
    let next_instruction = patch_address as usize + call_site_offset + 5;
    let relative = (call_target as isize)
        .checked_sub(next_instruction as isize)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(MH_STATUS::MH_ERROR_MEMORY_ALLOC)?;

    let mut patch = vec![0x90; 0x6034C5 - 0x60348C + 1];
    let code = [
        0xFF, 0x75, 0xE0, // push [ebp-20h]
        0xFF, 0x75, 0xDC, // push [ebp-24h]
        0xFF, 0xB7, 0xF8, 0x15, 0x00, 0x00, // push [edi+15F8h]
        0x8B, 0xCA, // mov ecx, edx
        0xE8, 0, 0, 0, 0, // call spp_movie_player_fill_buffer
    ];
    patch[..code.len()].copy_from_slice(&code);
    patch[15..19].copy_from_slice(&relative.to_le_bytes());
    unsafe { write_code_patch(patch_address, &patch) }
}
unsafe fn write_code_patch(address: *mut c_void, bytes: &[u8]) -> Result<(), MH_STATUS> {
    let mut old_protect = 0;
    let protect_ok = unsafe {
        VirtualProtect(
            address,
            bytes.len(),
            PAGE_EXECUTE_READWRITE,
            &mut old_protect,
        )
    };
    if protect_ok == 0 {
        return Err(MH_STATUS::MH_ERROR_MEMORY_PROTECT);
    }

    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), address.cast::<u8>(), bytes.len());
    }

    let mut ignored_protect = 0;
    unsafe {
        VirtualProtect(address, bytes.len(), old_protect, &mut ignored_protect);
    }

    Ok(())
}

unsafe extern "system" fn detour_get_file_attributes_w(file_name: PCWSTR) -> u32 {
    let hit = GET_FILE_ATTRIBUTES_W_HITS.fetch_add(1, Ordering::Relaxed);
    if hit < 20 {
        debug_log("siglus_hook: GetFileAttributesW hit");
    }

    match catch_unwind(AssertUnwindSafe(|| unsafe {
        get_file_attributes_w_hook_body(file_name)
    })) {
        Ok(attributes) => attributes,
        Err(_) => {
            debug_log("siglus_hook: panic inside GetFileAttributesW hook body");
            unsafe { call_original_get_file_attributes_w(file_name) }
        }
    }
}

unsafe fn get_file_attributes_w_hook_body(file_name: PCWSTR) -> u32 {
    if let Some(attributes) = unsafe { get_cached_file_attributes_w(file_name) } {
        return attributes;
    }

    unsafe { call_original_get_file_attributes_w(file_name) }
}

unsafe fn call_original_get_file_attributes_w(file_name: PCWSTR) -> u32 {
    let original = ORIGINAL_GET_FILE_ATTRIBUTES_W.load(Ordering::Acquire);
    if original.is_null() {
        return INVALID_FILE_ATTRIBUTES;
    }

    let original: GetFileAttributesWFn = unsafe { std::mem::transmute(original) };
    unsafe { original(file_name) }
}

unsafe extern "system" fn detour_copy_file_w(
    existing_file_name: PCWSTR,
    new_file_name: PCWSTR,
    fail_if_exists: BOOL,
) -> BOOL {
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        copy_file_w_hook_body(existing_file_name, new_file_name, fail_if_exists)
    })) {
        Ok(result) => result,
        Err(_) => {
            debug_log("siglus_hook: panic inside CopyFileW hook body");
            unsafe { call_original_copy_file_w(existing_file_name, new_file_name, fail_if_exists) }
        }
    }
}

unsafe fn copy_file_w_hook_body(
    existing_file_name: PCWSTR,
    new_file_name: PCWSTR,
    fail_if_exists: BOOL,
) -> BOOL {
    let existing = unsafe { pcwstr_to_log_string(existing_file_name) };
    let new = unsafe { pcwstr_to_log_string(new_file_name) };
    debug_log(&format!(
        "siglus_hook: CopyFileW existing=\"{}\" new=\"{}\" fail_if_exists={}",
        existing, new, fail_if_exists,
    ));

    if should_move_rotated_save_or_png(&existing, &new) {
        if fail_if_exists == 0 {
            debug_log(&format!(
                "siglus_hook: CopyFileW rewritten to MoveFileExW(REPLACE_EXISTING) existing=\"{}\" new=\"{}\"",
                existing, new,
            ));
            return unsafe {
                MoveFileExW(existing_file_name, new_file_name, MOVEFILE_REPLACE_EXISTING)
            };
        } else {
            debug_log(&format!(
                "siglus_hook: CopyFileW rewritten to MoveFileW existing=\"{}\" new=\"{}\"",
                existing, new,
            ));
            return unsafe { MoveFileW(existing_file_name, new_file_name) };
        }
    }

    unsafe { call_original_copy_file_w(existing_file_name, new_file_name, fail_if_exists) }
}

#[allow(dead_code)]
unsafe fn call_original_copy_file_w(
    existing_file_name: PCWSTR,
    new_file_name: PCWSTR,
    fail_if_exists: BOOL,
) -> BOOL {
    let original = ORIGINAL_COPY_FILE_W.load(Ordering::Acquire);
    if original.is_null() {
        return 0;
    }

    let original: CopyFileWFn = unsafe { std::mem::transmute(original) };
    unsafe { original(existing_file_name, new_file_name, fail_if_exists) }
}

fn should_move_rotated_save_or_png(existing: &str, new: &str) -> bool {
    let Some(existing) = numbered_sav_or_png_file(existing) else {
        return false;
    };
    let Some(new) = numbered_sav_or_png_file(new) else {
        return false;
    };

    existing.extension == new.extension
        && existing.number.checked_add(1) == Some(new.number)
        && (1001..=1009).contains(&new.number)
}

struct NumberedSavOrPngFile<'a> {
    number: u32,
    extension: &'a str,
}

fn numbered_sav_or_png_file(path: &str) -> Option<NumberedSavOrPngFile<'_>> {
    let file_name = path.rsplit(['\\', '/']).next()?;
    let (stem, extension) = file_name.rsplit_once('.')?;
    if stem.is_empty() || !stem.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }

    let extension = if extension.eq_ignore_ascii_case("sav") {
        "sav"
    } else if extension.eq_ignore_ascii_case("png") {
        "png"
    } else {
        return None;
    };

    let number = stem.parse().ok()?;
    Some(NumberedSavOrPngFile { number, extension })
}

unsafe extern "fastcall" fn detour_tnm_save_to_file(
    file_path: *const c_void,
    write_data: *const c_void,
) -> bool {
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        tnm_save_to_file_hook_body(file_path, write_data)
    })) {
        Ok(result) => result,
        Err(_) => {
            debug_log("siglus_hook: panic inside tnm_save_to_file hook body");
            unsafe { call_original_tnm_save_to_file(file_path, write_data) }
        }
    }
}

unsafe fn tnm_save_to_file_hook_body(file_path: *const c_void, write_data: *const c_void) -> bool {
    if IN_ASYNC_SAVE_WORKER.with(|flag| *flag.borrow()) {
        return unsafe { call_original_tnm_save_to_file(file_path, write_data) };
    }

    // let file_path_raw = unsafe { dump_u32_words(file_path, 6) };
    let file_path_text = unsafe { read_cstr(file_path) };
    let file_name = save_file_name_from_cstr_log(&file_path_text);
    let save_layout = file_name.as_deref().and_then(save_layout_for_file_name);
    let buffer_len = unsafe { byte_array_len(write_data) };
    let data_size =
        save_layout.and_then(|layout| unsafe { read_u32_at(write_data, layout.data_size_offset) });
    let payload_len = save_layout
        .and_then(|layout| buffer_len.and_then(|len| len.checked_sub(layout.data_offset)));
    debug_log(&format!(
        "siglus_hook: tnm_save_to_file file_path=\"{}\" save_kind={} write_data={} buffer_len={} payload_len_after_header={} data_size={}",
        file_path_text,
        save_layout.map(|layout| layout.kind).unwrap_or("not-sav"),
        unsafe { dump_byte_array(write_data) },
        format_option_usize(buffer_len),
        format_option_usize(payload_len),
        format_option_u32(data_size),
    ));

    if let Some(layout) = save_layout {
        if let Some(result) =
            unsafe { try_queue_placeholder_save(file_path, write_data as *mut c_void, layout) }
        {
            return result;
        }
    }

    unsafe { call_original_tnm_save_to_file(file_path, write_data) }
}

unsafe fn call_original_tnm_save_to_file(
    file_path: *const c_void,
    write_data: *const c_void,
) -> bool {
    let original = ORIGINAL_TNM_SAVE_TO_FILE.load(Ordering::Acquire);
    if original.is_null() {
        return false;
    }

    let original: TnmSaveToFileFn = unsafe { std::mem::transmute(original) };
    unsafe { original(file_path, write_data) }
}

unsafe fn try_queue_placeholder_save(
    file_path: *const c_void,
    write_data: *mut c_void,
    layout: SaveLayout,
) -> Option<bool> {
    let (save_start, save_end, _) = unsafe { read_byte_array_fields(write_data)? };
    let save_len = save_end.checked_sub(save_start)?;
    if save_len < layout.data_offset + PLACEHOLDER_HEADER_SIZE {
        return None;
    }

    let data_start = layout.data_offset;
    let data_ptr = (save_start + data_start) as *const u8;
    let magic = unsafe { std::slice::from_raw_parts(data_ptr, PLACEHOLDER_MAGIC.len()) };
    if magic != PLACEHOLDER_MAGIC {
        return None;
    }

    let raw_len = unsafe { std::ptr::read_unaligned(data_ptr.add(4).cast::<u32>()) } as usize;
    let version = unsafe { std::ptr::read_unaligned(data_ptr.add(8).cast::<u32>()) };
    if version != PLACEHOLDER_VERSION {
        debug_log(&format!(
            "siglus_hook: placeholder version mismatch version={version}"
        ));
        return None;
    }

    let raw_start = data_start + PLACEHOLDER_HEADER_SIZE;
    let raw_end = raw_start.checked_add(raw_len)?;
    if raw_end > save_len {
        debug_log(&format!(
            "siglus_hook: placeholder raw range invalid raw_len={} save_len={}",
            raw_len, save_len,
        ));
        return None;
    }

    let raw_ptr = (save_start + raw_start) as *const u8;
    let path = unsafe { read_cstr_plain(file_path)? };
    let header =
        unsafe { std::slice::from_raw_parts(save_start as *const u8, layout.data_offset) }.to_vec();
    let raw = unsafe { std::slice::from_raw_parts(raw_ptr, raw_len) }.to_vec();
    debug_log(&format!(
        "siglus_hook: placeholder detected; queue async save kind={} path={} raw_len=0x{raw_len:X}/{raw_len}",
        layout.kind, path,
    ));

    queue_async_save(AsyncSaveJob {
        path_key: path.to_ascii_lowercase(),
        file_path: path,
        layout,
        header,
        raw,
    });
    Some(true)
}

fn queue_async_save(job: AsyncSaveJob) {
    let tasks = SAVE_TASKS.get_or_init(|| Mutex::new(HashMap::new()));
    let slot = Arc::new(SaveSlot {
        completed: Mutex::new(false),
        completed_cv: Condvar::new(),
    });

    let previous = {
        let mut tasks = tasks.lock().unwrap();
        tasks.insert(job.path_key.clone(), Arc::clone(&slot))
    };

    if let Some(previous) = previous {
        debug_log(&format!(
            "siglus_hook: waiting previous async save path={}",
            job.file_path,
        ));
        let mut completed = previous.completed.lock().unwrap();
        while !*completed {
            completed = previous.completed_cv.wait(completed).unwrap();
        }
    }

    debug_log(&format!(
        "siglus_hook: async save queued path={} kind={} raw_len=0x{:X}/{}",
        job.file_path,
        job.layout.kind,
        job.raw.len(),
        job.raw.len(),
    ));

    let path_key = job.path_key.clone();
    thread::spawn(move || {
        let result = catch_unwind(AssertUnwindSafe(|| unsafe { run_async_save_job(&job) }));
        match result {
            Ok(true) => debug_log(&format!(
                "siglus_hook: async save complete path={}",
                job.file_path
            )),
            Ok(false) => debug_log(&format!(
                "siglus_hook: async save failed path={}",
                job.file_path
            )),
            Err(_) => debug_log(&format!(
                "siglus_hook: panic in async save path={}",
                job.file_path
            )),
        }

        if let Some(tasks) = SAVE_TASKS.get() {
            let mut tasks = tasks.lock().unwrap();
            if let Some(current) = tasks.get(&path_key) {
                if Arc::ptr_eq(current, &slot) {
                    tasks.remove(&path_key);
                }
            }
        }

        let mut completed = slot.completed.lock().unwrap();
        *completed = true;
        slot.completed_cv.notify_all();
    });
}

unsafe fn run_async_save_job(job: &AsyncSaveJob) -> bool {
    debug_log(&format!(
        "siglus_hook: async worker start path={}",
        job.file_path
    ));

    let mut temp = match unsafe { program_array_from_raw(job.raw.as_ptr(), job.raw.len()) } {
        Some(temp) => temp,
        None => {
            debug_log("siglus_hook: failed to allocate temp array for async repack");
            return false;
        }
    };

    debug_log("siglus_hook: async pack start");
    if !unsafe { call_original_tnm_pack_buffer(temp.as_mut_ptr()) } {
        debug_log("siglus_hook: original tnm_pack_buffer failed during async repack");
        return false;
    }
    debug_log("siglus_hook: async pack end");

    let (packed_start, packed_end, _) = match unsafe { read_byte_array_fields(temp.as_mut_ptr()) } {
        Some(fields) => fields,
        None => {
            debug_log("siglus_hook: failed to read packed temp array fields");
            return false;
        }
    };
    let Some(packed_len) = packed_end.checked_sub(packed_start) else {
        return false;
    };
    let Some(new_save_len) = job.layout.data_offset.checked_add(packed_len) else {
        return false;
    };

    let mut save_array = match unsafe {
        program_save_array_from_parts(
            &job.header,
            packed_start as *const u8,
            packed_len,
            new_save_len,
            job.layout,
        )
    } {
        Some(save_array) => save_array,
        None => {
            debug_log("siglus_hook: failed to allocate save array for async save");
            return false;
        }
    };

    let mut cstr = match ProgramCStr::new(&job.file_path) {
        Some(cstr) => cstr,
        None => {
            debug_log("siglus_hook: failed to build CSTR for async save");
            return false;
        }
    };
    debug_log(&format!(
        "siglus_hook: async save CSTR path={} {}",
        job.file_path,
        cstr.debug_description(),
    ));

    debug_log(&format!(
        "siglus_hook: async repack complete packed_len=0x{:X}/{} new_save_len=0x{:X}/{}",
        packed_len, packed_len, new_save_len, new_save_len,
    ));

    IN_ASYNC_SAVE_WORKER.with(|flag| {
        *flag.borrow_mut() = true;
    });
    debug_log("siglus_hook: async save start");
    let save_result =
        unsafe { call_original_tnm_save_to_file(cstr.as_mut_ptr(), save_array.as_mut_ptr()) };
    IN_ASYNC_SAVE_WORKER.with(|flag| {
        *flag.borrow_mut() = false;
    });
    debug_log(&format!("siglus_hook: async save end result={save_result}"));

    save_result
}

unsafe fn program_save_array_from_parts(
    header: &[u8],
    packed_start: *const u8,
    packed_len: usize,
    new_save_len: usize,
    layout: SaveLayout,
) -> Option<ProgramArray> {
    if header.len() != layout.data_offset || packed_start.is_null() {
        return None;
    }

    let mut array = ProgramArray { fields: [0; 3] };
    let start = unsafe { array_alloc(new_save_len)? };
    unsafe {
        std::ptr::copy_nonoverlapping(header.as_ptr(), start, header.len());
        std::ptr::copy_nonoverlapping(packed_start, start.add(layout.data_offset), packed_len);
        std::ptr::write_unaligned(
            start.add(layout.data_size_offset).cast::<u32>(),
            packed_len as u32,
        );
        array_replace_storage(
            array.as_mut_ptr(),
            start as u32,
            new_save_len as u32,
            new_save_len as u32,
        );
    }

    Some(array)
}

unsafe extern "fastcall" fn detour_tnm_pack_buffer(src: *const c_void) -> bool {
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        tnm_pack_buffer_hook_body(src)
    })) {
        Ok(result) => result,
        Err(_) => {
            debug_log("siglus_hook: panic inside tnm_pack_buffer hook body");
            unsafe { call_original_tnm_pack_buffer(src) }
        }
    }
}

unsafe fn tnm_pack_buffer_hook_body(src: *const c_void) -> bool {
    let len = unsafe { byte_array_len(src) };
    let async_candidate = len.map(|len| len > ASYNC_PACK_THRESHOLD).unwrap_or(false);
    debug_log(&format!(
        "siglus_hook: tnm_pack_buffer src={} raw_len={} async_candidate={}",
        unsafe { dump_byte_array(src) },
        format_option_usize(len),
        async_candidate,
    ));

    if async_candidate {
        if unsafe { write_placeholder_pack_buffer(src as *mut c_void) } {
            debug_log(&format!(
                "siglus_hook: tnm_pack_buffer placeholder applied raw_len={}",
                format_option_usize(len),
            ));
            return true;
        }

        debug_log("siglus_hook: tnm_pack_buffer placeholder failed; falling back to original");
    }

    let result = unsafe { call_original_tnm_pack_buffer(src) };
    debug_log(&format!(
        "siglus_hook: tnm_pack_buffer after result={} src={} sizes={}",
        result,
        unsafe { dump_byte_array(src) },
        unsafe { dump_packed_buffer_sizes(src) },
    ));

    result
}

unsafe fn write_placeholder_pack_buffer(src: *mut c_void) -> bool {
    let Some((raw_start, raw_end, _)) = (unsafe { read_byte_array_fields(src) }) else {
        return false;
    };

    let Some(raw_len) = raw_end.checked_sub(raw_start) else {
        return false;
    };

    let Some(placeholder_len) = raw_len.checked_add(PLACEHOLDER_HEADER_SIZE) else {
        return false;
    };

    let Some(new_start) = (unsafe { array_alloc(placeholder_len) }) else {
        return false;
    };

    let mut header = [0u8; PLACEHOLDER_HEADER_SIZE];
    header[0..4].copy_from_slice(PLACEHOLDER_MAGIC);
    header[4..8].copy_from_slice(&(raw_len as u32).to_le_bytes());
    header[8..12].copy_from_slice(&PLACEHOLDER_VERSION.to_le_bytes());

    unsafe {
        std::ptr::copy_nonoverlapping(header.as_ptr(), new_start, header.len());
        std::ptr::copy_nonoverlapping(
            raw_start as *const u8,
            new_start.add(PLACEHOLDER_HEADER_SIZE),
            raw_len,
        );

        array_replace_storage(
            src,
            new_start as u32,
            placeholder_len as u32,
            placeholder_len as u32,
        );
    }

    true
}

unsafe fn call_original_tnm_pack_buffer(src: *const c_void) -> bool {
    let original = ORIGINAL_TNM_PACK_BUFFER.load(Ordering::Acquire);
    if original.is_null() {
        return false;
    }

    let original: TnmPackBufferFn = unsafe { std::mem::transmute(original) };
    unsafe { original(src) }
}

unsafe extern "fastcall" fn detour_tnm_create_png_from_texture_and_save_to_file(
    file_path: *const c_void,
    width: i32,
    height: i32,
    p_rect: *const c_void,
    use_alpha: u32,
) -> bool {
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        tnm_create_png_from_texture_and_save_to_file_hook_body(
            file_path, width, height, p_rect, use_alpha,
        )
    })) {
        Ok(result) => result,
        Err(_) => {
            debug_log(
                "siglus_hook: panic inside tnm_create_png_from_texture_and_save_to_file hook body",
            );
            true
        }
    }
}

unsafe fn tnm_create_png_from_texture_and_save_to_file_hook_body(
    file_path: *const c_void,
    width: i32,
    height: i32,
    p_rect: *const c_void,
    use_alpha: u32,
) -> bool {
    let file_path_text = unsafe { read_cstr(file_path) };
    debug_log(&format!(
        "siglus_hook: tnm_create_png_from_texture_and_save_to_file file_path=\"{}\" width={} height={} p_rect=0x{:08X} {} use_alpha={} alpha_ignored=true",
        file_path_text,
        width,
        height,
        p_rect as usize,
        unsafe { dump_d3d_locked_rect(p_rect) },
        use_alpha,
    ));

    match unsafe { write_png_from_texture(file_path, width, height, p_rect) } {
        Ok(()) => {
            debug_log(&format!(
                "siglus_hook: rust png write complete file_path=\"{}\"",
                file_path_text,
            ));
            true
        }
        Err(error) => {
            debug_log(&format!(
                "siglus_hook: rust png write failed file_path=\"{}\" error={}",
                file_path_text, error,
            ));
            false
        }
    }
}

unsafe fn write_png_from_texture(
    file_path: *const c_void,
    width: i32,
    height: i32,
    p_rect: *const c_void,
) -> Result<(), String> {
    if width <= 0 || height <= 0 {
        return Err(format!("invalid size width={width} height={height}"));
    }
    if p_rect.is_null() {
        return Err("D3DLOCKED_RECT is null".to_string());
    }

    let path = unsafe { read_cstr_plain(file_path) }
        .ok_or_else(|| "failed to read file_path CSTR".to_string())?;
    let base = p_rect.cast::<u8>();
    let pitch = unsafe { std::ptr::read_unaligned(base.cast::<i32>()) };
    let bits = unsafe { std::ptr::read_unaligned(base.add(4).cast::<*const u8>()) };
    if pitch <= 0 {
        return Err(format!("unsupported pitch={pitch}"));
    }
    if bits.is_null() {
        return Err("D3DLOCKED_RECT.pBits is null".to_string());
    }

    let width = width as usize;
    let height = height as usize;
    let pitch = pitch as usize;
    let source_row_len = width
        .checked_mul(4)
        .ok_or_else(|| "source row length overflow".to_string())?;
    if pitch < source_row_len {
        return Err(format!(
            "pitch too small pitch={} row_len={}",
            pitch, source_row_len,
        ));
    }

    let rgb_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| "png buffer length overflow".to_string())?;
    let mut rgb = Vec::with_capacity(rgb_len);

    for y in 0..height {
        let row = unsafe { bits.add(y * pitch) };
        unsafe { append_bgra_row_as_rgb(row, width, &mut rgb) };
    }

    let file = File::create(&path).map_err(|error| format!("create {} failed: {error}", path))?;
    let writer = BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_filter(png::FilterType::NoFilter);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder
        .write_header()
        .map_err(|error| format!("write png header failed: {error}"))?;
    writer
        .write_image_data(&rgb)
        .map_err(|error| format!("write png data failed: {error}"))?;

    Ok(())
}

unsafe fn append_bgra_row_as_rgb(source: *const u8, width: usize, output: &mut Vec<u8>) {
    let chunks = width / 4;
    let tail = width % 4;
    for chunk_index in 0..chunks {
        let mut bgra = [0u8; 16];
        unsafe {
            std::ptr::copy_nonoverlapping(source.add(chunk_index * 16), bgra.as_mut_ptr(), 16);
        }
        output.extend_from_slice(&convert_shift_r(&bgra));
    }

    let tail_start = chunks * 4;
    for pixel_index in 0..tail {
        let pixel = unsafe { source.add((tail_start + pixel_index) * 4) };
        unsafe {
            output.push(*pixel.add(2));
            output.push(*pixel.add(1));
            output.push(*pixel);
        }
    }
}

fn convert_shift_r(bgra: &[u8; 16]) -> [u8; 12] {
    let bgra = bgra.as_chunks().0;
    let mut buffer = 0_u128;
    for bgra in bgra.iter().cloned().rev() {
        buffer <<= 24;
        buffer |= u128::from(u32::from_be_bytes(bgra) >> 8);
    }
    buffer.to_le_bytes().as_chunks().0[0]
}

#[allow(dead_code)]
unsafe fn call_original_tnm_create_png_from_texture_and_save_to_file(
    file_path: *const c_void,
    width: i32,
    height: i32,
    p_rect: *const c_void,
    use_alpha: u32,
) -> bool {
    let original = ORIGINAL_TNM_CREATE_PNG_FROM_TEXTURE_AND_SAVE_TO_FILE.load(Ordering::Acquire);
    if original.is_null() {
        return false;
    }

    let original: TnmCreatePngFromTextureAndSaveToFileFn = unsafe { std::mem::transmute(original) };
    unsafe { original(file_path, width, height, p_rect, use_alpha) }
}

unsafe fn main_module_offset(offset: usize) -> Result<*mut c_void, MH_STATUS> {
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    if module.is_null() {
        return Err(MH_STATUS::MH_ERROR_MODULE_NOT_FOUND);
    }

    Ok((module as usize + offset) as *mut c_void)
}

unsafe fn array_alloc(size: usize) -> Option<*mut u8> {
    let alloc = unsafe { main_module_offset(ARRAY_ALLOC_OFFSET).ok()? };
    let alloc: ArrayAllocFn = unsafe { std::mem::transmute(alloc) };
    let ptr = unsafe { alloc(size.try_into().ok()?) };
    if ptr == 0 { None } else { Some(ptr as *mut u8) }
}

unsafe fn array_replace_storage(array: *mut c_void, start: u32, size: u32, capacity: u32) -> i32 {
    let replace = unsafe { main_module_offset(ARRAY_REPLACE_STORAGE_OFFSET) }
        .expect("main module must exist for array replace storage");
    let replace: ArrayReplaceStorageFn = unsafe { std::mem::transmute(replace) };
    unsafe { replace(array, start, size, capacity) }
}

#[allow(dead_code)]
unsafe fn set_program_array_bytes(array: *mut c_void, bytes: &[u8]) -> bool {
    let Some(start) = (unsafe { array_alloc(bytes.len()) }) else {
        return false;
    };

    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), start, bytes.len());
        array_replace_storage(array, start as u32, bytes.len() as u32, bytes.len() as u32);
    }

    true
}

unsafe fn set_program_array_from_raw(array: *mut c_void, source: *const u8, len: usize) -> bool {
    if source.is_null() {
        return false;
    }

    let Some(start) = (unsafe { array_alloc(len) }) else {
        return false;
    };

    unsafe {
        std::ptr::copy_nonoverlapping(source, start, len);
        array_replace_storage(array, start as u32, len as u32, len as u32);
    }

    true
}

#[allow(dead_code)]
unsafe fn program_array_from_bytes(bytes: &[u8]) -> Option<ProgramArray> {
    let mut array = ProgramArray { fields: [0; 3] };
    if unsafe { set_program_array_bytes(array.as_mut_ptr(), bytes) } {
        Some(array)
    } else {
        None
    }
}

unsafe fn program_array_from_raw(source: *const u8, len: usize) -> Option<ProgramArray> {
    let mut array = ProgramArray { fields: [0; 3] };
    if unsafe { set_program_array_from_raw(array.as_mut_ptr(), source, len) } {
        Some(array)
    } else {
        None
    }
}

#[allow(dead_code)]
unsafe fn read_array_bytes(array: *const c_void) -> Option<Vec<u8>> {
    let (start, end, _) = unsafe { read_byte_array_fields(array)? };
    let len = end.checked_sub(start)?;
    Some(unsafe { std::slice::from_raw_parts(start as *const u8, len) }.to_vec())
}

#[allow(dead_code)]
unsafe fn dump_u32_words(value: *const c_void, word_count: usize) -> String {
    if value.is_null() {
        return "<null>".to_string();
    }

    let words = unsafe { std::slice::from_raw_parts(value.cast::<u32>(), word_count) };
    words
        .iter()
        .map(|word| format!("{word:08X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

unsafe fn dump_byte_array(value: *const c_void) -> String {
    if value.is_null() {
        return "<null>".to_string();
    }

    match unsafe { read_byte_array_fields(value) } {
        Some((start, end, cap_end)) => {
            let len = end.saturating_sub(start);
            format!(
                "start=0x{start:08X} end=0x{end:08X} cap_end=0x{cap_end:08X} len=0x{len:X}/{len}"
            )
        }
        None => "<invalid>".to_string(),
    }
}

unsafe fn dump_d3d_locked_rect(value: *const c_void) -> String {
    if value.is_null() {
        return "rect=<null>".to_string();
    }

    let base = value.cast::<u8>();
    let pitch = unsafe { std::ptr::read_unaligned(base.cast::<i32>()) };
    let bits = unsafe { std::ptr::read_unaligned(base.add(4).cast::<usize>()) };

    format!("pitch={} pBits=0x{bits:08X}", pitch)
}

unsafe fn byte_array_len(value: *const c_void) -> Option<usize> {
    let (start, end, _) = unsafe { read_byte_array_fields(value)? };
    Some(end.saturating_sub(start))
}

unsafe fn read_byte_array_fields(value: *const c_void) -> Option<(usize, usize, usize)> {
    if value.is_null() {
        return None;
    }

    let base = value.cast::<u8>();
    let start = unsafe { std::ptr::read_unaligned(base.cast::<usize>()) };
    let end = unsafe { std::ptr::read_unaligned(base.add(4).cast::<usize>()) };
    let cap_end = unsafe { std::ptr::read_unaligned(base.add(8).cast::<usize>()) };

    Some((start, end, cap_end))
}

unsafe fn read_u32_at(buffer: *const c_void, offset: usize) -> Option<u32> {
    let (start, end, _) = unsafe { read_byte_array_fields(buffer)? };
    if end.saturating_sub(start) < offset + 4 {
        return None;
    }

    Some(unsafe { std::ptr::read_unaligned((start + offset) as *const u32) })
}

unsafe fn dump_packed_buffer_sizes(buffer: *const c_void) -> String {
    let compressed_size = unsafe { read_decrypted_u32_at(buffer, 0) };
    let original_size = unsafe { read_decrypted_u32_at(buffer, 4) };

    format!(
        "decrypted_compressed_size={} decrypted_original_size={}",
        format_option_u32(compressed_size),
        format_option_u32(original_size),
    )
}

unsafe fn read_decrypted_u32_at(buffer: *const c_void, offset: usize) -> Option<u32> {
    let (start, end, _) = unsafe { read_byte_array_fields(buffer)? };
    if end.saturating_sub(start) < offset + 4 {
        return None;
    }

    let table = unsafe { tpc_angou_table()? };
    let mut bytes = [0u8; 4];
    for index in 0..4 {
        let encrypted = unsafe { *((start + offset + index) as *const u8) };
        let key = unsafe { *table.add((offset + index) % 256) };
        bytes[index] = encrypted ^ key;
    }

    Some(u32::from_le_bytes(bytes))
}

unsafe fn tpc_angou_table() -> Option<*const u8> {
    unsafe { main_module_offset(TPC_ANGOU_TABLE_OFFSET) }
        .ok()
        .map(|address| address.cast::<u8>() as *const u8)
}

unsafe fn read_cstr(value: *const c_void) -> String {
    if value.is_null() {
        return "<null>".to_string();
    }

    let base = value.cast::<u8>();
    let len = unsafe { std::ptr::read_unaligned(base.add(16).cast::<u32>()) } as usize;
    let cap = unsafe { std::ptr::read_unaligned(base.add(20).cast::<u32>()) };

    let chars = if len <= 7 {
        unsafe { std::slice::from_raw_parts(base.cast::<u16>(), len) }
    } else {
        let heap = unsafe { std::ptr::read_unaligned(base.cast::<*const u16>()) };
        if heap.is_null() {
            return format!("<null-heap len={len} cap={cap}>");
        }

        unsafe { std::slice::from_raw_parts(heap, len) }
    };

    format!("{} (len={len} cap={cap})", String::from_utf16_lossy(chars))
}

unsafe fn read_cstr_plain(value: *const c_void) -> Option<String> {
    if value.is_null() {
        return None;
    }

    let base = value.cast::<u8>();
    let len = unsafe { std::ptr::read_unaligned(base.add(16).cast::<u32>()) } as usize;

    let chars = if len <= 7 {
        unsafe { std::slice::from_raw_parts(base.cast::<u16>(), len) }
    } else {
        let heap = unsafe { std::ptr::read_unaligned(base.cast::<*const u16>()) };
        if heap.is_null() {
            return None;
        }
        unsafe { std::slice::from_raw_parts(heap, len) }
    };

    Some(String::from_utf16_lossy(chars))
}

fn save_file_name_from_cstr_log(value: &str) -> Option<&str> {
    let path = value.split(" (len=").next().unwrap_or(value);
    path.rsplit(['\\', '/']).next()
}

fn save_layout_for_file_name(value: &str) -> Option<SaveLayout> {
    let value = value.to_ascii_lowercase();
    match value.as_str() {
        "read.sav" => Some(SaveLayout {
            kind: "read",
            data_size_offset: READ_SAVE_DATA_SIZE_OFFSET,
            data_offset: READ_SAVE_DATA_OFFSET,
        }),
        "config.sav" => Some(SaveLayout {
            kind: "config",
            data_size_offset: CONFIG_SAVE_DATA_SIZE_OFFSET,
            data_offset: CONFIG_SAVE_DATA_OFFSET,
        }),
        "global.sav" => Some(SaveLayout {
            kind: "global",
            data_size_offset: GLOBAL_SAVE_DATA_SIZE_OFFSET,
            data_offset: GLOBAL_SAVE_DATA_OFFSET,
        }),
        _ => {
            let stem = value.strip_suffix(".sav")?;
            if !stem.is_empty() && stem.bytes().all(|byte| byte.is_ascii_digit()) {
                Some(SaveLayout {
                    kind: "normal",
                    data_size_offset: NORMAL_SAVE_DATA_SIZE_OFFSET,
                    data_offset: NORMAL_SAVE_DATA_OFFSET,
                })
            } else {
                None
            }
        }
    }
}

fn format_option_usize(value: Option<usize>) -> String {
    value
        .map(|value| format!("0x{value:X}/{value}"))
        .unwrap_or_else(|| "<unreadable>".to_string())
}

fn format_option_u32(value: Option<u32>) -> String {
    value
        .map(|value| format!("0x{value:X}/{value}"))
        .unwrap_or_else(|| "<unreadable>".to_string())
}

unsafe fn get_cached_file_attributes_w(file_name: PCWSTR) -> Option<u32> {
    let requested_path = unsafe { pcwstr_to_path_buf(file_name)? };

    THREAD_FILE_ATTRIBUTE_CACHE.with(|thread_cache| {
        let mut thread_cache = thread_cache.borrow_mut();
        if thread_cache.is_none() {
            if let Some(cache) = FILE_ATTRIBUTE_CACHE.get() {
                *thread_cache = Some(Arc::clone(cache));
            } else {
                return None;
            }
        }

        let cache = thread_cache.as_ref()?;
        let lookup = cache.lookup_attributes(&requested_path);

        lookup.to_option()
    })
}

unsafe fn pcwstr_to_path_buf(value: PCWSTR) -> Option<PathBuf> {
    if value.is_null() {
        return None;
    }

    let mut len = 0usize;
    while unsafe { *value.add(len) } != 0 {
        len += 1;
    }

    if len == 0 {
        return None;
    }

    Some(PathBuf::from(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(value, len)
    })))
}

#[allow(dead_code)]
unsafe fn pcwstr_to_log_string(value: PCWSTR) -> String {
    unsafe { pcwstr_to_path_buf(value) }
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "<null-or-empty>".to_string())
}

impl FileAttributeCache {
    fn lookup_attributes(&self, requested_path: &Path) -> AttributeLookup {
        if !parent_is_g00(requested_path) {
            return AttributeLookup {
                result: AttributeLookupResult::ParentNotG00,
            };
        }

        let key = match file_name_key(requested_path) {
            Some(key) => key,
            None => {
                return AttributeLookup {
                    result: AttributeLookupResult::MissingInG00,
                };
            }
        };

        let result = self
            .attributes
            .get(&key)
            .copied()
            .map(AttributeLookupResult::Hit)
            .unwrap_or(AttributeLookupResult::MissingInG00);

        AttributeLookup { result }
    }
}

impl AttributeLookup {
    fn to_option(self) -> Option<u32> {
        match self.result {
            AttributeLookupResult::Hit(attributes) => Some(attributes),
            AttributeLookupResult::MissingInG00 => {
                unsafe {
                    SetLastError(ERROR_FILE_NOT_FOUND);
                }
                Some(INVALID_FILE_ATTRIBUTES)
            }
            AttributeLookupResult::ParentNotG00 => None,
        }
    }
}

fn load_file_attribute_cache() {
    match build_file_attribute_cache() {
        Ok(cache) => {
            let file_count = cache.attributes.len();
            let g00_root = cache.g00_root.display().to_string();
            let _ = FILE_ATTRIBUTE_CACHE.set(Arc::new(cache));
            debug_log(&format!(
                "siglus_hook: cached {file_count} g00 file attributes from {g00_root}"
            ));
        }
        Err(error) => {
            debug_log(&format!(
                "siglus_hook: failed to cache g00 attributes: {error}"
            ));
        }
    }
}

fn build_file_attribute_cache() -> Result<FileAttributeCache, String> {
    let exe_path =
        attached_process_path().ok_or_else(|| "GetModuleFileNameW(NULL) failed".to_string())?;
    let exe_dir = exe_path.parent().ok_or_else(|| {
        format!(
            "attached process path has no parent: {}",
            exe_path.display()
        )
    })?;
    let g00_root = exe_dir.join("g00");
    let mut attributes = HashMap::new();

    collect_file_attributes(&g00_root, &mut attributes)
        .map_err(|error| format!("failed to scan {}: {error}", g00_root.display()))?;

    Ok(FileAttributeCache {
        g00_root,
        attributes,
    })
}

fn attached_process_path() -> Option<PathBuf> {
    let mut buffer = [0u16; 32768];
    let len = unsafe {
        GetModuleFileNameW(
            std::ptr::null_mut(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
    };

    if len == 0 {
        return None;
    }

    Some(PathBuf::from(String::from_utf16_lossy(
        &buffer[..len as usize],
    )))
}

fn collect_file_attributes(
    path: &Path,
    attributes: &mut HashMap<String, u32>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            if let Some(key) = file_name_key(&entry.path()) {
                attributes.insert(key, metadata.file_attributes());
            }
        }
    }

    Ok(())
}

fn parent_is_g00(path: &Path) -> bool {
    path.parent()
        .and_then(|parent| parent.file_name())
        .map(|name| name.to_string_lossy().eq_ignore_ascii_case("g00"))
        .unwrap_or(false)
}

fn file_name_key(path: &Path) -> Option<String> {
    path.file_name()
        .map(|file_name| file_name.to_string_lossy().to_ascii_lowercase())
}

fn debug_log(message: &str) {
    let mut wide: Vec<u16> = message.encode_utf16().collect();
    wide.push(0);

    unsafe {
        OutputDebugStringW(wide.as_ptr());
    }

    // let mut log_path = std::env::temp_dir();
    // log_path.push("siglus_hook.log");
    // if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) {
    //     let _ = writeln!(file, "{message}");
    // }
}
