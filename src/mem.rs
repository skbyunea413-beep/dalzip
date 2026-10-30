//! Thread count policy, after Bandizip's LZMA2 setup: start from the CPU count
//! (max 64) and step down while the estimated working set exceeds 80% of the
//! available physical memory.

pub fn available_memory() -> Option<u64> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) != 0 {
            return Some(m.ullAvailPhys);
        }
        None
    }
    #[cfg(not(windows))]
    None
}

pub fn cpu_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, 64)
}

/// Returns (threads, capped_by_memory). An explicit request is honoured as is.
pub fn pick_threads(requested: Option<usize>, per_thread: u64) -> (usize, bool) {
    if let Some(n) = requested {
        return (n.max(1), false);
    }
    let cpus = cpu_threads();
    match available_memory() {
        Some(avail) => {
            let fit = ((avail / 10 * 8) / per_thread.max(1)).max(1) as usize;
            if fit < cpus {
                (fit, true)
            } else {
                (cpus, false)
            }
        }
        None => (cpus, false),
    }
}
