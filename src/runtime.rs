use std::ffi::CStr;

#[derive(Debug, Clone)]
pub struct RuntimeInfo {
    pub os_name: String,
    pub os_version: String,
    pub kernel_version: String,
    pub arch: String,
}

pub fn detect_runtime_info() -> RuntimeInfo {
    #[cfg(unix)]
    {
        let (sysname, release, machine) = uname_triplet();
        let os_version = detect_os_version_unix(&sysname);
        RuntimeInfo {
            os_name: sysname,
            os_version,
            kernel_version: release,
            arch: machine,
        }
    }

    #[cfg(not(unix))]
    {
        RuntimeInfo {
            os_name: std::env::consts::OS.to_string(),
            os_version: "unknown".to_string(),
            kernel_version: "unknown".to_string(),
            arch: std::env::consts::ARCH.to_string(),
        }
    }
}

pub fn print_runtime_banner(command: &str, memory_limit_bytes: Option<usize>, address_space_limit_bytes: Option<usize>) {
    let info = detect_runtime_info();
    let memory_note = memory_limit_bytes
        .map(|bytes| format!(", memory_limit={:.1}GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0))
        .unwrap_or_default();
    let as_note = address_space_limit_bytes
        .map(|bytes| format!(", as_limit={:.1}GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0))
        .unwrap_or_default();
    eprintln!(
        "[kite runtime] command={}, os={} {}, kernel={}, arch={}, newline_support=LF/CRLF/CR{}{}",
        command,
        info.os_name,
        info.os_version,
        info.kernel_version,
        info.arch,
        memory_note,
        as_note,
    );
}

pub fn apply_process_memory_limit(limit_bytes: usize) -> anyhow::Result<usize> {
    let headroom_bytes = address_space_headroom(limit_bytes);
    let applied_limit_bytes = limit_bytes.saturating_add(headroom_bytes);
    #[cfg(unix)]
    {
        let limit = libc::rlimit {
            rlim_cur: applied_limit_bytes as libc::rlim_t,
            rlim_max: applied_limit_bytes as libc::rlim_t,
        };
        let result = unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) };
        if result != 0 {
            return Err(anyhow::anyhow!(
                "setrlimit(RLIMIT_AS) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
    }

    Ok(applied_limit_bytes)
}

fn address_space_headroom(limit_bytes: usize) -> usize {
    let gib = 1024 * 1024 * 1024;
    limit_bytes.clamp(16 * gib, 64 * gib)
}

#[cfg(unix)]
fn uname_triplet() -> (String, String, String) {
    let mut uts = std::mem::MaybeUninit::<libc::utsname>::uninit();
    let rc = unsafe { libc::uname(uts.as_mut_ptr()) };
    if rc != 0 {
        return (
            std::env::consts::OS.to_string(),
            "unknown".to_string(),
            std::env::consts::ARCH.to_string(),
        );
    }
    let uts = unsafe { uts.assume_init() };
    (
        c_array_to_string(&uts.sysname),
        c_array_to_string(&uts.release),
        c_array_to_string(&uts.machine),
    )
}

#[cfg(unix)]
fn c_array_to_string(raw: &[libc::c_char]) -> String {
    unsafe { CStr::from_ptr(raw.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(unix)]
fn detect_os_version_unix(sysname: &str) -> String {
    if sysname.eq_ignore_ascii_case("linux") {
        if let Ok(contents) = std::fs::read_to_string("/etc/os-release") {
            for line in contents.lines() {
                if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
                    return value.trim_matches('"').to_string();
                }
            }
        }
    }
    "unknown".to_string()
}