//! 在电脑端持续监视「当前焦点输入框」：当输入框里的文字停止变化约 `pause` 秒后，
//! 检测其中是否出现逐字累积式的重复，若出现就用清洗后的最终文字替换。
//!
//! 读取/写入通过 Windows UI Automation 完成，因此只对支持无障碍（UIA）的
//! 普通输入框有效：记事本、多数聊天/浏览器输入框等。密码框不会读取。

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::repetition::clean_repetition;

#[derive(Clone, Copy, Debug)]
pub struct RepeatWatchConfig {
    pub enabled: bool,
    pub pause_ms: u64,
    pub interval_ms: u64,
}

impl Default for RepeatWatchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            pause_ms: 2000,
            interval_ms: 150,
        }
    }
}

/// 只监视长度不超过该值的文本；更长的（例如整个网页文档）忽略。
const MAX_WATCH_CHARS: usize = 2000;
const MAX_WATCH_CHARS_I32: i32 = MAX_WATCH_CHARS as i32;

pub type SharedRepeatWatch = Arc<Mutex<RepeatWatchConfig>>;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 启动后台监视线程。非 Windows 平台为空实现。
pub fn spawn_watcher(config: SharedRepeatWatch) {
    #[cfg(windows)]
    {
        thread::spawn(move || run_watcher(config));
    }
    #[cfg(not(windows))]
    {
        let _ = config;
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::ffi::c_void;
    use uiautomation::patterns::{UITextPattern, UIValuePattern};
    use uiautomation::{UIAutomation, UIElement};

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
        fn QueryFullProcessImageNameW(
            process: *mut c_void,
            flags: u32,
            exe_name: *mut u16,
            size: *mut u32,
        ) -> i32;
        fn CloseHandle(object: *mut c_void) -> i32;
    }

    /// Terminal hosts and shells expose their scrollback as an editable UIA
    /// element. Never run repetition cleanup against terminal text: rewriting
    /// that element can replace or disturb the user's command line/scrollback.
    /// Matched case-insensitively against the owning process's image name.
    const EXCLUDED_TERMINAL_PROCESSES: &[&str] = &[
        // The interactive agent CLI: its full-screen input box lives in a
        // terminal, so its text must never be cleaned.
        "agent.exe",
        // Windows Terminal and the classic console host.
        "windowsterminal.exe",
        "conhost.exe",
        "openconsole.exe",
        // Common shells running inside a terminal.
        "cmd.exe",
        "powershell.exe",
        "pwsh.exe",
        "openshell.exe",
        "bash.exe",
        "wsl.exe",
        // Other terminal emulators.
        "mintty.exe",
        "alacritty.exe",
        "wezterm-gui.exe",
        "tabby.exe",
    ];

    fn process_image_path(process_id: u32) -> Option<String> {
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
            if process.is_null() {
                return None;
            }
            let mut buffer = vec![0u16; 32768];
            let mut size = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut size);
            let _ = CloseHandle(process);
            if ok == 0 {
                return None;
            }
            Some(String::from_utf16_lossy(&buffer[..size as usize]))
        }
    }

    fn is_terminal_process_name(name: &str) -> bool {
        EXCLUDED_TERMINAL_PROCESSES
            .iter()
            .any(|excluded| name.eq_ignore_ascii_case(excluded))
    }

    pub fn is_excluded_terminal(element: &UIElement) -> bool {
        // Fail closed: if UIA gives us an element but we cannot identify its
        // owner process, do not risk changing text in an unknown target.
        let Some(path) = element.get_process_id().ok().and_then(process_image_path) else {
            return true;
        };
        let name = std::path::Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        is_terminal_process_name(name)
    }

    /// 焦点元素当前的文字；读不到返回 None。
    /// 超过 [`MAX_WATCH_CHARS`] 的文本（例如整个网页文档）直接忽略，避免误操作与性能问题。
    pub fn read_text(element: &UIElement) -> Option<String> {
        if let Ok(value) = element.get_pattern::<UIValuePattern>() {
            if let Ok(text) = value.get_value() {
                if text.chars().count() <= MAX_WATCH_CHARS {
                    return Some(text);
                }
                return None;
            }
        }
        if let Ok(text_pattern) = element.get_pattern::<UITextPattern>() {
            if let Ok(range) = text_pattern.get_document_range() {
                if let Ok(text) = range.get_text(MAX_WATCH_CHARS_I32) {
                    if text.chars().count() <= MAX_WATCH_CHARS {
                        return Some(text);
                    }
                }
            }
        }
        None
    }

    /// 把焦点元素文字替换为 `text`。
    ///
    /// 优先使用**真实键盘输入**（聚焦 → 全选 → Ctrl+V 粘贴），而不是
    /// ValuePattern.set_value。原因：WebView2 / Electron / 浏览器里的输入框
    /// （例如 OpenCode 客户端）通常是受控组件，`set_value` 只改画面、
    /// 不改组件内部状态，发送时又会被重新渲染回旧文本；只有真实按键才会
    /// 触发 input 事件、让内部状态同步。粘贴同时会把光标留在文字末尾。
    ///
    /// 若真实输入不可用（例如控件没有 TextPattern 且无法聚焦），再退回
    /// ValuePattern；若控件明确只读，则不做任何修改。
    pub fn write_text(element: &UIElement, text: &str) -> bool {
        if paste_replace(element, text) {
            return true;
        }
        if let Ok(value) = element.get_pattern::<UIValuePattern>() {
            if value.is_readonly().unwrap_or(true) {
                // 明确只读：不要改用粘贴，直接放弃。
                return false;
            }
            if value.set_value(text).is_ok() {
                return true;
            }
        }
        false
    }

    /// 聚焦元素后用真实键盘「全选 + 粘贴」替换内容，并把光标移到末尾。
    /// 只有确认元素真正拿到焦点时才发送按键，避免把内容粘到别处。
    fn paste_replace(element: &UIElement, text: &str) -> bool {
        if element.set_focus().is_err() {
            return false;
        }
        thread::sleep(Duration::from_millis(80));
        if !element.has_keyboard_focus().unwrap_or(false) {
            return false;
        }

        // 全选：优先用 TextPattern 精确选中该控件内容，否则退回 Ctrl+A。
        let selected = element
            .get_pattern::<UITextPattern>()
            .ok()
            .and_then(|pattern| pattern.get_document_range().ok())
            .and_then(|range| range.select().ok())
            .is_some();
        if !selected && send_key_combo(true, Some(enigo::Key::Unicode('a'))).is_err() {
            return false;
        }
        thread::sleep(Duration::from_millis(60));

        if paste_clipboard(text).is_err() {
            return false;
        }
        thread::sleep(Duration::from_millis(140));

        // 保险：把光标移到文字末尾，避免停留在开头。
        let _ = send_key_combo(true, Some(enigo::Key::End));
        true
    }

    /// 发送一个按键；`ctrl` 为真时同时按住 Ctrl。`key` 为 None 表示只按/放 Ctrl。
    fn send_key_combo(ctrl: bool, key: Option<enigo::Key>) -> Result<(), String> {
        use enigo::{Direction, Enigo, Key, Keyboard, Settings};

        let mut enigo = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
        if ctrl {
            enigo
                .key(Key::Control, Direction::Press)
                .map_err(|e| e.to_string())?;
        }
        if let Some(key) = key {
            enigo
                .key(key, Direction::Click)
                .map_err(|e| e.to_string())?;
        }
        if ctrl {
            enigo
                .key(Key::Control, Direction::Release)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn paste_clipboard(text: &str) -> Result<(), String> {
        let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
        clipboard.set_text(text).map_err(|e| e.to_string())?;
        thread::sleep(Duration::from_millis(60));
        send_key_combo(true, Some(enigo::Key::Unicode('v')))
    }

    pub struct WatchState {
        tracked_text: String,
        last_change_ms: u64,
        attempted: bool,
        started: bool,
    }

    impl WatchState {
        fn reset(&mut self) {
            self.tracked_text.clear();
            self.last_change_ms = now_ms();
            self.attempted = true;
            self.started = false;
        }
    }

    pub fn run(automation: UIAutomation, config: SharedRepeatWatch) {
        let mut state = WatchState {
            tracked_text: String::new(),
            last_change_ms: now_ms(),
            attempted: true,
            started: false,
        };
        let mut was_enabled = false;

        loop {
            let (enabled, pause_ms, interval_ms) = {
                let guard = match config.lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                };
                (guard.enabled, guard.pause_ms, guard.interval_ms)
            };

            thread::sleep(Duration::from_millis(interval_ms.max(30)));

            if !enabled {
                if was_enabled {
                    state.reset();
                }
                was_enabled = false;
                continue;
            }
            if !was_enabled {
                state.reset();
                was_enabled = true;
                tracing::info!("重复文字清洗：已开始监视当前焦点输入框");
            }

            let element = match automation.get_focused_element() {
                Ok(element) => element,
                Err(_) => continue,
            };
            // Terminals/shells are excluded: their scrollback is a UIA-editable
            // text surface, so cleaning it would disturb the running command
            // line and output.
            if is_excluded_terminal(&element) {
                state.reset();
                continue;
            }
            let Some(current) = read_text(&element) else {
                continue;
            };

            if current != state.tracked_text {
                state.tracked_text = current;
                state.last_change_ms = now_ms();
                state.attempted = false;
                state.started = true;
                continue;
            }

            if state.attempted || !state.started {
                continue;
            }
            if now_ms().saturating_sub(state.last_change_ms) < pause_ms {
                continue;
            }

            // 这个版本已经尝试过，避免反复计算。
            state.attempted = true;

            if state.tracked_text.chars().count() < 6 {
                continue;
            }
            let Some(cleaned) = clean_repetition(&state.tracked_text) else {
                continue;
            };

            if write_text(&element, &cleaned) {
                tracing::info!("重复文字清洗：已替换为最终文字");
                state.tracked_text = cleaned;
                state.last_change_ms = now_ms();
            }
        }
    }
}

#[cfg(windows)]
fn run_watcher(config: SharedRepeatWatch) {
    let automation = match uiautomation::UIAutomation::new() {
        Ok(automation) => automation,
        Err(error) => {
            tracing::error!("重复文字清洗：初始化 UI Automation 失败: {error}");
            return;
        }
    };
    win::run(automation, config);
}

#[cfg(not(windows))]
fn run_watcher(_config: SharedRepeatWatch) {}

/// 探测当前焦点输入框是否可读写，返回给手机端做状态提示。
pub fn probe_focused_input() -> (bool, bool, String) {
    #[cfg(windows)]
    {
        let automation = match uiautomation::UIAutomation::new() {
            Ok(automation) => automation,
            Err(error) => return (false, false, format!("初始化失败: {error}")),
        };
        let element = match automation.get_focused_element() {
            Ok(element) => element,
            Err(_) => return (false, false, "无法获取当前焦点元素".into()),
        };
        if win::is_excluded_terminal(&element) {
            let label = describe_element(&element);
            return (false, false, format!("终端/命令行已跳过清洗：{label}"));
        }
        let readable = win::read_text(&element).is_some();
        let writable = element
            .get_pattern::<uiautomation::patterns::UIValuePattern>()
            .map(|value| !value.is_readonly().unwrap_or(true))
            .unwrap_or(false);
        let label = describe_element(&element);
        (readable, writable, label)
    }
    #[cfg(not(windows))]
    {
        (false, false, "仅支持 Windows".into())
    }
}

/// 生成一个简短、可读的元素描述，供手机端展示。
#[cfg(windows)]
fn describe_element(element: &uiautomation::UIElement) -> String {
    let control = element
        .get_control_type()
        .map(|control| format!("{control:?}"))
        .unwrap_or_default();
    let name = element.get_name().unwrap_or_default();
    let name = name.trim();
    if !name.is_empty() {
        let name: String = name.chars().take(24).collect();
        return format!("{control} 「{name}」");
    }
    // WebView2/Electron 的 ClassName 常是一长串 CSS 类名，这里做截断。
    let class = element.get_classname().unwrap_or_default();
    let class: String = class.trim().chars().take(32).collect();
    if class.is_empty() {
        control
    } else {
        format!("{control} {class}")
    }
}
