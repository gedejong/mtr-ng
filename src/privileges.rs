//! Giving up privileges once the raw sockets are open, as mtr's mtr-packet does.
//!
//! mtr-ng needs privileges for one thing only: opening raw ICMP sockets (UDP
//! probes and MPLS labels need them on Linux; ICMP can do without, through ping
//! sockets). `main` opens them first, then calls `give_up_setuid`, opens the log
//! file, and calls `drop_capabilities`, all before the async runtime starts any
//! thread: Linux capabilities belong to each thread, and a thread started earlier
//! would keep them.

use crate::Result;

/// Become the user who ran mtr-ng, if it was installed setuid root. Files opened
/// afterwards, such as the log, are opened as that user.
pub fn give_up_setuid() -> Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: plain system calls without pointers.
        unsafe {
            let (uid, gid) = (libc::getuid(), libc::getgid());
            if libc::getegid() != gid && libc::setgid(gid) != 0 {
                anyhow::bail!(
                    "cannot give up group privileges: {}",
                    std::io::Error::last_os_error()
                );
            }
            if libc::geteuid() != uid && libc::setuid(uid) != 0 {
                anyhow::bail!("cannot give up root: {}", std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

/// Drop the Linux capabilities of this thread, which must be the only one:
/// CAP_NET_RAW and every other. Root keeps its user id and, if it has it,
/// CAP_DAC_READ_SEARCH, so that `sudo mtr-ng --geoip-mmdb ~/City.mmdb` can still
/// read a database in a private home directory; mtr-ng writes nothing after this
/// but stdout and the log, which is already open.
pub fn drop_capabilities() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        #[repr(C)]
        struct Header {
            version: u32,
            pid: libc::c_int,
        }
        #[repr(C)]
        #[derive(Default)]
        struct Sets {
            effective: u32,
            permitted: u32,
            inheritable: u32,
        }
        // _LINUX_CAPABILITY_VERSION_3: two sets of 32 bits each
        let mut header = Header {
            version: 0x2008_0522,
            pid: 0,
        };
        let mut sets: [Sets; 2] = Default::default();
        // SAFETY: capget reads a version 3 header and writes two sets, both alive here.
        if unsafe { libc::syscall(libc::SYS_capget, &mut header, sets.as_mut_ptr()) } != 0 {
            anyhow::bail!(
                "cannot read capabilities: {}",
                std::io::Error::last_os_error()
            );
        }
        // CAP_DAC_READ_SEARCH is capability 2, in the first set. capset can only
        // keep what is permitted already, and Docker, for one, doesn't permit it.
        // SAFETY: a plain system call without pointers.
        let keep = match unsafe { libc::geteuid() } {
            0 => sets[0].permitted & 1 << 2,
            _ => 0,
        };
        sets = Default::default();
        sets[0].effective = keep;
        sets[0].permitted = keep;
        // SAFETY: capset reads a version 3 header and two sets, both alive here.
        if unsafe { libc::syscall(libc::SYS_capset, &header, sets.as_ptr()) } != 0 {
            anyhow::bail!(
                "cannot drop capabilities: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn giving_up_succeeds_whatever_this_process_holds() {
        // Without privileges both are no-ops; as root (the lab runs tests so) they
        // drop the capabilities of this test's own thread only.
        give_up_setuid().unwrap();
        drop_capabilities().unwrap();
    }
}
