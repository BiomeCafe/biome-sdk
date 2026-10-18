//! Who is on the other end of the IPC connection, as the operating system
//! says, and which program that process is running.
//!
//! The `game_pid` in a hello is what the game says about itself: any local
//! process can put any number there. The host uses [`pipe_client_pid`] /
//! [`unix_peer_pid`] instead when it needs to know which process it is
//! really talking to (to trust a game Biome installed without a prompt,
//! GAME_SDK_SPEC.md §4.2), pins that process at the accept
//! ([`PeerProcess`]), and asks the pinned process for its program on disk.

use std::path::PathBuf;

/// The process id of the client connected to a server pipe instance
/// (`GetNamedPipeClientProcessId`). `None` if Windows can't say.
#[cfg(windows)]
pub fn pipe_client_pid(pipe: &tokio::net::windows::named_pipe::NamedPipeServer) -> Option<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
    let mut pid: u32 = 0;
    // SAFETY: the handle is a live pipe instance owned by `pipe` for the
    // duration of the call, and `pid` is a valid out pointer.
    let ok = unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle() as _, &mut pid) };
    (ok != 0 && pid != 0).then_some(pid)
}

/// The process id of the peer of a unix socket (`SO_PEERCRED` /
/// `LOCAL_PEERPID`). `None` where the platform doesn't report it.
#[cfg(unix)]
pub fn unix_peer_pid(stream: &tokio::net::UnixStream) -> Option<u32> {
    let cred = stream.peer_cred().ok()?;
    cred.pid().and_then(|p| u32::try_from(p).ok()).filter(|p| *p != 0)
}

/// The client process of a connection, pinned when the connection is
/// accepted, so the question "which program is this?" is asked of that
/// process and no other.
///
/// A bare pid can be reused: if the client exits between the accept and
/// the trust check, another process could get its number. On Windows this
/// holds an open handle to the process from the accept on (Windows never
/// hands out a pid again while a handle to its process is open), and the
/// program path is read from that handle. On Linux the program path is
/// read at the accept (`/proc/<pid>/exe`) and kept. Elsewhere there is no
/// program, so nothing is trusted on its path.
///
/// What is left on Windows is the moment between asking the pipe for the
/// pid and opening it: the client would have to exit right after
/// connecting, with its pipe handle passed to another process, and a new
/// process get the same pid within those microseconds.
#[derive(Clone, Debug)]
pub struct PeerProcess {
    pid: u32,
    pinned: std::sync::Arc<Pinned>,
}

#[derive(Debug)]
struct Pinned {
    #[cfg(windows)]
    handle: Option<std::os::windows::io::OwnedHandle>,
    #[cfg(not(windows))]
    exe: Option<PathBuf>,
}

impl PeerProcess {
    /// Pin process `pid`. Call it right where the OS reported the pid.
    pub fn open(pid: u32) -> Self {
        #[cfg(windows)]
        let pinned = Pinned { handle: open_process(pid) };
        #[cfg(not(windows))]
        let pinned = Pinned { exe: proc_exe(pid) };
        Self { pid, pinned: std::sync::Arc::new(pinned) }
    }

    /// A pid with nothing pinned (tests): its program is unknown.
    pub fn unpinned(pid: u32) -> Self {
        #[cfg(windows)]
        let pinned = Pinned { handle: None };
        #[cfg(not(windows))]
        let pinned = Pinned { exe: None };
        Self { pid, pinned: std::sync::Arc::new(pinned) }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The program the pinned process runs, or `None` when it couldn't be
    /// pinned or read.
    pub fn exe(&self) -> Option<PathBuf> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            let handle = self.pinned.handle.as_ref()?;
            image_name(handle.as_raw_handle() as _)
        }
        #[cfg(not(windows))]
        {
            self.pinned.exe.clone()
        }
    }
}

#[cfg(windows)]
fn open_process(pid: u32) -> Option<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: a plain Win32 call; a non-null result is a handle we own,
    // closed when the OwnedHandle drops.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        (!handle.is_null()).then(|| std::os::windows::io::OwnedHandle::from_raw_handle(handle as _))
    }
}

/// `QueryFullProcessImageNameW` on an open process handle.
#[cfg(windows)]
fn image_name(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Threading::QueryFullProcessImageNameW;
    let mut buf = vec![0u16; 32 * 1024];
    let mut len = buf.len() as u32;
    // SAFETY: the handle is open for the duration of the call (the caller
    // holds it), and the buffer outlives the call that fills it.
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) };
    if ok == 0 || len == 0 {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(&buf[..len as usize])))
}

#[cfg(not(windows))]
fn proc_exe(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// The full path of the program process `pid` is running, or `None` if it
/// is gone or can't be read. Windows: `QueryFullProcessImageNameW`. Linux:
/// `/proc/<pid>/exe`. Elsewhere: `None` (nothing is trusted on its path).
/// To ask about a connection's client, use [`PeerProcess`], which pins the
/// process at the accept.
pub fn process_exe(pid: u32) -> Option<PathBuf> {
    PeerProcess::open(pid).exe()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_found() {
        if !cfg!(any(windows, target_os = "linux")) {
            return;
        }
        let ours = process_exe(std::process::id()).expect("our own program");
        let expected = std::env::current_exe().unwrap();
        assert_eq!(
            std::fs::canonicalize(ours).unwrap(),
            std::fs::canonicalize(expected).unwrap()
        );
    }

    /// A pinned process answers about that process only: once it has
    /// exited it answers nothing or still its own program (Windows refuses
    /// the query for an exited process), never another process's; an
    /// unpinned pid answers nothing.
    #[test]
    fn a_pinned_process_keeps_its_program() {
        if !cfg!(any(windows, target_os = "linux")) {
            return;
        }
        let ours = PeerProcess::open(std::process::id());
        assert_eq!(ours.pid(), std::process::id());
        assert!(ours.exe().is_some());
        assert!(PeerProcess::unpinned(std::process::id()).exe().is_none());

        #[cfg(windows)]
        {
            // About a second of life, so it is still running when pinned.
            let mut child = std::process::Command::new("cmd")
                .args(["/c", "ping", "-n", "2", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let pinned = PeerProcess::open(child.id());
            let running = pinned.exe().expect("the child's program, through its handle");
            assert!(running.to_string_lossy().to_ascii_lowercase().ends_with("cmd.exe"), "{}", running.display());
            child.wait().unwrap();
            drop(child);
            if let Some(exe) = pinned.exe() {
                assert_eq!(exe, running);
            }
        }
    }

    #[test]
    fn a_process_that_does_not_exist_has_no_program() {
        // Pids are multiples of 4 on Windows; an odd one never exists.
        #[cfg(windows)]
        assert!(process_exe(u32::MAX - 2).is_none());
        #[cfg(target_os = "linux")]
        assert!(process_exe(u32::MAX - 2).is_none());
    }
}
