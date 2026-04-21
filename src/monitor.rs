use crate::config::{active_patterns, SharedConfig};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use windows::core::{Interface, VARIANT};
use windows::Win32::Foundation::{BOOL, CloseHandle, HWND, LPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::ProcessStatus::GetModuleBaseNameW;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCondition, IUIAutomationElement,
    IUIAutomationValuePattern, TreeScope_Descendants, UIA_ControlTypePropertyId,
    UIA_EditControlTypeId, UIA_ValuePatternId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_CONTROL, VK_W,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowThreadProcessId, IsWindowVisible,
};

const BROWSER_PROCESSES: &[&str] = &[
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "brave.exe",
    "opera.exe",
    "vivaldi.exe",
    "arc.exe",
    "zen.exe",
    "librewolf.exe",
];

struct WindowList(Vec<HWND>);

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if IsWindowVisible(hwnd).as_bool() {
        let list = &mut *(lparam.0 as *mut WindowList);
        list.0.push(hwnd);
    }
    BOOL(1)
}

fn hwnd_eq(a: HWND, b: HWND) -> bool {
    a.0 as usize == b.0 as usize
}

fn process_name_for_window(hwnd: HWND) -> Option<String> {
    unsafe {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            false,
            pid,
        )
        .ok()?;
        let mut buf = [0u16; 260];
        let len = GetModuleBaseNameW(handle, None, &mut buf);
        let _ = CloseHandle(handle);
        if len == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]).to_lowercase())
    }
}

fn is_browser(name: &str) -> bool {
    BROWSER_PROCESSES.iter().any(|p| name == *p)
}

fn collect_browser_windows() -> Vec<HWND> {
    let mut list = WindowList(Vec::new());
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut list as *mut _ as isize));
    }
    list.0
        .into_iter()
        .filter(|hwnd| {
            process_name_for_window(*hwnd)
                .map(|n| is_browser(&n))
                .unwrap_or(false)
        })
        .collect()
}

struct UIA {
    automation: IUIAutomation,
    edit_condition: IUIAutomationCondition,
}

impl UIA {
    fn new() -> windows::core::Result<Self> {
        unsafe {
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            let variant = VARIANT::from(UIA_EditControlTypeId.0);
            let edit_condition =
                automation.CreatePropertyCondition(UIA_ControlTypePropertyId, &variant)?;
            Ok(Self {
                automation,
                edit_condition,
            })
        }
    }

    fn read_urls(&self, hwnd: HWND) -> Vec<String> {
        let mut out = Vec::new();
        unsafe {
            let Ok(root) = self.automation.ElementFromHandle(hwnd) else {
                return out;
            };
            let Ok(edits) = root.FindAll(TreeScope_Descendants, &self.edit_condition) else {
                return out;
            };
            let Ok(len) = edits.Length() else {
                return out;
            };
            for i in 0..len {
                let Ok(el) = edits.GetElement(i) else {
                    continue;
                };
                if let Some(value) = read_value(&el) {
                    if looks_like_url(&value) {
                        out.push(value);
                    }
                }
            }
        }
        out
    }
}

fn read_value(el: &IUIAutomationElement) -> Option<String> {
    unsafe {
        let unk = el.GetCurrentPattern(UIA_ValuePatternId).ok()?;
        let pattern: IUIAutomationValuePattern = unk.cast().ok()?;
        let bstr = pattern.CurrentValue().ok()?;
        let s = bstr.to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

fn looks_like_url(s: &str) -> bool {
    let lower = s.to_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.contains(".com")
        || lower.contains(".io")
        || lower.contains(".net")
        || lower.contains(".org")
}

fn url_matches(url: &str, patterns: &[String]) -> bool {
    let lower = url.to_lowercase();
    patterns.iter().any(|p| lower.contains(p))
}

fn send_ctrl_w() {
    unsafe {
        let inputs = [
            key_input(VK_CONTROL, false),
            key_input(VK_W, false),
            key_input(VK_W, true),
            key_input(VK_CONTROL, true),
        ];
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn key_input(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if key_up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

pub fn spawn(cfg: SharedConfig, stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let uia = match UIA::new() {
            Ok(u) => u,
            Err(e) => {
                eprintln!("UIA init failed: {e:?}");
                return;
            }
        };

        let mut last_action = Instant::now() - Duration::from_secs(60);
        let cooldown = Duration::from_millis(800);

        while !stop.load(Ordering::Relaxed) {
            let patterns = active_patterns(&cfg.read().unwrap());
            if patterns.is_empty() {
                thread::sleep(Duration::from_millis(700));
                continue;
            }

            let foreground = unsafe { GetForegroundWindow() };
            let foreground_is_browser = foreground.0 as usize != 0
                && process_name_for_window(foreground)
                    .map(|n| is_browser(&n))
                    .unwrap_or(false);

            let windows_to_scan: Vec<HWND> = if foreground_is_browser {
                vec![foreground]
            } else {
                collect_browser_windows()
            };

            let mut hit: Option<HWND> = None;
            for hwnd in windows_to_scan {
                for url in uia.read_urls(hwnd) {
                    if url_matches(&url, &patterns) {
                        hit = Some(hwnd);
                        break;
                    }
                }
                if hit.is_some() {
                    break;
                }
            }

            if let Some(hwnd) = hit {
                let foreground_now = unsafe { GetForegroundWindow() };
                if hwnd_eq(foreground_now, hwnd) && last_action.elapsed() > cooldown {
                    send_ctrl_w();
                    last_action = Instant::now();
                    thread::sleep(Duration::from_millis(450));
                    continue;
                }
            }

            thread::sleep(Duration::from_millis(350));
        }

        unsafe { CoUninitialize() };
    });
}
