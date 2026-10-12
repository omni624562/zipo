//! 介面語言：繁體中文（預設）或英文。
//!
//! 字串以 `L!("中文", "English")` 或 `F!("中文{x}", "English {x}", …)`（同 `format!`）撰寫，
//! 兩種語言放在一起，格式參數由編譯器檢查。

use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// 只影響目前執行緒的語言（測試用：平行執行的其他測試不受影響）
    static THREAD_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// 目前是否使用英文
pub fn english() -> bool {
    THREAD_OVERRIDE.with(|o| o.get()).unwrap_or_else(|| ENGLISH.load(Ordering::Relaxed))
}

/// 在 `f` 執行期間，讓目前執行緒使用指定的語言（不影響其他執行緒）
pub fn with_language<T>(english: bool, f: impl FnOnce() -> T) -> T {
    let prev = THREAD_OVERRIDE.with(|o| o.replace(Some(english)));
    let r = f();
    THREAD_OVERRIDE.with(|o| o.set(prev));
    r
}

pub fn set_english(on: bool) {
    ENGLISH.store(on, Ordering::Relaxed);
}

/// 依語言選擇（給常數表等無法使用巨集的地方）
pub fn pick(zh: &'static str, en: &'static str) -> &'static str {
    if english() { en } else { zh }
}

/// 系統的介面語言不是中文時，預設使用英文
pub fn system_prefers_english() -> bool {
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn GetUserDefaultUILanguage() -> u16;
        }
        const LANG_CHINESE: u16 = 0x04;
        // SAFETY: 沒有參數，只回傳語言代碼
        let lang = unsafe { GetUserDefaultUILanguage() };
        lang & 0x3FF != LANG_CHINESE
    }
    #[cfg(not(windows))]
    {
        let lang = std::env::var("LC_ALL").or_else(|_| std::env::var("LANG")).unwrap_or_default();
        !lang.to_lowercase().starts_with("zh")
    }
}

/// 依目前語言選擇字串：`L!("中文", "English")`
#[macro_export]
macro_rules! L {
    ($zh:literal, $en:literal $(,)?) => {
        if $crate::i18n::english() { $en } else { $zh }
    };
}

/// 依目前語言格式化：`F!("中文{x}", "English {x}", 參數…)`，用法同 `format!`
#[macro_export]
macro_rules! F {
    ($zh:literal, $en:literal $(, $($arg:tt)*)?) => {
        if $crate::i18n::english() { ::std::format!($en $(, $($arg)*)?) } else { ::std::format!($zh $(, $($arg)*)?) }
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn switches_language() {
        let n = 3;
        assert_eq!(L!("中文", "English"), "中文");
        assert_eq!(F!("{n} 個", "{n} items"), "3 個");
        let (l, f) = super::with_language(true, || (L!("中文", "English"), F!("{n} 個", "{n} items")));
        assert_eq!((l, f.as_str()), ("English", "3 items"));
        assert_eq!(L!("中文", "English"), "中文", "離開後恢復");
    }
}
