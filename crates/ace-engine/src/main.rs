//! outpace daemon entry point.

// jemalloc gives 64-bit non-MSVC targets configurable page decay and live/resident counters for
// `/debug/memstats`. 32-bit targets keep the system allocator: jemalloc's `ffs` helper needs a
// `__ffsdi2` builtin that fails to link against static 32-bit musl. Windows/MSVC also keeps the
// system allocator.
#[cfg(all(not(target_env = "msvc"), target_pointer_width = "64"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(all(not(target_env = "msvc"), target_pointer_width = "64"))]
mod allocator_config {
    // The pinned jemalloc excludes Mach-O from its background-thread implementation, even though
    // Apple has pthreads. Decay there is driven by allocator activity, not a periodic idle worker.
    #[cfg(all(unix, not(target_vendor = "apple")))]
    const CONFIG: &std::ffi::CStr = c"background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0";
    #[cfg(not(all(unix, not(target_vendor = "apple"))))]
    const CONFIG: &std::ffi::CStr = c"background_thread:false,dirty_decay_ms:1000,muzzy_decay_ms:0";

    // jemalloc declares `extern const char *malloc_conf`, so this must be a thin C pointer, not a
    // Rust slice. The wrapper adds Sync without changing that ABI.
    #[repr(transparent)]
    struct MallocConf(*const libc::c_char);

    // SAFETY: the pointer refers to immutable, NUL-terminated static bytes for the process lifetime.
    unsafe impl Sync for MallocConf {}

    // Match tikv-jemalloc-sys's NO_UNPREFIXED_MALLOC_TARGETS. Those platforms must retain prefixed
    // allocation symbols to avoid mixing libc and jemalloc allocations (e.g. through realpath).
    #[used]
    #[cfg_attr(
        any(
            target_vendor = "apple",
            target_os = "android",
            target_os = "dragonfly"
        ),
        export_name = "_rjem_malloc_conf"
    )]
    #[cfg_attr(
        not(any(
            target_vendor = "apple",
            target_os = "android",
            target_os = "dragonfly"
        )),
        export_name = "malloc_conf"
    )]
    static MALLOC_CONF: MallocConf = MallocConf(CONFIG.as_ptr());

    #[cfg(test)]
    mod tests {
        #[test]
        fn effective_background_threads_match_platform_support() {
            let expected = cfg!(all(unix, not(target_vendor = "apple")));
            assert_eq!(
                tikv_jemalloc_ctl::opt::background_thread::read().unwrap(),
                expected,
                "allocator defaults changed; run without MALLOC_CONF or _RJEM_MALLOC_CONF overrides"
            );
            #[cfg(all(unix, not(target_vendor = "apple")))]
            assert!(tikv_jemalloc_ctl::background_thread::read().unwrap());
        }

        #[test]
        fn effective_decay_settings_use_the_exported_configuration() {
            // SAFETY: these read-only jemalloc mallctl options both have the documented ssize_t
            // type, and the keys are NUL-terminated. Read the allocator, not just the Rust string,
            // to catch a wrong symbol prefix or C ABI that would silently leave default settings.
            let (dirty, muzzy) = unsafe {
                (
                    tikv_jemalloc_ctl::raw::read::<libc::ssize_t>(b"opt.dirty_decay_ms\0").unwrap(),
                    tikv_jemalloc_ctl::raw::read::<libc::ssize_t>(b"opt.muzzy_decay_ms\0").unwrap(),
                )
            };
            assert_eq!(
                (dirty, muzzy),
                (1000, 0),
                "allocator defaults changed; run without MALLOC_CONF or _RJEM_MALLOC_CONF overrides"
            );
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ace_engine::cli::run().await
}
