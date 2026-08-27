//! Console-free construction for bounded Windows diagnostic helpers.
//!
//! These children keep their normal stdio and lifetime semantics; only the
//! creation flag that would allocate a user-visible console is suppressed.

use std::ffi::OsStr;
use std::os::windows::process::CommandExt;

pub(crate) const CREATE_NO_WINDOW_FLAG: u32 = 0x0800_0000;

pub(crate) fn std_hidden(program: impl AsRef<OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW_FLAG);
    command
}

pub(crate) fn tokio_hidden(program: impl AsRef<OsStr>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW_FLAG);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let state = &mut *(state.0 as *mut WindowCount);
        let mut owner_pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut owner_pid));
        if owner_pid == state.pid && IsWindowVisible(hwnd).as_bool() {
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
    fn hidden_helper_has_no_visible_console_window() {
        let mut child = std_hidden("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Milliseconds 750",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bounded hidden helper");
        let pid = child.id();
        thread::sleep(Duration::from_millis(200));
        assert_eq!(visible_window_count(pid), 0);
        assert!(child.wait().expect("wait for hidden helper").success());
    }
}
