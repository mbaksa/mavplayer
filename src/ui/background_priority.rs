//! Best-effort scheduling preference for media analysis, not playback.
pub(super) fn lower() {
    #[cfg(target_os = "linux")]
    unsafe {
        // Linux nice and I/O priority apply to the calling thread. FFmpeg
        // decoder threads created afterwards inherit its CPU nice value.
        let _ = libc::nice(5);
        // IOPRIO_WHO_PROCESS = 1, IOPRIO_CLASS_BE = 2, priority level 7.
        let _ = libc::syscall(libc::SYS_ioprio_set, 1, 0, (2 << 13) | 7);
    }
    #[cfg(windows)]
    unsafe {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut std::ffi::c_void;
            fn SetThreadPriority(thread: *mut std::ffi::c_void, priority: i32) -> i32;
        }
        // THREAD_PRIORITY_BELOW_NORMAL; failure is non-fatal.
        let _ = SetThreadPriority(GetCurrentThread(), -1);
    }
}
