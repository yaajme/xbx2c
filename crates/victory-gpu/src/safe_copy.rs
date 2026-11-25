//! A copy out of memory that may not be there: a plain copy whose fault, if
//! a page of the source is missing, ends it with `false`. The kernel's
//! `ReadProcessMemory` says the same at a microsecond or more a call, and
//! some 13,000 constant buffers of 500 bytes a frame are copied that way.
//! `VICTORY_SAFE_COPY=1`; off by default.
//!
//! The copy is one instruction at a known address. A vectored handler, ahead
//! of the others, sends a read fault there on to the routine's failing
//! return. A write fault is left to the handlers behind it, and nothing of
//! ours is guarded (the destination is host memory).

use std::sync::OnceLock;

core::arch::global_asm!(
    ".globl victory_safe_copy",
    ".globl victory_safe_copy_at",
    ".globl victory_safe_copy_failed",
    // (destination rcx, source rdx, bytes r8) -> rax: 1 copied, 0 a fault.
    // rsi and rdi are the caller's: kept in volatile registers.
    "victory_safe_copy:",
    "mov r9, rdi",
    "mov r10, rsi",
    "mov rdi, rcx",
    "mov rsi, rdx",
    "mov rcx, r8",
    "victory_safe_copy_at:",
    "rep movsb",
    "mov rdi, r9",
    "mov rsi, r10",
    "mov eax, 1",
    "ret",
    "victory_safe_copy_failed:",
    "mov rdi, r9",
    "mov rsi, r10",
    "xor eax, eax",
    "ret",
);

unsafe extern "system" {
    fn victory_safe_copy(dst: *mut u8, src: *const u8, len: usize) -> u64;
    fn victory_safe_copy_at();
    fn victory_safe_copy_failed();
}

/// Whether the copies are made here; registers the handler the first time.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SAFE_COPY").is_some_and(|v| v == "1") && install())
}

fn install() -> bool {
    use windows_sys::Win32::Foundation::{
        EXCEPTION_ACCESS_VIOLATION, EXCEPTION_GUARD_PAGE, EXCEPTION_IN_PAGE_ERROR, NTSTATUS,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_read(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let (record, context) = unsafe { (&*(*info).ExceptionRecord, &mut *(*info).ContextRecord) };
        if context.Rip != victory_safe_copy_at as *const () as u64 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let code = record.ExceptionCode;
        let fault = code == EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || code == EXCEPTION_IN_PAGE_ERROR as NTSTATUS
            || code == EXCEPTION_GUARD_PAGE as NTSTATUS;
        // A write fault (1) is another handler's.
        if !fault || record.NumberParameters < 1 || record.ExceptionInformation[0] == 1 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        context.Rip = victory_safe_copy_failed as *const () as u64;
        EXCEPTION_CONTINUE_EXECUTION
    }
    // SAFETY: a handler that lives as long as the process.
    let on = unsafe { !AddVectoredExceptionHandler(1, Some(on_read)).is_null() };
    tracing::info!(on, "title memory is read in place, faults caught (VICTORY_SAFE_COPY)");
    on
}

/// Copies `len` bytes from `src` to `dst`; `false` if a page of `src` is not
/// readable (some bytes may have been copied then).
///
/// # Safety
/// `dst` is writable for `len` bytes; the two do not overlap; `on` is true.
pub(crate) unsafe fn copy(dst: *mut u8, src: *const u8, len: usize) -> bool {
    // SAFETY: the caller's, and the handler takes faults of the source.
    unsafe { victory_safe_copy(dst, src, len) != 0 }
}

#[cfg(test)]
mod tests {
    /// Bytes arrive, a missing page is `false` and not a crash, and so is a
    /// copy that starts in a page that is there and runs into one that is not.
    #[test]
    fn a_missing_source_page_ends_the_copy() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        assert!(super::install());
        let source: Vec<u8> = (0..70_000u32).map(|i| (i * 7) as u8).collect();
        let mut to = vec![0u8; source.len()];
        // SAFETY: two live buffers of this length.
        assert!(unsafe { super::copy(to.as_mut_ptr(), source.as_ptr(), source.len()) });
        assert_eq!(to, source);
        // SAFETY: two pages reserved, the first committed.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), 0x2000, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!VirtualAlloc(base.cast(), 0x1000, MEM_COMMIT, PAGE_READWRITE).is_null());
            base.write_bytes(5, 0x1000);
            assert!(super::copy(to.as_mut_ptr(), base, 0x1000));
            assert!(to[..0x1000].iter().all(|&b| b == 5));
            assert!(!super::copy(to.as_mut_ptr(), base.add(0xF00), 0x200));
            assert!(!super::copy(to.as_mut_ptr(), base.add(0x1000), 16));
            assert!(!super::copy(to.as_mut_ptr(), 16 as *const u8, 16));
            VirtualFree(base.cast(), 0, MEM_RELEASE);
        }
    }
}
