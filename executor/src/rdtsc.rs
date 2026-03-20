//! rdtsc.rs — Fast CPU timestamp counter for hot-path timing.
//!
//! RDTSC reads the CPU's time-stamp counter (~1ns resolution, ~5ns cost).
//! vs std::time::Instant::now() which does a vDSO syscall (~25ns cost).

/// Read the CPU timestamp counter (monotonic, ~1ns resolution).
#[inline(always)]
pub fn rdtsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        std::time::Instant::now().elapsed().as_nanos() as u64
    }
}

/// Approximate TSC ticks per microsecond (calibrated once at startup).
static TSC_FREQ_MHZ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Calibrate TSC frequency. Call once at startup.
pub fn calibrate() {
    let t0 = rdtsc();
    std::thread::sleep(std::time::Duration::from_millis(10));
    let t1 = rdtsc();
    let ticks_per_ms = (t1 - t0) / 10;
    let freq_mhz = ticks_per_ms / 1000;
    TSC_FREQ_MHZ.store(freq_mhz.max(1), std::sync::atomic::Ordering::Relaxed);
}

/// Convert TSC tick difference to microseconds.
#[inline(always)]
pub fn ticks_to_us(ticks: u64) -> u64 {
    let freq = TSC_FREQ_MHZ.load(std::sync::atomic::Ordering::Relaxed);
    if freq == 0 {
        return ticks / 3000; // fallback: assume ~3GHz
    }
    ticks / freq
}
