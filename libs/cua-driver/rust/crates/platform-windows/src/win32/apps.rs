//! Enumerate running processes on Windows.
//!
//! Uses CreateToolhelp32Snapshot / Process32FirstW / Process32NextW (simpler
//! and more portable than NtQuerySystemInformation or EnumProcesses+psapi).

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent_pid: u32,
    pub name: String,
}

/// Return all running processes (pid, parent_pid, executable name).
pub fn list_processes() -> Vec<ProcessInfo> {
    let mut result = Vec::new();
    unsafe {
        let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(h) => h,
            Err(_) => return result,
        };

        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let name = decode_wstr(&entry.szExeFile);
                result.push(ProcessInfo {
                    pid: entry.th32ProcessID,
                    parent_pid: entry.th32ParentProcessID,
                    name,
                });
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    result
}

fn decode_wstr(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Return all transitive descendants of `root_pid` (BFS through the process
/// tree). Includes processes that may have been spawned *after* `root_pid`
/// itself exited — useful for tracking launcher-stub chains where the
/// originally-launched binary re-execs into another process and exits (GIMP's
/// `gimp-3.exe` → `gimp-3.2.exe`; LibreOffice's `swriter.exe` → `soffice.bin`).
///
/// The result is in arrival order, which on Windows tends to correlate with
/// process-creation order — useful when picking the "main" descendant to
/// query for windows. Always includes `root_pid` itself first (even if it's
/// no longer alive, callers handle the empty-windows case the same way).
pub fn list_descendants(root_pid: u32) -> Vec<u32> {
    let all = list_processes();
    descendants_from_processes(root_pid, &all)
}

fn descendants_from_processes(root_pid: u32, all: &[ProcessInfo]) -> Vec<u32> {
    let mut result = vec![root_pid];
    let mut frontier = vec![root_pid];
    while let Some(parent) = frontier.pop() {
        for p in all {
            if p.parent_pid == parent && !result.contains(&p.pid) {
                result.push(p.pid);
                frontier.push(p.pid);
            }
        }
    }
    result
}

/// Return the launched PID and only its descendants absent before launch.
/// Executable names do not establish ownership. The root comes from the native
/// launch receipt and remains valid even when its window has not appeared yet.
pub fn launched_processes(
    root_pid: u32,
    pre_launch_pids: &std::collections::HashSet<u32>,
) -> Vec<u32> {
    launched_processes_from_snapshot(root_pid, pre_launch_pids, &list_processes())
}

fn launched_processes_from_snapshot(
    root_pid: u32,
    pre_launch_pids: &std::collections::HashSet<u32>,
    current: &[ProcessInfo],
) -> Vec<u32> {
    if root_pid == 0 {
        return Vec::new();
    }
    // Remove old branches before traversal so a reused parent PID cannot make
    // another application's newly spawned children part of this launch.
    let eligible: Vec<_> = current
        .iter()
        .filter(|process| process.pid == root_pid || !pre_launch_pids.contains(&process.pid))
        .cloned()
        .collect();
    descendants_from_processes(root_pid, &eligible)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, parent_pid: u32) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid,
            name: format!("process-{pid}.exe"),
        }
    }

    #[test]
    fn descendants_include_root_and_only_its_transitive_process_tree() {
        let processes = vec![
            process(42, 1),
            process(43, 42),
            process(44, 43),
            process(45, 42),
            process(99, 1),
            process(100, 99),
        ];

        let descendants = descendants_from_processes(42, &processes);

        assert_eq!(descendants, vec![42, 43, 45, 44]);
        assert!(!descendants.contains(&99));
        assert!(!descendants.contains(&100));
    }

    #[test]
    fn launch_does_not_adopt_existing_or_new_same_name_processes() {
        let mut processes = vec![process(42, 1), process(99, 1), process(100, 1)];
        for entry in &mut processes {
            entry.name = "maya.exe".into();
        }
        let before = [99].into_iter().collect();
        assert_eq!(
            launched_processes_from_snapshot(42, &before, &processes),
            vec![42]
        );
    }

    #[test]
    fn launch_keeps_renamed_descendants_after_launcher_exit() {
        let mut child = process(43, 42);
        child.name = "soffice.bin".into();
        let processes = vec![child, process(44, 43)];
        assert_eq!(
            launched_processes_from_snapshot(42, &Default::default(), &processes),
            vec![42, 43, 44]
        );
    }

    #[test]
    fn launch_prunes_existing_pid_branches_and_their_new_children() {
        let processes = vec![
            process(42, 1),
            process(43, 42),
            process(44, 43),
            process(45, 42),
        ];
        let before = [43].into_iter().collect();
        assert_eq!(
            launched_processes_from_snapshot(42, &before, &processes),
            vec![42, 45]
        );
    }

    #[test]
    fn launch_retains_receipt_pid_when_no_window_process_is_available() {
        assert_eq!(
            launched_processes_from_snapshot(42, &Default::default(), &[]),
            vec![42]
        );
    }

    #[test]
    fn launch_without_a_native_pid_receipt_has_no_owned_candidates() {
        let processes = vec![process(99, 0), process(100, 99)];
        assert!(launched_processes_from_snapshot(0, &Default::default(), &processes).is_empty());
    }
}
