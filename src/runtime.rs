const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const GIB: usize = 1024 * MIB;
const TIB: usize = 1024 * GIB;

pub fn parse_memory_spec(spec: &str) -> anyhow::Result<usize> {
    if spec.len() < 2 {
        anyhow::bail!(
            "invalid memory size '{}': expected formats like 512M, 64G, or 1T",
            spec
        );
    }

    let (number, suffix) = spec.split_at(spec.len() - 1);
    let multiplier = match suffix {
        "M" => MIB,
        "G" => GIB,
        "T" => TIB,
        _ => {
            anyhow::bail!("invalid memory size '{}': suffix must be M, G, or T", spec)
        }
    };
    let value: usize = number.parse().map_err(|_| {
        anyhow::anyhow!(
            "invalid memory size '{}': numeric part must be a positive integer",
            spec
        )
    })?;
    if value == 0 {
        anyhow::bail!(
            "invalid memory size '{}': value must be greater than zero",
            spec
        );
    }
    value
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow::anyhow!("memory size '{}' exceeds supported range", spec))
}

pub fn resolve_memory_limit_bytes(requested: Option<&str>) -> anyhow::Result<usize> {
    if let Some(spec) = requested {
        return parse_memory_spec(spec);
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

#[cfg(test)]
mod tests {
    use super::parse_memory_spec;

    #[test]
    fn parse_memory_spec_accepts_m_g_t_suffixes() {
        assert_eq!(parse_memory_spec("512M").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_spec("64G").unwrap(), 64 * 1024 * 1024 * 1024);
        assert_eq!(
            parse_memory_spec("1T").unwrap(),
            1024usize * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn parse_memory_spec_rejects_missing_or_bad_suffixes() {
        assert!(parse_memory_spec("64").is_err());
        assert!(parse_memory_spec("64g").is_err());
        assert!(parse_memory_spec("2P").is_err());
        assert!(parse_memory_spec("0G").is_err());
    }
}
