//! Direct `libc::flock()` wrappers for platforms where `std::fs::File::try_lock`
//! is broken by an stdlib `#[cfg]` omission (Rust ≤1.95 on Android).
//!
//! Remove this module once the toolchain ships with the fix (Rust ≥1.96).

use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;

pub fn try_flock_exclusive(file: &File) -> io::Result<()> {
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        Ok(())
    } else {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EAGAIN) || err.raw_os_error() == Some(libc::EWOULDBLOCK)
        {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "lock held"))
        } else {
            Err(err)
        }
    }
}

#[allow(dead_code)]
pub fn try_flock_shared(file: &File) -> io::Result<()> {
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    if ret == 0 {
        Ok(())
    } else {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EAGAIN) || err.raw_os_error() == Some(libc::EWOULDBLOCK)
        {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "lock held"))
        } else {
            Err(err)
        }
    }
}

#[allow(dead_code)]
pub fn lock_exclusive(file: &File) -> io::Result<()> {
    cvt(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) })
}

#[allow(dead_code)]
pub fn lock_shared(file: &File) -> io::Result<()> {
    cvt(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) })
}

#[allow(dead_code)]
pub fn unlock(file: &File) -> io::Result<()> {
    cvt(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) })
}

#[allow(dead_code)]
fn cvt(ret: i32) -> io::Result<()> {
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
