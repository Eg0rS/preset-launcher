//! Win32 process helpers: enumerate, gracefully close, kill, launch.

use std::collections::{HashMap, HashSet};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetWindow, GetWindowThreadProcessId, IsWindowVisible, PostMessageW,
    SW_SHOWNORMAL, WM_CLOSE,
};
use windows::core::{BOOL, HSTRING, PCWSTR};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ERROR_ELEVATION_REQUIRED: i32 = 740;

/// Processes that must never be closed, whatever the config says.
const PROTECTED: &[&str] = &[
    "explorer.exe", "svchost.exe", "csrss.exe", "winlogon.exe", "wininit.exe", "lsass.exe",
    "services.exe", "smss.exe", "dwm.exe", "system", "registry", "fontdrvhost.exe",
    "sihost.exe", "ctfmon.exe", "taskmgr.exe",
];

pub fn is_protected(process_name: &str) -> bool {
    PROTECTED.contains(&process_name)
}

/// Returns `lowercase image name -> pids` for all running processes (except ourselves).
pub fn snapshot() -> HashMap<String, Vec<u32>> {
    let mut map: HashMap<String, Vec<u32>> = HashMap::new();
    let me = std::process::id();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return map;
        };
        let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut entry).is_ok();
        while ok {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
            if entry.th32ProcessID != me {
                map.entry(name).or_default().push(entry.th32ProcessID);
            }
            ok = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    map
}

struct EnumCtx {
    pids: HashSet<u32>,
    found: Vec<HWND>,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = unsafe { &mut *(lparam.0 as *mut EnumCtx) };
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if ctx.pids.contains(&pid) {
        let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
        let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.map(|h| !h.is_invalid()).unwrap_or(false);
        if visible && !owned {
            ctx.found.push(hwnd);
        }
    }
    BOOL(1)
}

/// Visible, unowned top-level windows belonging to the given processes.
fn visible_windows(pids: &[u32]) -> Vec<HWND> {
    let mut ctx = EnumCtx { pids: pids.iter().copied().collect(), found: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut ctx as *mut EnumCtx as isize));
    }
    ctx.found
}

pub fn visible_window_count(pids: &[u32]) -> usize {
    visible_windows(pids).len()
}

/// Sends WM_CLOSE to all visible top-level windows of the given processes.
/// Returns how many windows were asked to close.
pub fn post_close_to_windows(pids: &[u32]) -> usize {
    let windows = visible_windows(pids);
    for &hwnd in &windows {
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    windows.len()
}

pub fn kill(pid: u32) -> bool {
    unsafe {
        match OpenProcess(PROCESS_TERMINATE, false, pid) {
            Ok(h) => {
                let ok = TerminateProcess(h, 1).is_ok();
                let _ = CloseHandle(h);
                ok
            }
            Err(_) => false,
        }
    }
}

/// Runs a user-specified close command line via `cmd /C`, without a console window.
pub fn run_close_command(cmdline: &str) -> std::io::Result<()> {
    Command::new("cmd.exe")
        .raw_arg("/C")
        .raw_arg(cmdline)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Starts a program. `.exe` files are started directly (fast, exact arguments);
/// everything else (.lnk, .bat, URLs, documents) and programs that require
/// elevation go through ShellExecute.
pub fn launch(path: &str, args: &str, working_dir: &str) -> Result<(), String> {
    let lower = path.to_lowercase();
    let is_exe = lower.ends_with(".exe") && !lower.contains("://");
    if is_exe {
        let mut cmd = Command::new(path);
        if !args.trim().is_empty() {
            cmd.raw_arg(args);
        }
        if !working_dir.trim().is_empty() {
            cmd.current_dir(working_dir);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        match cmd.spawn() {
            Ok(_) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(ERROR_ELEVATION_REQUIRED) => { /* fall through */ }
            Err(e) => return Err(e.to_string()),
        }
    }
    shell_execute(path, args, working_dir)
}

fn shell_execute(path: &str, args: &str, working_dir: &str) -> Result<(), String> {
    let file = HSTRING::from(path);
    let params = HSTRING::from(args);
    let dir = HSTRING::from(working_dir);
    let res = unsafe {
        ShellExecuteW(
            None,
            &HSTRING::from("open"),
            &file,
            if args.trim().is_empty() { PCWSTR::null() } else { PCWSTR(params.as_ptr()) },
            if working_dir.trim().is_empty() { PCWSTR::null() } else { PCWSTR(dir.as_ptr()) },
            SW_SHOWNORMAL,
        )
    };
    // Per docs, values > 32 mean success.
    if res.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecute вернул код {}", res.0 as isize))
    }
}
