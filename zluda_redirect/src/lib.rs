#![cfg(target_os = "windows")]

extern crate detours_sys;
extern crate winapi;

use std::{
    ffi::c_void,
    mem,
    os::raw::{c_int, c_uint, c_ulong},
    ptr, slice, usize,
};

use detours_sys::{
    DetourAttach, DetourDetach, DetourRestoreAfterWith, DetourTransactionAbort,
    DetourTransactionBegin, DetourTransactionCommit, DetourUpdateProcessWithDll,
    DetourUpdateThread,
};
use wchar::wch;
use winapi::{
    shared::minwindef::{BOOL, LPVOID},
    um::{
        handleapi::{CloseHandle, INVALID_HANDLE_VALUE},
        minwinbase::LPSECURITY_ATTRIBUTES,
        processthreadsapi::{
            CreateProcessA, GetCurrentProcessId, GetCurrentThread, GetCurrentThreadId, OpenThread,
            ResumeThread, SuspendThread, TerminateProcess, LPPROCESS_INFORMATION, LPSTARTUPINFOA,
            LPSTARTUPINFOW,
        },
        tlhelp32::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        },
        winbase::CREATE_SUSPENDED,
        winnt::{LPSTR, LPWSTR, THREAD_SUSPEND_RESUME},
    },
};
use winapi::{
    shared::minwindef::{DWORD, FALSE, HMODULE, TRUE},
    um::{libloaderapi::LoadLibraryExA, winnt::LPCSTR},
};
use winapi::{
    shared::minwindef::{FARPROC, HINSTANCE},
    um::{
        libloaderapi::{GetModuleFileNameA, GetProcAddress},
        processthreadsapi::{CreateProcessAsUserW, CreateProcessW},
        winbase::{CreateProcessWithLogonW, CreateProcessWithTokenW},
        winnt::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, HANDLE, LPCWSTR},
    },
};
use winapi::{
    shared::winerror::NO_ERROR,
    um::libloaderapi::{LoadLibraryA, LoadLibraryExW, LoadLibraryW},
};

include!("payload_guid.rs");

const NVCUDA_UTF8: &'static str = "NVCUDA.DLL";
const NVCUDA_UTF16: &[u16] = wch!("NVCUDA.DLL");
static mut ZLUDA_PATH_UTF8: Vec<u8> = Vec::new();
static mut ZLUDA_PATH_UTF16: Option<&'static [u16]> = None;
static mut DETACH_LOAD_LIBRARY: bool = false;
static mut NVCUDA_ORIGINAL_MODULE: HMODULE = ptr::null_mut();
static mut CUINIT_ORIGINAL_FN: FARPROC = ptr::null_mut();
static mut CURRENT_MODULE_FILENAME: Vec<u8> = Vec::new();
const CUDA_ERROR_NOT_SUPPORTED: c_uint = 801;
const CUDA_ERROR_UNKNOWN: c_uint = 999;

static mut LOAD_LIBRARY_A: unsafe extern "system" fn(lpLibFileName: LPCSTR) -> HMODULE =
    LoadLibraryA;

static mut LOAD_LIBRARY_W: unsafe extern "system" fn(lpLibFileName: LPCWSTR) -> HMODULE =
    LoadLibraryW;

static mut LOAD_LIBRARY_EX_A: unsafe extern "system" fn(
    lpLibFileName: LPCSTR,
    hFile: HANDLE,
    dwFlags: DWORD,
) -> HMODULE = LoadLibraryExA;

static mut CREATE_PROCESS_A: unsafe extern "system" fn(
    lpApplicationName: LPCSTR,
    lpCommandLine: LPSTR,
    lpProcessAttributes: LPSECURITY_ATTRIBUTES,
    lpThreadAttributes: LPSECURITY_ATTRIBUTES,
    bInheritHandles: BOOL,
    dwCreationFlags: DWORD,
    lpEnvironment: LPVOID,
    lpCurrentDirectory: LPCSTR,
    lpStartupInfo: LPSTARTUPINFOA,
    lpProcessInformation: LPPROCESS_INFORMATION,
) -> BOOL = CreateProcessA;

static mut CREATE_PROCESS_W: unsafe extern "system" fn(
    lpApplicationName: LPCWSTR,
    lpCommandLine: LPWSTR,
    lpProcessAttributes: LPSECURITY_ATTRIBUTES,
    lpThreadAttributes: LPSECURITY_ATTRIBUTES,
    bInheritHandles: BOOL,
    dwCreationFlags: DWORD,
    lpEnvironment: LPVOID,
    lpCurrentDirectory: LPCWSTR,
    lpStartupInfo: LPSTARTUPINFOW,
    lpProcessInformation: LPPROCESS_INFORMATION,
) -> BOOL = CreateProcessW;

static mut CREATE_PROCESS_AS_USER_W: unsafe extern "system" fn(
    hToken: HANDLE,
    lpApplicationName: LPCWSTR,
    lpCommandLine: LPWSTR,
    lpProcessAttributes: LPSECURITY_ATTRIBUTES,
    lpThreadAttributes: LPSECURITY_ATTRIBUTES,
    bInheritHandles: BOOL,
    dwCreationFlags: DWORD,
    lpEnvironment: LPVOID,
    lpCurrentDirectory: LPCWSTR,
    lpStartupInfo: LPSTARTUPINFOW,
    lpProcessInformation: LPPROCESS_INFORMATION,
) -> BOOL = CreateProcessAsUserW;

static mut CREATE_PROCESS_WITH_TOKEN_W: unsafe extern "system" fn(
    hToken: HANDLE,
    dwLogonFlags: DWORD,
    lpApplicationName: LPCWSTR,
    lpCommandLine: LPWSTR,
    dwCreationFlags: DWORD,
    lpEnvironment: LPVOID,
    lpCurrentDirectory: LPCWSTR,
    lpStartupInfo: LPSTARTUPINFOW,
    lpProcessInformation: LPPROCESS_INFORMATION,
) -> BOOL = CreateProcessWithTokenW;

static mut CREATE_PROCESS_WITH_LOGON_W: unsafe extern "system" fn(
    lpUsername: LPCWSTR,
    lpDomain: LPCWSTR,
    lpPassword: LPCWSTR,
    dwLogonFlags: DWORD,
    lpApplicationName: LPCWSTR,
    lpCommandLine: LPWSTR,
    dwCreationFlags: DWORD,
    lpEnvironment: LPVOID,
    lpCurrentDirectory: LPCWSTR,
    lpStartupInfo: LPSTARTUPINFOW,
    lpProcessInformation: LPPROCESS_INFORMATION,
) -> BOOL = CreateProcessWithLogonW;

static mut LOAD_LIBRARY_EX_W: unsafe extern "system" fn(
    lpLibFileName: LPCWSTR,
    hFile: HANDLE,
    dwFlags: DWORD,
) -> HMODULE = LoadLibraryExW;

#[no_mangle]
#[allow(non_snake_case)]
unsafe extern "system" fn ZludaLoadLibraryW_NoRedirect(lpLibFileName: LPCWSTR) -> HMODULE {
    (LOAD_LIBRARY_W)(lpLibFileName)
}

#[allow(non_snake_case)]
unsafe extern "system" fn ZludaLoadLibraryA(lpLibFileName: LPCSTR) -> HMODULE {
    let nvcuda_file_name = if is_nvcuda_dll_utf8(lpLibFileName as *const _) {
        ZLUDA_PATH_UTF8.as_ptr() as *const _
    } else {
        lpLibFileName
    };
    (LOAD_LIBRARY_A)(nvcuda_file_name)
}

#[allow(non_snake_case)]
unsafe extern "system" fn ZludaLoadLibraryW(lpLibFileName: LPCWSTR) -> HMODULE {
    let nvcuda_file_name = if is_nvcuda_dll_utf16(lpLibFileName) {
        ZLUDA_PATH_UTF16.unwrap().as_ptr()
    } else {
        lpLibFileName
    };
    (LOAD_LIBRARY_W)(nvcuda_file_name)
}

#[allow(non_snake_case)]
unsafe extern "system" fn ZludaLoadLibraryExA(
    lpLibFileName: LPCSTR,
    hFile: HANDLE,
    dwFlags: DWORD,
) -> HMODULE {
    let nvcuda_file_name = if is_nvcuda_dll_utf8(lpLibFileName as *const _) {
        ZLUDA_PATH_UTF8.as_ptr() as *const _
    } else {
        lpLibFileName
    };
    (LOAD_LIBRARY_EX_A)(nvcuda_file_name, hFile, dwFlags)
}

#[allow(non_snake_case)]
unsafe extern "system" fn ZludaLoadLibraryExW(
    lpLibFileName: LPCWSTR,
    hFile: HANDLE,
    dwFlags: DWORD,
) -> HMODULE {
    let nvcuda_file_name = if is_nvcuda_dll_utf16(lpLibFileName) {
        ZLUDA_PATH_UTF16.unwrap().as_ptr()
    } else {
        lpLibFileName
    };
    (LOAD_LIBRARY_EX_W)(nvcuda_file_name, hFile, dwFlags)
}

#[allow(non_snake_case)]
unsafe extern "system" fn ZludaCreateProcessA(
    lpApplicationName: LPCSTR,
    lpCommandLine: LPSTR,
    lpProcessAttributes: LPSECURITY_ATTRIBUTES,
    lpThreadAttributes: LPSECURITY_ATTRIBUTES,
    bInheritHandles: BOOL,
    dwCreationFlags: DWORD,
    lpEnvironment: LPVOID,
    lpCurrentDirectory: LPCSTR,
    lpStartupInfo: LPSTARTUPINFOA,
    lpProcessInformation: LPPROCESS_INFORMATION,
) -> BOOL {
    let create_proc_result = CREATE_PROCESS_A(
        lpApplicationName,
        lpCommandLine,
        lpProcessAttributes,
        lpThreadAttributes,
        bInheritHandles,
        dwCreationFlags | CREATE_SUSPENDED,
        lpEnvironment,
        lpCurrentDirectory,
        lpStartupInfo,
        lpProcessInformation,
    );
    continue_create_process_hook(create_proc_result, dwCreationFlags, lpProcessInformation)
}

#[allow(non_snake_case)]
unsafe extern "system" fn ZludaCreateProcessW(
    lpApplicationName: LPCWSTR,
    lpCommandLine: LPWSTR,
    lpProcessAttributes: LPSECURITY_ATTRIBUTES,
    lpThreadAttributes: LPSECURITY_ATTRIBUTES,
    bInheritHandles: BOOL,
    dwCreationFlags: DWORD,
    lpEnvironment: LPVOID,
    lpCurrentDirectory: LPCWSTR,
    lpStartupInfo: LPSTARTUPINFOW,
    lpProcessInformation: LPPROCESS_INFORMATION,
) -> BOOL {
    let create_proc_result = CREATE_PROCESS_W(
        lpApplicationName,
        lpCommandLine,
        lpProcessAttributes,
        lpThreadAttributes,
        bInheritHandles,
        dwCreationFlags | CREATE_SUSPENDED,
        lpEnvironment,
        lpCurrentDirectory,
        lpStartupInfo,
        lpProcessInformation,
    );
    continue_create_process_hook(create_proc_result, dwCreationFlags, lpProcessInformation)
}

#[allow(non_snake_case)]
unsafe extern "system" fn ZludaCreateProcessAsUserW(
    hToken: HANDLE,
    lpApplicationName: LPCWSTR,
    lpCommandLine: LPWSTR,
    lpProcessAttributes: LPSECURITY_ATTRIBUTES,
    lpThreadAttributes: LPSECURITY_ATTRIBUTES,
    bInheritHandles: BOOL,
    dwCreationFlags: DWORD,
    lpEnvironment: LPVOID,
    lpCurrentDirectory: LPCWSTR,
    lpStartupInfo: LPSTARTUPINFOW,
    lpProcessInformation: LPPROCESS_INFORMATION,
) -> BOOL {
    let create_proc_result = CREATE_PROCESS_AS_USER_W(
        hToken,
        lpApplicationName,
        lpCommandLine,
        lpProcessAttributes,
        lpThreadAttributes,
        bInheritHandles,
        dwCreationFlags | CREATE_SUSPENDED,
        lpEnvironment,
        lpCurrentDirectory,
        lpStartupInfo,
        lpProcessInformation,
    );
    continue_create_process_hook(create_proc_result, dwCreationFlags, lpProcessInformation)
}

#[allow(non_snake_case)]
unsafe extern "system" fn ZludaCreateProcessWithLogonW(
    lpUsername: LPCWSTR,
    lpDomain: LPCWSTR,
    lpPassword: LPCWSTR,
    dwLogonFlags: DWORD,
    lpApplicationName: LPCWSTR,
    lpCommandLine: LPWSTR,
    dwCreationFlags: DWORD,
    lpEnvironment: LPVOID,
    lpCurrentDirectory: LPCWSTR,
    lpStartupInfo: LPSTARTUPINFOW,
    lpProcessInformation: LPPROCESS_INFORMATION,
) -> BOOL {
    let create_proc_result = CREATE_PROCESS_WITH_LOGON_W(
        lpUsername,
        lpDomain,
        lpPassword,
        dwLogonFlags,
        lpApplicationName,
        lpCommandLine,
        dwCreationFlags | CREATE_SUSPENDED,
        lpEnvironment,
        lpCurrentDirectory,
        lpStartupInfo,
        lpProcessInformation,
    );
    continue_create_process_hook(create_proc_result, dwCreationFlags, lpProcessInformation)
}

#[allow(non_snake_case)]
unsafe extern "system" fn ZludaCreateProcessWithTokenW(
    hToken: HANDLE,
    dwLogonFlags: DWORD,
    lpApplicationName: LPCWSTR,
    lpCommandLine: LPWSTR,
    dwCreationFlags: DWORD,
    lpEnvironment: LPVOID,
    lpCurrentDirectory: LPCWSTR,
    lpStartupInfo: LPSTARTUPINFOW,
    lpProcessInformation: LPPROCESS_INFORMATION,
) -> BOOL {
    let create_proc_result = CREATE_PROCESS_WITH_TOKEN_W(
        hToken,
        dwLogonFlags,
        lpApplicationName,
        lpCommandLine,
        dwCreationFlags,
        lpEnvironment,
        lpCurrentDirectory,
        lpStartupInfo,
        lpProcessInformation,
    );
    continue_create_process_hook(create_proc_result, dwCreationFlags, lpProcessInformation)
}

unsafe fn continue_create_process_hook(
    create_proc_result: BOOL,
    creation_flags: DWORD,
    process_information: LPPROCESS_INFORMATION,
) -> BOOL {
    if create_proc_result == 0 {
        return 0;
    }
    if DetourUpdateProcessWithDll(
        (*process_information).hProcess,
        &mut CURRENT_MODULE_FILENAME.as_ptr() as *mut _ as *mut _,
        1,
    ) == 0
    {
        TerminateProcess((*process_information).hProcess, 1);
        return 0;
    }
    if detours_sys::DetourCopyPayloadToProcess(
        (*process_information).hProcess,
        &PAYLOAD_GUID,
        ZLUDA_PATH_UTF16.unwrap().as_ptr() as *mut _,
        (ZLUDA_PATH_UTF16.unwrap().len() * mem::size_of::<u16>()) as u32,
    ) == FALSE
    {
        TerminateProcess((*process_information).hProcess, 1);
        return 0;
    }

    if creation_flags & CREATE_SUSPENDED == 0 {
        if ResumeThread((*process_information).hThread) == -1i32 as u32 {
            TerminateProcess((*process_information).hProcess, 1);
            return 0;
        }
    }
    create_proc_result
}

unsafe extern "C" fn cuinit_detour(flags: c_uint) -> c_uint {
    let zluda_module = LoadLibraryW(ZLUDA_PATH_UTF16.unwrap().as_ptr());
    if zluda_module == ptr::null_mut() {
        return CUDA_ERROR_UNKNOWN;
    }
    let suspended_threads = suspend_all_threads_except_current();
    let suspended_threads = match suspended_threads {
        Some(t) => t,
        None => return CUDA_ERROR_UNKNOWN,
    };
    if DetourTransactionBegin() != NO_ERROR as i32 {
        resume_threads(&suspended_threads);
        return CUDA_ERROR_UNKNOWN;
    }
    for t in suspended_threads.iter() {
        if DetourUpdateThread(*t) != NO_ERROR as i32 {
            DetourTransactionAbort();
            resume_threads(&suspended_threads);
            return CUDA_ERROR_UNKNOWN;
        }
    }
    if detours_sys::DetourEnumerateExports(
        NVCUDA_ORIGINAL_MODULE as *mut _,
        &zluda_module as *const _ as *mut _,
        Some(override_nvcuda_export),
    ) == FALSE
    {
        DetourTransactionAbort();
        resume_threads(&suspended_threads);
        return CUDA_ERROR_UNKNOWN;
    }
    if DetourTransactionCommit() != NO_ERROR as i32 {
        DetourTransactionAbort();
        resume_threads(&suspended_threads);
        return CUDA_ERROR_UNKNOWN;
    }
    resume_threads(&suspended_threads);
    let zluda_cuinit = GetProcAddress(zluda_module, b"cuInit\0".as_ptr() as *const _);
    (mem::transmute::<_, unsafe extern "C" fn(c_uint) -> c_uint>(zluda_cuinit))(flags)
}

unsafe fn suspend_all_threads_except_current() -> Option<Vec<*mut c_void>> {
    let thread_snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
    if thread_snap == INVALID_HANDLE_VALUE {
        return None;
    }
    let current_thread = GetCurrentThreadId();
    let current_process = GetCurrentProcessId();
    let mut threads = Vec::new();
    let mut thread = mem::zeroed::<THREADENTRY32>();
    thread.dwSize = mem::size_of::<THREADENTRY32>() as u32;
    if Thread32First(thread_snap, &mut thread) == 0 {
        CloseHandle(thread_snap);
        return None;
    }
    loop {
        if thread.th32OwnerProcessID == current_process && thread.th32ThreadID != current_thread {
            let thread_handle = OpenThread(THREAD_SUSPEND_RESUME, 0, thread.th32ThreadID);
            if thread_handle == ptr::null_mut() {
                CloseHandle(thread_snap);
                resume_threads(&threads);
                return None;
            }
            if SuspendThread(thread_handle) == (-1i32 as u32) {
                CloseHandle(thread_snap);
                resume_threads(&threads);
                return None;
            }
            threads.push(thread_handle);
        }
        if Thread32Next(thread_snap, &mut thread) == 0 {
            break;
        }
    }
    CloseHandle(thread_snap);
    Some(threads)
}

unsafe fn resume_threads(threads: &[*mut c_void]) {
    for t in threads {
        ResumeThread(*t);
        CloseHandle(*t);
    }
}

unsafe extern "C" fn override_nvcuda_export(
    context_ptr: *mut c_void,
    _: c_ulong,
    name: LPCSTR,
    mut address: *mut c_void,
) -> c_int {
    let zluda_module: HMODULE = *(context_ptr as *mut HMODULE);
    let mut zluda_fn = GetProcAddress(zluda_module, name);
    if zluda_fn == ptr::null_mut() {
        // We only support 64 bits and in all relevant calling conventions stack
        // is caller-cleaned, so probably we will not crash
        zluda_fn = unsupported_cuda_fn as *mut _;
    }
    if DetourAttach((&mut address) as *mut _, zluda_fn as *mut _) != NO_ERROR as i32 {
        return FALSE;
    }
    TRUE
}

unsafe extern "C" fn unsupported_cuda_fn() -> c_uint {
    CUDA_ERROR_NOT_SUPPORTED
}

fn is_nvcuda_dll_utf8(lib: *const u8) -> bool {
    is_nvcuda_dll(lib, 0, NVCUDA_UTF8.as_bytes(), |c| {
        if c >= 'a' as u8 && c <= 'z' as u8 {
            c - 32
        } else {
            c
        }
    })
}
fn is_nvcuda_dll_utf16(lib: *const u16) -> bool {
    is_nvcuda_dll(lib, 0u16, NVCUDA_UTF16, |c| {
        if c >= 'a' as u16 && c <= 'z' as u16 {
            c - 32
        } else {
            c
        }
    })
}

fn is_nvcuda_dll<T: Copy + PartialEq>(
    lib: *const T,
    zero: T,
    dll_name: &[T],
    uppercase: impl Fn(T) -> T,
) -> bool {
    let mut len = 0;
    loop {
        if unsafe { *lib.offset(len) } == zero {
            break;
        }
        len += 1;
    }
    if (len as usize) < dll_name.len() {
        return false;
    }
    let slice =
        unsafe { slice::from_raw_parts(lib.offset(len - dll_name.len() as isize), dll_name.len()) };
    for i in 0..dll_name.len() {
        if uppercase(slice[i]) != dll_name[i] {
            return false;
        }
    }
    true
}

#[allow(non_snake_case)]
#[no_mangle]
unsafe extern "system" fn DllMain(instDLL: HINSTANCE, dwReason: u32, _: *const u8) -> i32 {
    if dwReason == DLL_PROCESS_ATTACH {
        if DetourRestoreAfterWith() == FALSE {
            return FALSE;
        }
        if !initialize_current_module_name(instDLL) {
            return FALSE;
        }
        match get_zluda_dll_path() {
            Some(path) => {
                ZLUDA_PATH_UTF16 = Some(path);
                // from_utf16_lossy(...) handles terminating NULL correctly
                ZLUDA_PATH_UTF8 = String::from_utf16_lossy(path).into_bytes();
            }
            None => return FALSE,
        }
        // If the application (directly or not) links to nvcuda.dll, nvcuda.dll
        // will get loaded before we can act. In this case, instead of
        // redirecting LoadLibrary* to load ZLUDA, we redirect cuInit to
        // a cuInit implementation that will load ZLUDA and set up detouts.
        // We can't do it here because LoadLibrary* inside DllMain is illegal.
        // We greatly prefer wholesale redirecting inside LoadLibrary*.
        // Hooking inside cuInit is brittle in the face of multiple
        // threads (DetourUpdateThread)
        match get_cuinit() {
            Some((nvcuda_mod, cuinit_fn)) => attach_cuinit(nvcuda_mod, cuinit_fn),
            None => attach_load_libary(),
        }
    } else if dwReason == DLL_PROCESS_DETACH {
        if DETACH_LOAD_LIBRARY {
            detach_load_library()
        } else {
            detach_cuinit()
        }
    } else {
        TRUE
    }
}

#[must_use]
unsafe fn initialize_current_module_name(current_module: HINSTANCE) -> bool {
    let mut name = vec![0; 128 as usize];
    loop {
        let size = GetModuleFileNameA(
            current_module,
            name.as_mut_ptr() as *mut _,
            name.len() as u32,
        );
        if size == 0 {
            return false;
        }
        if size < name.len() as u32 {
            name.truncate(size as usize);
            CURRENT_MODULE_FILENAME = name;
            return true;
        }
        name.resize(name.len() * 2, 0);
    }
}

unsafe fn get_cuinit() -> Option<(HMODULE, FARPROC)> {
    let mut module = ptr::null_mut();
    loop {
        module = detours_sys::DetourEnumerateModules(module);
        if module == ptr::null_mut() {
            return None;
        }
        let cuinit_addr = GetProcAddress(module as *mut _, b"cuInit\0".as_ptr() as *const _);
        if cuinit_addr != ptr::null_mut() {
            return Some((module as *mut _, cuinit_addr));
        }
    }
}

#[must_use]
unsafe fn attach_cuinit(nvcuda_mod: HMODULE, mut cuinit: FARPROC) -> i32 {
    if DetourTransactionBegin() != NO_ERROR as i32 {
        return FALSE;
    }
    if !attach_create_process() {
        return FALSE;
    }
    NVCUDA_ORIGINAL_MODULE = nvcuda_mod;
    CUINIT_ORIGINAL_FN = cuinit;
    if DetourAttach(mem::transmute(&mut cuinit), cuinit_detour as *mut _) != NO_ERROR as i32 {
        return FALSE;
    }
    if DetourTransactionCommit() != NO_ERROR as i32 {
        return FALSE;
    }
    TRUE
}

#[must_use]
unsafe fn detach_cuinit() -> i32 {
    if DetourTransactionBegin() != NO_ERROR as i32 {
        return FALSE;
    }
    if !detach_create_process() {
        return FALSE;
    }
    if DetourUpdateThread(GetCurrentThread()) != NO_ERROR as i32 {
        return FALSE;
    }
    if DetourDetach(
        mem::transmute(&mut CUINIT_ORIGINAL_FN),
        cuinit_detour as *mut _,
    ) != NO_ERROR as i32
    {
        return FALSE;
    }
    if DetourTransactionCommit() != NO_ERROR as i32 {
        return FALSE;
    }
    TRUE
}

#[must_use]
unsafe fn attach_load_libary() -> i32 {
    if DetourTransactionBegin() != NO_ERROR as i32 {
        return FALSE;
    }
    if !attach_create_process() {
        return FALSE;
    }
    if DetourAttach(
        mem::transmute(&mut LOAD_LIBRARY_A),
        ZludaLoadLibraryA as *mut _,
    ) != NO_ERROR as i32
    {
        return FALSE;
    }
    if DetourAttach(
        mem::transmute(&mut LOAD_LIBRARY_W),
        ZludaLoadLibraryW as *mut _,
    ) != NO_ERROR as i32
    {
        return FALSE;
    }
    if DetourAttach(
        mem::transmute(&mut LOAD_LIBRARY_EX_A),
        ZludaLoadLibraryExA as *mut _,
    ) != NO_ERROR as i32
    {
        return FALSE;
    }
    if DetourAttach(
        mem::transmute(&mut LOAD_LIBRARY_EX_W),
        ZludaLoadLibraryExW as *mut _,
    ) != NO_ERROR as i32
    {
        return FALSE;
    }
    if DetourTransactionCommit() != NO_ERROR as i32 {
        return FALSE;
    }
    TRUE
}

#[must_use]
unsafe fn detach_load_library() -> i32 {
    if DetourTransactionBegin() != NO_ERROR as i32 {
        return FALSE;
    }
    if !detach_create_process() {
        return FALSE;
    }
    if DetourUpdateThread(GetCurrentThread()) != NO_ERROR as i32 {
        return FALSE;
    }
    if DetourDetach(
        mem::transmute(&mut LOAD_LIBRARY_A),
        ZludaLoadLibraryA as *mut _,
    ) != NO_ERROR as i32
    {
        return FALSE;
    }
    if DetourDetach(
        mem::transmute(&mut LOAD_LIBRARY_W),
        ZludaLoadLibraryW as *mut _,
    ) != NO_ERROR as i32
    {
        return FALSE;
    }
    if DetourDetach(
        mem::transmute(&mut LOAD_LIBRARY_EX_A),
        ZludaLoadLibraryExA as *mut _,
    ) != NO_ERROR as i32
    {
        return FALSE;
    }
    if DetourDetach(
        mem::transmute(&mut LOAD_LIBRARY_EX_W),
        ZludaLoadLibraryExW as *mut _,
    ) != NO_ERROR as i32
    {
        return FALSE;
    }
    if DetourTransactionCommit() != NO_ERROR as i32 {
        return FALSE;
    }
    TRUE
}

fn get_zluda_dll_path() -> Option<&'static [u16]> {
    let mut module = ptr::null_mut();
    loop {
        module = unsafe { detours_sys::DetourEnumerateModules(module) };
        if module == ptr::null_mut() {
            break;
        }
        let mut size = 0;
        let payload = unsafe { detours_sys::DetourFindPayload(module, &PAYLOAD_GUID, &mut size) };
        if payload != ptr::null_mut() {
            return unsafe {
                Some(slice::from_raw_parts(
                    payload as *const _,
                    (size as usize) / mem::size_of::<u16>(),
                ))
            };
        }
    }
    None
}

#[must_use]
unsafe fn attach_create_process() -> bool {
    if DetourAttach(
        mem::transmute(&mut CREATE_PROCESS_A),
        ZludaCreateProcessA as *mut _,
    ) != NO_ERROR as i32
    {
        return false;
    }
    if DetourAttach(
        mem::transmute(&mut CREATE_PROCESS_W),
        ZludaCreateProcessW as *mut _,
    ) != NO_ERROR as i32
    {
        return false;
    }
    if DetourAttach(
        mem::transmute(&mut CREATE_PROCESS_AS_USER_W),
        ZludaCreateProcessAsUserW as *mut _,
    ) != NO_ERROR as i32
    {
        return false;
    }
    if DetourAttach(
        mem::transmute(&mut CREATE_PROCESS_WITH_LOGON_W),
        ZludaCreateProcessWithLogonW as *mut _,
    ) != NO_ERROR as i32
    {
        return false;
    }
    if DetourAttach(
        mem::transmute(&mut CREATE_PROCESS_WITH_TOKEN_W),
        ZludaCreateProcessWithTokenW as *mut _,
    ) != NO_ERROR as i32
    {
        return false;
    }
    true
}

#[must_use]
unsafe fn detach_create_process() -> bool {
    if DetourDetach(
        mem::transmute(&mut CREATE_PROCESS_A),
        ZludaCreateProcessA as *mut _,
    ) != NO_ERROR as i32
    {
        return false;
    }
    if DetourDetach(
        mem::transmute(&mut CREATE_PROCESS_W),
        ZludaCreateProcessW as *mut _,
    ) != NO_ERROR as i32
    {
        return false;
    }
    if DetourDetach(
        mem::transmute(&mut CREATE_PROCESS_AS_USER_W),
        ZludaCreateProcessAsUserW as *mut _,
    ) != NO_ERROR as i32
    {
        return false;
    }
    if DetourDetach(
        mem::transmute(&mut CREATE_PROCESS_WITH_LOGON_W),
        ZludaCreateProcessWithLogonW as *mut _,
    ) != NO_ERROR as i32
    {
        return false;
    }
    if DetourDetach(
        mem::transmute(&mut CREATE_PROCESS_WITH_TOKEN_W),
        ZludaCreateProcessWithTokenW as *mut _,
    ) != NO_ERROR as i32
    {
        return false;
    }
    true
}
