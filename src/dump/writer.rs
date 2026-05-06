use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

pub fn append_file_sendfile(dst: &File, src: &File) -> io::Result<u64> {
    let total = src.metadata()?.len();
    let mut copied = 0u64;

    #[cfg(target_os = "linux")]
    {
        while copied < total {
            let mut offset = copied as libc::off_t;
            let remaining = (total - copied) as usize;
            let chunk = remaining.min(1 << 30);
            let sent = unsafe { libc::sendfile(dst.as_raw_fd(), src.as_raw_fd(), &mut offset, chunk) };
            if sent < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if sent == 0 {
                break;
            }
            copied += sent as u64;
        }
        return Ok(copied);
    }

    #[allow(unreachable_code)]
    {
        let mut src_clone = src.try_clone()?;
        let mut dst_clone = dst.try_clone()?;
        Ok(std::io::copy(&mut src_clone, &mut dst_clone)?)
    }
}
