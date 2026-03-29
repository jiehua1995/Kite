use crate::utils::fmt::format_duration;
use std::io::{Write, stderr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// 中文：一个轻量级终端转圈器。输入是前缀和提示文本，
/// 输出是后台线程持续刷新 stderr，直到对象被 drop。
/// English: A lightweight terminal spinner. It takes a prefix and message,
/// then refreshes stderr from a background thread until the object is dropped.
pub struct ProgressSpinner {
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ProgressSpinner {
    /// 中文：按需启动进度转圈器。`enabled=false` 时返回 `None`，
    /// 这样调用方就可以零成本关闭进度显示。
    /// English: Start the spinner when requested. When `enabled=false`, it
    /// returns `None` so callers can disable progress with near-zero overhead.
    pub fn start(
        enabled: bool,
        prefix: impl Into<String>,
        message: impl Into<String>,
    ) -> Option<Self> {
        if !enabled {
            return None;
        }

        let prefix = prefix.into();
        let message = message.into();
        let done = Arc::new(AtomicBool::new(false));
        let done_flag = done.clone();
        let handle = thread::spawn(move || {
            let frames = ['|', '/', '-', '\\'];
            let start = Instant::now();
            let mut frame_idx = 0usize;
            while !done_flag.load(Ordering::Relaxed) {
                let elapsed = format_duration(start.elapsed());
                eprint!(
                    "\r[{}] {} {} ({})",
                    prefix,
                    frames[frame_idx % frames.len()],
                    message,
                    elapsed
                );
                let _ = stderr().flush();
                frame_idx += 1;
                thread::sleep(Duration::from_millis(120));
            }

            let clear = " ".repeat(prefix.len() + message.len() + 24);
            eprint!("\r{}\r", clear);
            let _ = stderr().flush();
        });

        Some(Self {
            done,
            handle: Some(handle),
        })
    }
}

impl Drop for ProgressSpinner {
    /// 中文：对象销毁时通知后台线程停止并等待它收尾。
    /// English: Stop the background spinner thread and join it during drop.
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
