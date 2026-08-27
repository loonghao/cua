use std::ffi::OsStr;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnedConsoleChildRole {
    BrowserProcessTreeCleanup,
    PathProbe,
    Installer,
    RecordingFfmpeg,
    RecordingFfprobe,
}

#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW_FLAG: u32 = 0x0800_0000;

pub(crate) fn command(_role: OwnedConsoleChildRole, program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW_FLAG);
    }
    command
}

#[cfg(all(test, windows))]
mod tests {
    use super::{command, OwnedConsoleChildRole, CREATE_NO_WINDOW_FLAG};
    use std::io::Read;
    use std::process::Stdio;
    use std::thread;
    use std::time::Duration;
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, TRUE};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
    };

    #[derive(Default)]
    struct WindowCount {
        pid: u32,
        visible: usize,
    }

    unsafe extern "system" fn count_visible_child_windows(hwnd: HWND, state: LPARAM) -> BOOL {
        let state = unsafe { &mut *(state.0 as *mut WindowCount) };
        let mut owner_pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner_pid)) };
        if owner_pid == state.pid && unsafe { IsWindowVisible(hwnd) }.as_bool() {
            state.visible += 1;
        }
        TRUE
    }

    fn visible_window_count(pid: u32) -> usize {
        let mut state = WindowCount { pid, visible: 0 };
        unsafe {
            let _ = EnumWindows(
                Some(count_visible_child_windows),
                LPARAM(&mut state as *mut WindowCount as isize),
            );
        }
        state.visible
    }

    #[test]
    fn every_owned_console_child_role_is_hidden_and_preserves_pipes_and_lifecycle() {
        assert_eq!(CREATE_NO_WINDOW_FLAG, 0x0800_0000);
        for role in [
            OwnedConsoleChildRole::BrowserProcessTreeCleanup,
            OwnedConsoleChildRole::PathProbe,
            OwnedConsoleChildRole::Installer,
            OwnedConsoleChildRole::RecordingFfmpeg,
            OwnedConsoleChildRole::RecordingFfprobe,
        ] {
            let mut child = command(role, "powershell.exe")
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "$ErrorActionPreference='Stop'; [Console]::Out.Write('pipe-ok'); Start-Sleep -Milliseconds 500",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn bounded role helper");
            let pid = child.id();
            thread::sleep(Duration::from_millis(150));
            assert_eq!(visible_window_count(pid), 0, "role={role:?} pid={pid}");
            let mut stdout = String::new();
            child
                .stdout
                .take()
                .expect("stdout pipe")
                .read_to_string(&mut stdout)
                .expect("read stdout pipe");
            let status = child.wait().expect("wait for exact role child");
            assert!(status.success(), "role={role:?} pid={pid} status={status}");
            assert_eq!(stdout, "pipe-ok", "role={role:?} pid={pid}");
        }
    }
}
