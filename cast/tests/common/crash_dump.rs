//! Writes a minidump (every thread's stack and the memory it points to) to
//! `target/crash-dumps/<test binary>-<pid>.dmp` under the crate when a test
//! binary dies of an unhandled exception such as an access violation, while
//! the folder holds fewer than three dumps. Test binaries only: desktop's tests compile it in too.
//! The handler is installed before `main` by the C runtime's initializer
//! table, so no test has to call it. `std::process::abort` (a fail-fast)
//! bypasses it.

#![cfg(windows)]

use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS, MINIDUMP_EXCEPTION_INFORMATION,
    MiniDumpWithDataSegs, MiniDumpWithIndirectlyReferencedMemory, MiniDumpWithThreadInfo,
    MiniDumpWriteDump, SetUnhandledExceptionFilter,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId,
};

// Built at start-up: the crashing process may have a corrupt heap.
static DUMP_PATH: OnceLock<Vec<u16>> = OnceLock::new();

#[used]
#[unsafe(link_section = ".CRT$XCU")]
static INSTALL: extern "C" fn() = install;

extern "C" fn install() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/target/crash-dumps");
    let binary = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.file_stem()?.to_string_lossy().into_owned()))
        .unwrap_or_default();
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // A crash loop must not fill the disk: three dumps are enough to study.
    let dumps = std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| e.path().extension() == Some("dmp".as_ref()))
            })
            .count()
    });
    if dumps >= 3 {
        return;
    }
    let path = format!("{dir}/{binary}-{}.dmp", std::process::id());
    let _ = DUMP_PATH.set(path.encode_utf16().chain([0]).collect());
    unsafe { SetUnhandledExceptionFilter(Some(write_dump)) };
}

unsafe extern "system" fn write_dump(pointers: *const EXCEPTION_POINTERS) -> i32 {
    let Some(path) = DUMP_PATH.get() else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let exception = MINIDUMP_EXCEPTION_INFORMATION {
        ThreadId: unsafe { GetCurrentThreadId() },
        ExceptionPointers: pointers.cast_mut(),
        ClientPointers: 0,
    };
    // Written from inside the process, the dump fails now and then with a
    // partial memory read (half the first tries on the PC, never a second)
    // and leaves a file without its stream directory; a retry starts it over.
    for _ in 0..4 {
        let written = unsafe {
            let file = CreateFileW(
                path.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ,
                std::ptr::null(),
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            );
            if file == INVALID_HANDLE_VALUE {
                break;
            }
            let written = MiniDumpWriteDump(
                GetCurrentProcess(),
                GetCurrentProcessId(),
                file,
                MiniDumpWithDataSegs
                    | MiniDumpWithIndirectlyReferencedMemory
                    | MiniDumpWithThreadInfo,
                &exception,
                std::ptr::null(),
                std::ptr::null(),
            );
            CloseHandle(file);
            written
        };
        if written != 0 {
            break;
        }
    }
    // The process still dies as before, so the loop sees the same exit code.
    EXCEPTION_CONTINUE_SEARCH
}
