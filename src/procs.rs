//! Process ancestry, used to tie an agent's MCP server and its hooks together.
//!
//! Claude Code and Codex launch `chitchat mcp` directly and run hook commands
//! (possibly through a shell), so both are descendants of the client process.
//! Walking up from our parent and skipping shells finds that client. Its pid plus
//! start time identifies one agent across /clear, compaction and pid reuse.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    /// Opaque start timestamp; only compared for equality.
    pub started_at: i64,
}

/// Overrides client detection with a specific pid (tests, unusual launchers).
pub const CLIENT_PID_ENV: &str = "CHITCHAT_CLIENT_PID";

const SHELLS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "mksh",
    "tcsh",
    "csh",
    "nu",
    "elvish",
    "xonsh",
    "pwsh",
    "powershell",
    "cmd",
];

const MAX_HOPS: usize = 8;

/// The Claude Code / Codex process this chitchat process is running under.
///
/// `pid_env` names an environment variable the client sets to its own pid (Claude
/// Code gives hooks `CLAUDE_PID`); `CHITCHAT_CLIENT_PID` overrides both that and
/// the ancestry walk.
pub fn client_process(pid_env: Option<&str>) -> Option<ProcInfo> {
    for var in [Some(CLIENT_PID_ENV), pid_env].into_iter().flatten() {
        if let Some(pid) = std::env::var(var).ok().and_then(|v| v.parse().ok()) {
            return info(pid);
        }
    }
    let mut pid = parent_pid()?;
    for _ in 0..MAX_HOPS {
        if pid <= 1 {
            return None;
        }
        let proc = info(pid)?;
        if !is_shell(&proc.name) {
            return Some(proc);
        }
        pid = proc.ppid;
    }
    None
}

/// Whether the process that had `pid` at `started_at` is still running.
pub fn is_alive(pid: u32, started_at: i64) -> bool {
    info(pid).is_some_and(|p| p.started_at == started_at)
}

fn is_shell(name: &str) -> bool {
    let name = name.trim_start_matches('-'); // login shells show up as "-zsh"
    let name = name.strip_suffix(".exe").unwrap_or(name);
    SHELLS.contains(&name)
}

#[cfg(unix)]
fn parent_pid() -> Option<u32> {
    Some(std::os::unix::process::parent_id())
}

#[cfg(not(unix))]
fn parent_pid() -> Option<u32> {
    None
}

#[cfg(target_os = "macos")]
pub fn info(pid: u32) -> Option<ProcInfo> {
    use std::ffi::CStr;
    use std::mem::{MaybeUninit, size_of};

    let mut raw = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is a properly sized, zero-initialized proc_bsdinfo.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            raw.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: proc_pidinfo filled the whole struct.
    let raw = unsafe { raw.assume_init() };
    let text = |chars: &[libc::c_char]| {
        // SAFETY: the kernel NUL-terminates these fixed-size name buffers.
        unsafe { CStr::from_ptr(chars.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    let name = Some(text(&raw.pbi_name))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| text(&raw.pbi_comm));
    Some(ProcInfo {
        pid,
        ppid: raw.pbi_ppid,
        name,
        started_at: (raw.pbi_start_tvsec as i64) * 1_000_000 + raw.pbi_start_tvusec as i64,
    })
}

#[cfg(target_os = "linux")]
pub fn info(pid: u32) -> Option<ProcInfo> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Format: "pid (comm) state ppid ... starttime ...". comm may contain spaces
    // or parentheses, so split at the last ')'.
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat[open + 1..close].to_string();
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    // fields[0] is field 3 (state), so field N is fields[N - 3].
    let ppid = fields.get(1)?.parse().ok()?;
    let started_at = fields.get(19)?.parse().ok()?;
    Some(ProcInfo {
        pid,
        ppid,
        name,
        started_at,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn info(_pid: u32) -> Option<ProcInfo> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_are_recognized() {
        for name in ["zsh", "-zsh", "bash", "fish", "sh", "pwsh.exe"] {
            assert!(is_shell(name), "{name}");
        }
        for name in ["claude", "codex", "node", "cargo"] {
            assert!(!is_shell(name), "{name}");
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn reads_own_process_and_detects_exit() {
        let me = info(std::process::id()).unwrap();
        assert_eq!(me.pid, std::process::id());
        assert!(is_alive(me.pid, me.started_at));
        assert!(!is_alive(me.pid, me.started_at + 1));

        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        // Right after spawn the child may not have exec'd yet, in which case it
        // still carries the forked test thread's name. Wait for the exec.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let proc = loop {
            let proc = info(child.id()).unwrap();
            if proc.name == "sleep" || std::time::Instant::now() > deadline {
                break proc;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(proc.ppid, std::process::id());
        assert_eq!(proc.name, "sleep");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!is_alive(proc.pid, proc.started_at));
    }
}
