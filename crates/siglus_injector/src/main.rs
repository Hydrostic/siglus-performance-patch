#![cfg(windows)]

use std::env;
use std::ffi::{c_void, CString};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, HMODULE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    WriteProcessMemory,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows_sys::Win32::System::SystemInformation::{
    IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_UNKNOWN,
};
use windows_sys::Win32::System::Threading::{
    CreateRemoteThread, GetCurrentProcess, GetExitCodeThread, IsWow64Process2, OpenProcess,
    WaitForSingleObject, INFINITE, PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION,
    PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
};

const KERNEL32_DLL: &[u16] = &[
    'k' as u16, 'e' as u16, 'r' as u16, 'n' as u16, 'e' as u16, 'l' as u16, '3' as u16, '2' as u16,
    '.' as u16, 'd' as u16, 'l' as u16, 'l' as u16, 0,
];

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args_os();
    let exe = args.next().unwrap_or_default();
    let process = args
        .next()
        .ok_or_else(|| usage(Path::new(&exe).file_name().and_then(|name| name.to_str()).unwrap_or("siglus_injector")))?;

    let dll_path = match args.next() {
        Some(path) => PathBuf::from(path),
        None => default_hook_dll_path()?,
    };

    let dll_path = dll_path
        .canonicalize()
        .map_err(|error| format!("failed to resolve DLL path {}: {error}", dll_path.display()))?;

    let pid = match process.to_string_lossy().parse::<u32>() {
        Ok(pid) => pid,
        Err(_) => find_process_id(&process.to_string_lossy())?,
    };

    let remote_module = inject_dll(pid, &dll_path)?;
    println!(
        "Injected {} into process {pid}; remote module handle: 0x{remote_module:X}",
        dll_path.display()
    );
    Ok(())
}

fn usage(exe: &str) -> String {
    format!("usage: {exe} <process-name-or-pid> [path-to-siglus_hook.dll]")
}

fn default_hook_dll_path() -> Result<PathBuf, String> {
    let exe = env::current_exe().map_err(|error| format!("failed to locate injector exe: {error}"))?;
    let dll_name = if cfg!(debug_assertions) {
        "siglus_hook.dll"
    } else {
        "siglus_hook.dll"
    };

    Ok(exe.with_file_name(dll_name))
}

fn find_process_id(process_name: &str) -> Result<u32, String> {
    let target = process_name.trim_end_matches(".exe").to_ascii_lowercase();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(format!("CreateToolhelp32Snapshot failed: {}", unsafe { GetLastError() }));
    }

    let snapshot = Handle(snapshot);
    let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

    let mut has_entry = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
    while has_entry {
        let exe_name = wide_null_terminated_to_string(&entry.szExeFile);
        let normalized = exe_name.trim_end_matches(".exe").to_ascii_lowercase();
        if normalized == target {
            return Ok(entry.th32ProcessID);
        }

        has_entry = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
    }

    Err(format!("process not found: {process_name}"))
}

fn inject_dll(pid: u32, dll_path: &Path) -> Result<u32, String> {
    let access = PROCESS_CREATE_THREAD
        | PROCESS_QUERY_INFORMATION
        | PROCESS_VM_OPERATION
        | PROCESS_VM_WRITE
        | PROCESS_VM_READ;
    let process = unsafe { OpenProcess(access, 0, pid) };
    if process.is_null() {
        return Err(format!("OpenProcess({pid}) failed: {}", unsafe { GetLastError() }));
    }
    let process = Handle(process);

    ensure_process_arch_matches(process.0, dll_path)?;

    let dll_path_wide = path_to_wide(dll_path);
    let byte_len = dll_path_wide.len() * size_of::<u16>();
    let remote_memory = unsafe {
        VirtualAllocEx(
            process.0,
            std::ptr::null(),
            byte_len,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    if remote_memory.is_null() {
        return Err(format!("VirtualAllocEx failed: {}", unsafe { GetLastError() }));
    }

    let allocation = RemoteAllocation {
        process: process.0,
        address: remote_memory,
    };

    let mut written = 0;
    let wrote = unsafe {
        WriteProcessMemory(
            process.0,
            allocation.address,
            dll_path_wide.as_ptr().cast(),
            byte_len,
            &mut written,
        )
    };
    if wrote == 0 || written != byte_len {
        return Err(format!("WriteProcessMemory failed: {}", unsafe { GetLastError() }));
    }

    let load_library = load_library_w_address()?;
    let thread_start = unsafe { std::mem::transmute(load_library) };
    let thread = unsafe {
        CreateRemoteThread(
            process.0,
            std::ptr::null(),
            0,
            Some(thread_start),
            allocation.address,
            0,
            std::ptr::null_mut(),
        )
    };
    if thread.is_null() {
        return Err(format!("CreateRemoteThread failed: {}", unsafe { GetLastError() }));
    }

    let thread = Handle(thread);
    unsafe {
        WaitForSingleObject(thread.0, INFINITE);
    }

    let mut exit_code = 0;
    if unsafe { GetExitCodeThread(thread.0, &mut exit_code) } == 0 {
        return Err(format!("GetExitCodeThread failed: {}", unsafe { GetLastError() }));
    }

    if exit_code == 0 {
        return Err(format!(
            "remote LoadLibraryW failed for {}. Check that the target can read the DLL path, the DLL matches process architecture, and required runtime/dependencies are available.",
            dll_path.display()
        ));
    }

    Ok(exit_code)
}

fn ensure_process_arch_matches(target_process: HANDLE, dll_path: &Path) -> Result<(), String> {
    let injector_arch = process_arch(unsafe { GetCurrentProcess() })?;
    let target_arch = process_arch(target_process)?;
    let dll_arch = pe_machine(dll_path)?;

    if injector_arch != target_arch {
        return Err(format!(
            "process architecture mismatch: injector is {}, target is {}. Build both DLL and injector with the matching target, e.g. `cargo build --target i686-pc-windows-msvc` for 32-bit Siglus games.",
            arch_name(injector_arch),
            arch_name(target_arch),
        ));
    }

    if dll_arch != target_arch {
        return Err(format!(
            "DLL architecture mismatch: DLL is {}, target is {}. Rebuild siglus_hook for the same target architecture.",
            arch_name(dll_arch),
            arch_name(target_arch),
        ));
    }

    Ok(())
}

fn process_arch(process: HANDLE) -> Result<u16, String> {
    let mut process_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut native_machine = IMAGE_FILE_MACHINE_UNKNOWN;

    let ok = unsafe { IsWow64Process2(process, &mut process_machine, &mut native_machine) };
    if ok == 0 {
        return Err(format!("IsWow64Process2 failed: {}", unsafe { GetLastError() }));
    }

    if process_machine == IMAGE_FILE_MACHINE_UNKNOWN {
        Ok(native_machine)
    } else {
        Ok(process_machine)
    }
}

fn arch_name(machine: u16) -> &'static str {
    match machine {
        IMAGE_FILE_MACHINE_I386 => "x86",
        IMAGE_FILE_MACHINE_AMD64 => "x64",
        _ => "unknown",
    }
}

fn pe_machine(path: &Path) -> Result<u16, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("failed to open DLL {}: {error}", path.display()))?;
    let mut dos_header = [0u8; 0x40];
    file.read_exact(&mut dos_header)
        .map_err(|error| format!("failed to read DLL DOS header {}: {error}", path.display()))?;

    if &dos_header[0..2] != b"MZ" {
        return Err(format!("DLL is not a PE file: {}", path.display()));
    }

    let pe_offset = u32::from_le_bytes([
        dos_header[0x3c],
        dos_header[0x3d],
        dos_header[0x3e],
        dos_header[0x3f],
    ]) as u64;

    file.seek(SeekFrom::Start(pe_offset))
        .map_err(|error| format!("failed to seek DLL PE header {}: {error}", path.display()))?;

    let mut coff_header = [0u8; 6];
    file.read_exact(&mut coff_header)
        .map_err(|error| format!("failed to read DLL PE header {}: {error}", path.display()))?;

    if &coff_header[0..4] != b"PE\0\0" {
        return Err(format!("DLL has an invalid PE signature: {}", path.display()));
    }

    Ok(u16::from_le_bytes([coff_header[4], coff_header[5]]))
}

fn load_library_w_address() -> Result<unsafe extern "system" fn() -> isize, String> {
    let kernel32 = unsafe { GetModuleHandleW(KERNEL32_DLL.as_ptr()) };
    if kernel32.is_null() {
        return Err(format!("GetModuleHandleW(kernel32.dll) failed: {}", unsafe {
            GetLastError()
        }));
    }

    let proc_name = CString::new("LoadLibraryW").expect("static string has no nul");
    unsafe { GetProcAddress(kernel32 as HMODULE, proc_name.as_ptr().cast()) }
        .ok_or_else(|| format!("GetProcAddress(LoadLibraryW) failed: {}", unsafe { GetLastError() }))
}

fn path_to_wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn wide_null_terminated_to_string(wide: &[u16]) -> String {
    let len = wide.iter().position(|ch| *ch == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..len])
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct RemoteAllocation {
    process: HANDLE,
    address: *mut c_void,
}

impl Drop for RemoteAllocation {
    fn drop(&mut self) {
        if !self.address.is_null() {
            unsafe {
                VirtualFreeEx(self.process, self.address, 0, MEM_RELEASE);
            }
        }
    }
}
