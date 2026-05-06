const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const GIB: usize = 1024 * MIB;

pub fn resolve_memory_limit_bytes(requested_gb: f64) -> anyhow::Result<usize> {
    if requested_gb.is_finite() && requested_gb > 0.0 {
        return Ok((requested_gb * GIB as f64) as usize);
    }

    let detected_bytes = detect_physical_memory_bytes()?;
    let headroom_bytes = auto_memory_headroom(detected_bytes);
    Ok(detected_bytes.saturating_sub(headroom_bytes))
}

pub fn apply_process_memory_limit(limit_bytes: usize) -> anyhow::Result<usize> {
    #[cfg(unix)]
    {
        let limit = libc::rlimit {
            rlim_cur: limit_bytes as libc::rlim_t,
            rlim_max: limit_bytes as libc::rlim_t,
        };
        let result = unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) };
        if result != 0 {
            return Err(anyhow::anyhow!(
                "setrlimit(RLIMIT_AS) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
    }

    Ok(limit_bytes)
}

fn auto_memory_headroom(total_bytes: usize) -> usize {
    total_bytes
        .checked_div(100)
        .unwrap_or(0)
        .clamp(512 * MIB, 2 * GIB)
}

fn detect_physical_memory_bytes() -> anyhow::Result<usize> {
    #[cfg(unix)]
    {
        let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if pages <= 0 || page_size <= 0 {
            anyhow::bail!("sysconf(_SC_PHYS_PAGES/_SC_PAGESIZE) failed");
        }
        let total = (pages as u128).saturating_mul(page_size as u128);
        return usize::try_from(total)
            .map_err(|_| anyhow::anyhow!("physical memory exceeds usize range"));
    }

    #[cfg(not(unix))]
    anyhow::bail!("automatic memory detection is unsupported on this platform")
}