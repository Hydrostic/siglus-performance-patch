#![cfg(windows)]

use std::ffi::c_void;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use minhook::{MinHook, MH_STATUS};
use windows_sys::Win32::Foundation::{
    SetLastError, BOOL, ERROR_FILE_NOT_FOUND, HINSTANCE, TRUE,
};
#[allow(unused_imports)]
use windows_sys::Win32::Storage::FileSystem::{
    CopyFileW, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
};
use windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows_sys::Win32::System::LibraryLoader::{
    DisableThreadLibraryCalls, GetModuleFileNameW, GetModuleHandleW,
};
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_sys::Win32::System::Threading::CreateThread;
use windows_sys::core::PCWSTR;

type GetFileAttributesWFn = unsafe extern "system" fn(PCWSTR) -> u32;
#[allow(dead_code)]
type CopyFileWFn = unsafe extern "system" fn(PCWSTR, PCWSTR, BOOL) -> BOOL;
type TnmSaveToFileFn = unsafe extern "fastcall" fn(*const c_void, *const c_void) -> bool;
type TnmPackBufferFn = unsafe extern "fastcall" fn(*const c_void) -> bool;

const TNM_SAVE_TO_FILE_OFFSET: usize = 0x25DE60;
const TNM_PACK_BUFFER_OFFSET: usize = 0x25E120;

static ORIGINAL_GET_FILE_ATTRIBUTES_W: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
#[allow(dead_code)]
static ORIGINAL_COPY_FILE_W: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ORIGINAL_TNM_SAVE_TO_FILE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ORIGINAL_TNM_PACK_BUFFER: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static GET_FILE_ATTRIBUTES_W_HITS: AtomicUsize = AtomicUsize::new(0);
static FILE_ATTRIBUTE_CACHE: OnceLock<Arc<FileAttributeCache>> = OnceLock::new();

thread_local! {
    static THREAD_FILE_ATTRIBUTE_CACHE: RefCell<Option<Arc<FileAttributeCache>>> = RefCell::new(None);
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

    // let original_copy_file_w = unsafe {
    //     MinHook::create_hook(
    //         CopyFileW as *mut c_void,
    //         detour_copy_file_w as *mut c_void,
    //     )?
    // };
    // ORIGINAL_COPY_FILE_W.store(original_copy_file_w, Ordering::Release);

    let tnm_save_to_file = unsafe { main_module_offset(TNM_SAVE_TO_FILE_OFFSET)? };
    let original_tnm_save_to_file = unsafe {
        MinHook::create_hook(
            tnm_save_to_file,
            detour_tnm_save_to_file as *mut c_void,
        )?
    };
    ORIGINAL_TNM_SAVE_TO_FILE.store(original_tnm_save_to_file, Ordering::Release);

    let tnm_pack_buffer = unsafe { main_module_offset(TNM_PACK_BUFFER_OFFSET)? };
    let original_tnm_pack_buffer = unsafe {
        MinHook::create_hook(
            tnm_pack_buffer,
            detour_tnm_pack_buffer as *mut c_void,
        )?
    };
    ORIGINAL_TNM_PACK_BUFFER.store(original_tnm_pack_buffer, Ordering::Release);

    unsafe { MinHook::enable_all_hooks()? };
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

#[allow(dead_code)]
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

#[allow(dead_code)]
unsafe fn copy_file_w_hook_body(
    existing_file_name: PCWSTR,
    new_file_name: PCWSTR,
    fail_if_exists: BOOL,
) -> BOOL {
    debug_log(&format!(
        "siglus_hook: CopyFileW existing=\"{}\" new=\"{}\" fail_if_exists={}",
        unsafe { pcwstr_to_log_string(existing_file_name) },
        unsafe { pcwstr_to_log_string(new_file_name) },
        fail_if_exists,
    ));

    // unsafe { call_original_copy_file_w(existing_file_name, new_file_name, fail_if_exists) }
    TRUE
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

unsafe fn tnm_save_to_file_hook_body(
    file_path: *const c_void,
    write_data: *const c_void,
) -> bool {
    // let file_path_raw = unsafe { dump_u32_words(file_path, 6) };
    debug_log(&format!(
        "siglus_hook: tnm_save_to_file file_path=\"{}\" write_data={}",
        unsafe { read_cstr(file_path) },
        unsafe { dump_byte_array(write_data) },
    ));

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
    debug_log(&format!(
        "siglus_hook: tnm_pack_buffer src={}",
        unsafe { dump_byte_array(src) },
    ));

    unsafe { call_original_tnm_pack_buffer(src) }
}

unsafe fn call_original_tnm_pack_buffer(src: *const c_void) -> bool {
    let original = ORIGINAL_TNM_PACK_BUFFER.load(Ordering::Acquire);
    if original.is_null() {
        return false;
    }

    let original: TnmPackBufferFn = unsafe { std::mem::transmute(original) };
    unsafe { original(src) }
}

unsafe fn main_module_offset(offset: usize) -> Result<*mut c_void, MH_STATUS> {
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    if module.is_null() {
        return Err(MH_STATUS::MH_ERROR_MODULE_NOT_FOUND);
    }

    Ok((module as usize + offset) as *mut c_void)
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

    let base = value.cast::<u8>();
    let start = unsafe { std::ptr::read_unaligned(base.cast::<usize>()) };
    let end = unsafe { std::ptr::read_unaligned(base.add(4).cast::<usize>()) };
    let cap_end = unsafe { std::ptr::read_unaligned(base.add(8).cast::<usize>()) };
    let len = end.saturating_sub(start);

    format!(
        "start=0x{start:08X} end=0x{end:08X} cap_end=0x{cap_end:08X} len=0x{len:X}/{len}"
    )
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

        let result = self.attributes
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
            debug_log(&format!("siglus_hook: failed to cache g00 attributes: {error}"));
        }
    }
}

fn build_file_attribute_cache() -> Result<FileAttributeCache, String> {
    let exe_path = attached_process_path()
        .ok_or_else(|| "GetModuleFileNameW(NULL) failed".to_string())?;
    let exe_dir = exe_path
        .parent()
        .ok_or_else(|| format!("attached process path has no parent: {}", exe_path.display()))?;
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

    Some(PathBuf::from(String::from_utf16_lossy(&buffer[..len as usize])))
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

    let mut log_path = std::env::temp_dir();
    log_path.push("siglus_hook.log");
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = writeln!(file, "{message}");
    }
}
