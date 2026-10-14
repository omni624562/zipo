//! Windows 工作列按鈕上的進度（ITaskbarList3）：切到其他視窗也看得到壓縮、解壓縮的進度。

#![cfg_attr(not(windows), allow(dead_code))]

use crate::jobs::Snapshot;

/// 工作列按鈕的進度狀態；進度以千分比表示，只有改變時才通知 Windows
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    None,
    /// 還不知道總量
    Indeterminate,
    /// 等待使用者回答（例如是否取代同名檔案）
    Paused(u64),
    Normal(u64),
}

/// 依進行中工作的狀態決定工作列要顯示什麼
pub fn state_for(job: Option<&Snapshot>) -> State {
    let Some(s) = job else { return State::None };
    let permille = (s.done.min(s.total) * 1000).checked_div(s.total).unwrap_or(0);
    if s.question.is_some() {
        State::Paused(permille)
    } else if s.total == 0 {
        State::Indeterminate
    } else {
        State::Normal(permille)
    }
}

#[cfg(windows)]
pub struct Taskbar {
    list: Option<windows::Win32::UI::Shell::ITaskbarList3>,
    hwnd: windows::Win32::Foundation::HWND,
    last: State,
}

#[cfg(windows)]
impl Taskbar {
    /// `hwnd`：主視窗；無法建立時（例如工作列不支援）就不顯示，不影響其他功能
    pub fn new(hwnd: isize) -> Self {
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
        use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList};
        // SAFETY: 在主執行緒初始化 COM 並建立工作列物件；失敗時不使用
        let list = unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER).ok().filter(|l| l.HrInit().is_ok())
        };
        Taskbar { list, hwnd: windows::Win32::Foundation::HWND(hwnd as *mut _), last: State::None }
    }

    pub fn set(&mut self, state: State) {
        use windows::Win32::UI::Shell::{TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED};
        if state == self.last {
            return;
        }
        self.last = state;
        let Some(list) = &self.list else { return };
        // SAFETY: hwnd 是本程式的主視窗；呼叫失敗只會讓工作列不顯示進度
        unsafe {
            let _ = match state {
                State::None => list.SetProgressState(self.hwnd, TBPF_NOPROGRESS),
                State::Indeterminate => list.SetProgressState(self.hwnd, TBPF_INDETERMINATE),
                State::Paused(v) => list.SetProgressState(self.hwnd, TBPF_PAUSED).and_then(|()| list.SetProgressValue(self.hwnd, v, 1000)),
                State::Normal(v) => list.SetProgressState(self.hwnd, TBPF_NORMAL).and_then(|()| list.SetProgressValue(self.hwnd, v, 1000)),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn snap(total: u64, done: u64, question: bool) -> Snapshot {
        Snapshot {
            total,
            done,
            current: String::new(),
            stage: String::new(),
            elapsed: Duration::ZERO,
            speed: 0.0,
            remaining: None,
            cancelling: false,
            question: question.then(|| "a.txt".into()),
        }
    }

    #[test]
    fn states() {
        assert_eq!(state_for(None), State::None);
        assert_eq!(state_for(Some(&snap(0, 0, false))), State::Indeterminate);
        assert_eq!(state_for(Some(&snap(200, 50, false))), State::Normal(250));
        assert_eq!(state_for(Some(&snap(200, 500, false))), State::Normal(1000), "超過總量時不超過 100%");
        assert_eq!(state_for(Some(&snap(200, 100, true))), State::Paused(500), "等待回答時暫停");
    }

    /// 這台電腦能建立 Windows 的工作列物件（不需要真的視窗）
    #[cfg(windows)]
    #[test]
    fn taskbar_object_is_created() {
        let mut t = Taskbar::new(0);
        assert!(t.list.is_some());
        // 沒有視窗時呼叫失敗也不出錯
        t.set(State::Normal(500));
        t.set(State::None);
    }
}
