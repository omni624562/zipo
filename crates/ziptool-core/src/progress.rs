use std::io::{self, Read};
use std::path::Path;

/// 目的檔已存在時，使用者的回答（對應 7-Zip 的「確認取代檔案」對話框）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwriteAnswer {
    /// 取代這個檔案
    Yes,
    /// 全部取代
    YesAll,
    /// 略過這個檔案
    No,
    /// 全部略過
    NoAll,
    /// 之後全部自動重新命名
    RenameAll,
    /// 中止整個工作
    Cancel,
}

/// 進度回報介面。CLI 用進度條實作，GUI 用共享狀態實作。
/// 單位為位元組。
pub trait Progress: Sync {
    /// 開始處理，`total` 為預計處理的總位元組數。
    fn start(&self, _total: u64) {}
    /// 又處理了 `bytes` 位元組。
    fn advance(&self, _bytes: u64) {}
    /// 開始處理某個項目。
    fn entry(&self, _name: &str) {}
    /// 回傳 true 時，進行中的工作會盡快中止並回傳 `Error::Cancelled`。
    fn is_cancelled(&self) -> bool {
        false
    }
    /// 覆寫模式為 `Ask` 且目的檔已存在時呼叫；預設不詢問、直接取代。
    fn ask_overwrite(&self, _path: &Path) -> OverwriteAnswer {
        OverwriteAnswer::Yes
    }
}

/// 不回報進度。
pub struct NoProgress;
impl Progress for NoProgress {}

/// 讀取時同步回報進度並檢查取消。
pub(crate) struct ProgressReader<'a, R> {
    inner: R,
    progress: &'a dyn Progress,
}

impl<'a, R> ProgressReader<'a, R> {
    pub fn new(inner: R, progress: &'a dyn Progress) -> Self {
        Self { inner, progress }
    }
}

impl<R: Read> Read for ProgressReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.progress.is_cancelled() {
            // 不能用 Interrupted：io::copy 等會自動重試，造成無限迴圈。
            return Err(io::Error::other("cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.progress.advance(n as u64);
        Ok(n)
    }
}
