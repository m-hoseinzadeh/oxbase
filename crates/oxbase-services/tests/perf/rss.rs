//! Resident memory of this process (Linux `/proc/self/status` `VmRSS`), sampled on a thread.
//! Elsewhere every reading is `None` and the memory assertion is skipped.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// `VmRSS` in bytes, or `None` where `/proc/self/status` does not exist.
pub fn vm_rss() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kib: u64 = line
        .trim_start_matches("VmRSS:")
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some(kib * 1024)
}

/// Samples [`vm_rss`] every `interval` until [`Sampler::finish`], keeping the peak.
pub struct Sampler {
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl Sampler {
    pub fn start(interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(vm_rss().unwrap_or(0)));
        let thread = {
            let stop = Arc::clone(&stop);
            let peak = Arc::clone(&peak);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Some(rss) = vm_rss() {
                        peak.fetch_max(rss, Ordering::Relaxed);
                    }
                    std::thread::sleep(interval);
                }
            })
        };
        Self {
            stop,
            peak,
            thread: Some(thread),
        }
    }

    /// Stops sampling; the peak in bytes (`None` without `/proc`).
    pub fn finish(mut self) -> Option<u64> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(rss) = vm_rss() {
            self.peak.fetch_max(rss, Ordering::Relaxed);
        }
        vm_rss().map(|_| self.peak.load(Ordering::Relaxed))
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
