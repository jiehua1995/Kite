/// 中文：把 `Duration` 格式化成更适合命令行展示的时间字符串。
/// 输入是 Rust 标准库的持续时间，输出是 `1.234s` / `2m03.456s` 之类文本。
/// English: Format a `Duration` into a compact CLI-friendly string such as
/// `1.234s` or `2m03.456s`.
pub fn format_duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    let ms = d.subsec_millis();
    if s >= 3600 {
        format!("{}h{:02}m{:02}s", s / 3600, (s % 3600) / 60, s % 60)
    } else if s >= 60 {
        let m = s / 60;
        let sec = s % 60;
        format!("{}m{:02}.{:03}s", m, sec, ms)
    } else {
        format!("{}.{:03}s", s, ms)
    }
}

/// 中文：把字节数格式化成人类可读的大小字符串。
/// 输入是字节数，输出是 `KB/MB/GB` 风格文本。
/// English: Format raw bytes into a human-readable size string using
/// `KB/MB/GB` units.
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.2} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}
