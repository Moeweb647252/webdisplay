use crate::capture::dda::MonitorInfo;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_LCONTROL, VK_LMENU,
    VK_LSHIFT, VK_LWIN, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

#[derive(Debug, Clone, Copy)]
pub struct ActiveMonitor {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

impl ActiveMonitor {
    pub fn from_info(info: &MonitorInfo) -> Self {
        Self {
            left: info.left,
            top: info.top,
            width: info.width,
            height: info.height,
        }
    }
}

pub struct InputInjector {
    virtual_left: i32,
    virtual_top: i32,
    virtual_width: i32,
    virtual_height: i32,
}

const XBUTTON1_DATA: u32 = 0x0001;
const XBUTTON2_DATA: u32 = 0x0002;

impl InputInjector {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let virtual_left = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let virtual_top = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let virtual_width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let virtual_height = GetSystemMetrics(SM_CYVIRTUALSCREEN);

            if virtual_width <= 0 || virtual_height <= 0 {
                return Err("虚拟桌面尺寸无效".to_string());
            }

            Ok(Self {
                virtual_left,
                virtual_top,
                virtual_width,
                virtual_height,
            })
        }
    }

    pub fn move_mouse(&self, monitor: ActiveMonitor, x: f32, y: f32) -> Result<(), String> {
        let (desktop_x, desktop_y) = self.to_desktop_point(monitor, x, y);
        self.send_mouse_move(desktop_x, desktop_y)
    }

    pub fn mouse_button(
        &self,
        monitor: ActiveMonitor,
        x: f32,
        y: f32,
        button: u8,
        down: bool,
    ) -> Result<(), String> {
        let (desktop_x, desktop_y) = self.to_desktop_point(monitor, x, y);
        let Some((button_flags, button_data)) = mouse_button_flags(button, down) else {
            return Ok(());
        };

        let inputs = [
            self.mouse_input_absolute(desktop_x, desktop_y, MOUSEEVENTF_MOVE),
            mouse_input(0, 0, button_data, button_flags),
        ];
        self.send_inputs(&inputs)
    }

    pub fn mouse_wheel(
        &self,
        monitor: ActiveMonitor,
        x: f32,
        y: f32,
        delta_x: i32,
        delta_y: i32,
    ) -> Result<(), String> {
        let (desktop_x, desktop_y) = self.to_desktop_point(monitor, x, y);

        let mut inputs = Vec::with_capacity(3);
        inputs.push(self.mouse_input_absolute(desktop_x, desktop_y, MOUSEEVENTF_MOVE));

        if delta_y != 0 {
            inputs.push(mouse_input(0, 0, delta_y as u32, MOUSEEVENTF_WHEEL));
        }
        if delta_x != 0 {
            inputs.push(mouse_input(0, 0, delta_x as u32, MOUSEEVENTF_HWHEEL));
        }

        self.send_inputs(&inputs)
    }

    pub fn keyboard_key(
        &self,
        key_code: u16,
        code: Option<&str>,
        down: bool,
    ) -> Result<(), String> {
        if key_code == 0 {
            return Ok(());
        }

        let vk = map_virtual_key(key_code, code);
        let flags = if down {
            KEYBD_EVENT_FLAGS(0)
        } else {
            KEYEVENTF_KEYUP
        };

        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };

        self.send_inputs(&[input])
    }

    fn send_mouse_move(&self, desktop_x: i32, desktop_y: i32) -> Result<(), String> {
        let input = self.mouse_input_absolute(desktop_x, desktop_y, MOUSEEVENTF_MOVE);
        self.send_inputs(&[input])
    }

    fn mouse_input_absolute(
        &self,
        desktop_x: i32,
        desktop_y: i32,
        flags: MOUSE_EVENT_FLAGS,
    ) -> INPUT {
        let abs_x = to_sendinput_absolute(desktop_x, self.virtual_left, self.virtual_width);
        let abs_y = to_sendinput_absolute(desktop_y, self.virtual_top, self.virtual_height);

        mouse_input(
            abs_x,
            abs_y,
            0,
            flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )
    }

    fn send_inputs(&self, inputs: &[INPUT]) -> Result<(), String> {
        if inputs.is_empty() {
            return Ok(());
        }

        unsafe {
            let sent = SendInput(inputs, std::mem::size_of::<INPUT>() as i32) as usize;
            if sent != inputs.len() {
                let err = GetLastError();
                return Err(format!(
                    "SendInput 发送不完整: sent={}, expected={}, err={}",
                    sent,
                    inputs.len(),
                    err.0
                ));
            }
        }

        Ok(())
    }

    fn to_desktop_point(&self, monitor: ActiveMonitor, x: f32, y: f32) -> (i32, i32) {
        let clamped_x = x.clamp(0.0, 1.0);
        let clamped_y = y.clamp(0.0, 1.0);

        let width = monitor.width.max(1);
        let height = monitor.height.max(1);
        let local_x = (clamped_x * (width - 1) as f32).round() as i32;
        let local_y = (clamped_y * (height - 1) as f32).round() as i32;

        (monitor.left + local_x, monitor.top + local_y)
    }
}

fn mouse_input(dx: i32, dy: i32, mouse_data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_button_flags(button: u8, down: bool) -> Option<(MOUSE_EVENT_FLAGS, u32)> {
    match button {
        0 => Some((
            if down {
                MOUSEEVENTF_LEFTDOWN
            } else {
                MOUSEEVENTF_LEFTUP
            },
            0,
        )),
        1 => Some((
            if down {
                MOUSEEVENTF_MIDDLEDOWN
            } else {
                MOUSEEVENTF_MIDDLEUP
            },
            0,
        )),
        2 => Some((
            if down {
                MOUSEEVENTF_RIGHTDOWN
            } else {
                MOUSEEVENTF_RIGHTUP
            },
            0,
        )),
        3 => Some((
            if down {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            XBUTTON1_DATA,
        )),
        4 => Some((
            if down {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            XBUTTON2_DATA,
        )),
        _ => None,
    }
}

fn to_sendinput_absolute(value: i32, virtual_origin: i32, virtual_size: i32) -> i32 {
    if virtual_size <= 1 {
        return 0;
    }

    let numerator = (value - virtual_origin) as i64 * 65535;
    let denominator = (virtual_size - 1) as i64;
    (numerator / denominator).clamp(0, 65535) as i32
}

fn map_virtual_key(key_code: u16, code: Option<&str>) -> VIRTUAL_KEY {
    match code {
        Some("ShiftLeft") => VK_LSHIFT,
        Some("ShiftRight") => VK_RSHIFT,
        Some("ControlLeft") => VK_LCONTROL,
        Some("ControlRight") => VK_RCONTROL,
        Some("AltLeft") => VK_LMENU,
        Some("AltRight") => VK_RMENU,
        Some("MetaLeft") => VK_LWIN,
        Some("MetaRight") => VK_RWIN,
        _ => VIRTUAL_KEY(key_code),
    }
}
