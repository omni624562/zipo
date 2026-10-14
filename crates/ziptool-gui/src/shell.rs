//! 檔案總管右鍵選單整合（Windows）。
//!
//! 只寫入目前使用者的設定（`HKCU\Software\Classes`），不需要系統管理員權限，
//! 也不會更改壓縮檔的預設開啟程式；隨時可以從「工具」選單移除。
//! 預設程式則另外註冊（`register_default`），最後由使用者在 Windows 設定中選取。

#![cfg_attr(not(windows), allow(dead_code))]

use ziptool_core::L;
use std::path::{Path, PathBuf};

/// 實際使用的位置；測試時改用其他機碼，不影響真實設定。
pub const ROOT: &str = r"Software\Classes";

/// 右鍵選單要出現的壓縮檔副檔名；.001 是分割壓縮檔的第一個（例如 a.zip.001）
pub const ARCHIVE_EXTS: [&str; 12] = [".zip", ".7z", ".tar", ".gz", ".tgz", ".bz2", ".tbz2", ".xz", ".txz", ".zst", ".tzst", ".001"];

/// （機碼路徑, 顯示文字, 命令列參數）
fn verbs(exe: &Path) -> Vec<(String, &'static str, String)> {
    let exe = exe.display();
    let mut v = vec![
        (r"*\shell\ZipTool.Add".to_string(), L!("用 Zipo 壓縮…", "Compress with Zipo…"), format!("\"{exe}\" --add \"%1\"")),
        (r"Directory\shell\ZipTool.Add".to_string(), L!("用 Zipo 壓縮…", "Compress with Zipo…"), format!("\"{exe}\" --add \"%1\"")),
    ];
    for ext in ARCHIVE_EXTS {
        let base = format!(r"SystemFileAssociations\{ext}\shell");
        v.push((format!(r"{base}\ZipTool.Open"), L!("用 Zipo 開啟", "Open with Zipo"), format!("\"{exe}\" \"%1\"")));
        v.push((format!(r"{base}\ZipTool.ExtractHere"), L!("解壓縮到這裡（Zipo）", "Extract here (Zipo)"), format!("\"{exe}\" --extract-here \"%1\"")));
        v.push((format!(r"{base}\ZipTool.ExtractTo"), L!("解壓縮到同名資料夾（Zipo）", "Extract to a folder of the same name (Zipo)"), format!("\"{exe}\" --extract-to \"%1\"")));
        v.push((format!(r"{base}\ZipTool.ExtractFolder"), L!("解壓縮到資料夾（Zipo）…", "Extract to folder (Zipo)…"), format!("\"{exe}\" --extract-folder \"%1\"")));
    }
    v
}

#[cfg(windows)]
pub fn install(root: &str, exe: &Path) -> std::io::Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for (key, label, command) in verbs(exe) {
        let (k, _) = hkcu.create_subkey(format!(r"{root}\{key}"))?;
        k.set_value("MUIVerb", &label)?;
        k.set_value("Icon", &format!("\"{}\",0", exe.display()))?;
        let (c, _) = k.create_subkey("command")?;
        c.set_value("", &command)?;
    }
    Ok(())
}

#[cfg(windows)]
pub fn uninstall(root: &str) -> std::io::Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for (key, _, _) in verbs(Path::new("")) {
        match hkcu.delete_subkey_all(format!(r"{root}\{key}")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn is_installed(root: &str) -> bool {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    RegKey::predef(HKEY_CURRENT_USER).open_subkey(format!(r"{root}\*\shell\ZipTool.Add")).is_ok()
}

/// 啟動時呼叫：已加入右鍵選單或已註冊預設程式，但缺少新版才有的項目（例如 .001）、
/// 選單文字或命令與目前不同（新版改了文字、切換了介面語言、程式搬了位置）時更新；使用者沒有啟用的不會自行加上
#[cfg(windows)]
pub fn refresh(root: &str, base: &str, exe: &Path) {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let outdated = |key: &str, label: &str, command: &str| {
        let label_now: Option<String> = hkcu.open_subkey(format!(r"{root}\{key}")).and_then(|k| k.get_value("MUIVerb")).ok();
        let command_now: Option<String> = hkcu.open_subkey(format!(r"{root}\{key}\command")).and_then(|k| k.get_value("")).ok();
        label_now.as_deref() != Some(label) || command_now.as_deref() != Some(command)
    };
    if is_installed(root) && verbs(exe).iter().any(|(key, label, command)| outdated(key, label, command)) {
        let _ = install(root, exe);
    }
    let missing = |ext: &str| {
        hkcu.open_subkey(format!(r"{base}\Classes\{ext}\OpenWithProgids")).and_then(|k| k.get_raw_value(PROG_ID)).is_err()
            || hkcu.open_subkey(format!(r"{base}\{APP_NAME}\Capabilities\FileAssociations")).and_then(|k| k.get_raw_value(ext)).is_err()
    };
    if is_default_registered(base) && ARCHIVE_EXTS.iter().any(|ext| missing(ext)) {
        let _ = register_default(base, exe);
    }
}

#[cfg(not(windows))]
pub fn refresh(_root: &str, _base: &str, _exe: &Path) {}

/// 預設程式註冊用的 ProgID 與「已註冊的應用程式」名稱
pub const PROG_ID: &str = "ZipTool.Archive";
/// 機碼名稱維持舊名，已設定的預設程式才不會失效；顯示名稱見 `DISPLAY_NAME`
pub const APP_NAME: &str = "ZipTool";
pub const DISPLAY_NAME: &str = "Zipo";
/// 實際使用的位置；測試時改用其他機碼。其下會用到 `Classes`、`ZipTool\Capabilities`、`RegisteredApplications`。
pub const SOFTWARE: &str = "Software";

/// 向 Windows 註冊「Zipo 可以開啟這些壓縮檔」。
/// Windows 10／11 不允許程式自行變更預設程式，註冊後還要由使用者在「設定」中選取。
#[cfg(windows)]
pub fn register_default(base: &str, exe: &Path) -> std::io::Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let exe = exe.display();
    let (prog, _) = hkcu.create_subkey(format!(r"{base}\Classes\{PROG_ID}"))?;
    prog.set_value("", &L!("Zipo 壓縮檔", "Zipo archive"))?;
    // 索引 1：壓縮檔圖示（見 assets/ziptool.rc）
    prog.create_subkey("DefaultIcon")?.0.set_value("", &format!("\"{exe}\",1"))?;
    prog.create_subkey(r"shell\open\command")?.0.set_value("", &format!("\"{exe}\" \"%1\""))?;

    let (caps, _) = hkcu.create_subkey(format!(r"{base}\{APP_NAME}\Capabilities"))?;
    caps.set_value("ApplicationName", &DISPLAY_NAME)?;
    caps.set_value("ApplicationDescription", &L!("壓縮與解壓縮 ZIP、7z、TAR 等格式", "Compress and extract ZIP, 7z, TAR and more"))?;
    let (assoc, _) = caps.create_subkey("FileAssociations")?;
    for ext in ARCHIVE_EXTS {
        assoc.set_value(ext, &PROG_ID)?;
        // 出現在「開啟檔案」的程式清單中
        hkcu.create_subkey(format!(r"{base}\Classes\{ext}\OpenWithProgids"))?.0.set_value(PROG_ID, &"")?;
    }
    hkcu.create_subkey(format!(r"{base}\RegisteredApplications"))?
        .0
        .set_value(APP_NAME, &format!(r"Software\{APP_NAME}\Capabilities"))?;
    notify_assoc_changed();
    Ok(())
}

#[cfg(windows)]
pub fn unregister_default(base: &str) -> std::io::Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let ignore_missing = |r: std::io::Result<()>| match r {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    };
    ignore_missing(hkcu.delete_subkey_all(format!(r"{base}\Classes\{PROG_ID}")))?;
    // 只刪除自己建立的 Capabilities；同名機碼下若有其他程式的設定則保留，空了才刪除上層機碼
    ignore_missing(hkcu.delete_subkey_all(format!(r"{base}\{APP_NAME}\Capabilities")))?;
    if let Ok(k) = hkcu.open_subkey(format!(r"{base}\{APP_NAME}"))
        && k.enum_keys().next().is_none()
        && k.enum_values().next().is_none()
    {
        drop(k);
        ignore_missing(hkcu.delete_subkey(format!(r"{base}\{APP_NAME}")))?;
    }
    for ext in ARCHIVE_EXTS {
        if let Ok(k) = hkcu.open_subkey_with_flags(format!(r"{base}\Classes\{ext}\OpenWithProgids"), winreg::enums::KEY_SET_VALUE) {
            ignore_missing(k.delete_value(PROG_ID))?;
        }
    }
    if let Ok(k) = hkcu.open_subkey_with_flags(format!(r"{base}\RegisteredApplications"), winreg::enums::KEY_SET_VALUE) {
        ignore_missing(k.delete_value(APP_NAME))?;
    }
    notify_assoc_changed();
    Ok(())
}

#[cfg(windows)]
pub fn is_default_registered(base: &str) -> bool {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    RegKey::predef(HKEY_CURRENT_USER).open_subkey(format!(r"{base}\Classes\{PROG_ID}")).is_ok()
}

/// 開啟 Windows「設定 › 預設應用程式」中 Zipo 的頁面
#[cfg(windows)]
pub fn open_default_apps_settings() {
    let _ = std::process::Command::new("explorer").arg(format!("ms-settings:defaultapps?registeredAppUser={APP_NAME}")).spawn();
}

/// 通知檔案總管更新檔案關聯（圖示、開啟方式）
#[cfg(windows)]
fn notify_assoc_changed() {
    unsafe extern "system" {
        fn SHChangeNotify(event: i32, flags: u32, item1: *const std::ffi::c_void, item2: *const std::ffi::c_void);
    }
    const SHCNE_ASSOCCHANGED: i32 = 0x0800_0000;
    // SAFETY: 這個事件不使用 item 參數，傳空指標即可
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, 0, std::ptr::null(), std::ptr::null()) };
}

/// 在檔案總管選取多個檔案執行右鍵選單的項目時，Windows 會每個檔案各啟動一個 Zipo。
/// 第一個啟動的負責收集，其他的把路徑交給它後結束，最後只開一個視窗、處理全部檔案。
/// `kind` 區分不同的選單項目（add、extract-here…）；回傳 `None` 代表路徑已交給另一個 Zipo，這個應直接結束。
#[cfg(windows)]
pub fn collect(kind: &str, paths: Vec<PathBuf>) -> Option<Vec<PathBuf>> {
    let queue = std::env::temp_dir().join("Zipo").join(format!("{kind}-queue"));
    collect_in(&queue, &format!(r"Local\Zipo.Collect.{kind}"), paths, std::time::Duration::from_millis(400))
}

/// 路徑以 UTF-16 存放（Windows 的檔名可能不是有效的 UTF-8），每個以 0 結尾
#[cfg(windows)]
fn encode_paths(paths: &[PathBuf]) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    paths.iter().flat_map(|p| p.as_os_str().encode_wide().chain([0])).flat_map(u16::to_le_bytes).collect()
}

#[cfg(windows)]
fn decode_paths(bytes: &[u8]) -> Vec<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let wide: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    wide.split(|&c| c == 0).filter(|p| !p.is_empty()).map(|p| PathBuf::from(std::ffi::OsString::from_wide(p))).collect()
}

/// `queue`：放路徑的資料夾；`mutex`：收集者的具名 mutex（行程結束時自動釋放）；
/// `quiet`：這段時間內沒有新的路徑就停止收集（最多 3 秒）
#[cfg(windows)]
pub fn collect_in(queue: &Path, mutex: &str, paths: Vec<PathBuf>, quiet: std::time::Duration) -> Option<Vec<PathBuf>> {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant, SystemTime};
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;
    static SEQ: AtomicU32 = AtomicU32::new(0);
    if std::fs::create_dir_all(queue).is_err() {
        return Some(paths);
    }
    // 先放進佇列（寫完才改名，讀取時不會看到寫一半的檔案），再確認有沒有收集者：
    // 收集者停止收集後還會再讀一次佇列，所以不會漏掉
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let base = format!("{}-{stamp}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
    let mine = queue.join(format!("{base}.paths"));
    let tmp = queue.join(format!("{base}.tmp"));
    if std::fs::write(&tmp, encode_paths(&paths)).and_then(|()| std::fs::rename(&tmp, &mine)).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Some(paths);
    }
    let name: Vec<u16> = mutex.encode_utf16().chain([0]).collect();
    // SAFETY: 名稱為以 0 結尾的 UTF-16 字串；建立後立即讀取錯誤碼
    let (handle, exists) = unsafe {
        let h = CreateMutexW(None, false, PCWSTR(name.as_ptr()));
        (h, GetLastError() == ERROR_ALREADY_EXISTS)
    };
    let Ok(handle) = handle else {
        let _ = std::fs::remove_file(&mine);
        return Some(paths);
    };
    if exists {
        // SAFETY: 關閉自己取得的控制代碼
        unsafe {
            let _ = CloseHandle(handle);
        }
        return None;
    }
    // 收集：超過一分鐘的是之前異常結束留下的，不採用
    let fresh = |e: &std::fs::DirEntry| e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d < Duration::from_secs(60));
    let pending = || -> Vec<std::fs::DirEntry> {
        let mut v: Vec<_> = std::fs::read_dir(queue).map(|d| d.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "paths")).collect()).unwrap_or_default();
        v.sort_by_key(|e| e.file_name());
        v
    };
    let started = Instant::now();
    let (mut count, mut changed) = (pending().len(), Instant::now());
    while changed.elapsed() < quiet && started.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(30));
        let n = pending().len();
        if n != count {
            (count, changed) = (n, Instant::now());
        }
    }
    // SAFETY: 關閉自己建立的 mutex；之後才啟動的會成為新的收集者
    unsafe {
        let _ = CloseHandle(handle);
    }
    let mut all = Vec::new();
    for e in pending() {
        if fresh(&e)
            && let Ok(bytes) = std::fs::read(e.path())
        {
            all.extend(decode_paths(&bytes));
        }
        let _ = std::fs::remove_file(e.path());
    }
    // 自己的路徑已被前一個收集者取走（交接的瞬間）：由它處理
    if all.is_empty() {
        return None;
    }
    all.sort();
    all.dedup();
    Some(all)
}

/// 剪貼簿的「檔案清單」格式（與檔案總管的複製、貼上相同）
#[cfg(windows)]
const CF_HDROP: u32 = 15;

/// DROPFILES 結構加上以 0 分隔、兩個 0 結尾的 UTF-16 路徑
#[cfg(windows)]
fn dropfiles_bytes(paths: &[PathBuf]) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    const HEADER: u32 = 20; // pFiles、pt（x、y）、fNC、fWide 各 4 位元組
    let mut out = Vec::new();
    for v in [HEADER, 0, 0, 0, 1] {
        out.extend(v.to_le_bytes());
    }
    for p in paths {
        out.extend(p.as_os_str().encode_wide().chain([0]).flat_map(u16::to_le_bytes));
    }
    out.extend([0, 0]);
    out
}

/// 讀出 HDROP 中的所有路徑
#[cfg(windows)]
fn hdrop_paths(h: windows::Win32::UI::Shell::HDROP) -> Vec<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::UI::Shell::DragQueryFileW;
    // SAFETY: h 是有效的 HDROP；緩衝區長度依查詢到的長度配置
    unsafe {
        (0..DragQueryFileW(h, u32::MAX, None))
            .map(|i| {
                let mut buf = vec![0u16; DragQueryFileW(h, i, None) as usize + 1];
                let n = DragQueryFileW(h, i, Some(&mut buf)) as usize;
                PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]))
            })
            .collect()
    }
}

/// 把檔案放到剪貼簿（之後可以在檔案總管貼上）
#[cfg(windows)]
pub fn copy_files(paths: &[PathBuf]) -> std::io::Result<()> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    let bytes = dropfiles_bytes(paths);
    // SAFETY: 配置的記憶體大小與寫入的內容相同；成功交給剪貼簿後由系統管理，失敗時自行釋放
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let ptr = GlobalLock(mem) as *mut u8;
        if ptr.is_null() {
            let _ = GlobalFree(Some(mem));
            return Err(std::io::Error::last_os_error());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(mem);
        if let Err(e) = OpenClipboard(None) {
            let _ = GlobalFree(Some(mem));
            return Err(e.into());
        }
        let r = EmptyClipboard().and_then(|()| SetClipboardData(CF_HDROP, Some(HANDLE(mem.0))));
        let _ = CloseClipboard();
        if let Err(e) = r {
            let _ = GlobalFree(Some(mem));
            return Err(e.into());
        }
    }
    Ok(())
}

/// 剪貼簿上的檔案（在檔案總管按「複製」的檔案）；沒有時為空
#[cfg(windows)]
pub fn clipboard_files() -> Vec<PathBuf> {
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard};
    use windows::Win32::UI::Shell::HDROP;
    // SAFETY: 開啟剪貼簿期間讀取，讀完立即關閉
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP).is_err() || OpenClipboard(None).is_err() {
            return Vec::new();
        }
        let files = GetClipboardData(CF_HDROP).map(|h| hdrop_paths(HDROP(h.0))).unwrap_or_default();
        let _ = CloseClipboard();
        files
    }
}

#[cfg(not(windows))]
pub fn copy_files(_paths: &[PathBuf]) -> std::io::Result<()> {
    Err(std::io::Error::other(L!("只支援 Windows", "Only supported on Windows")))
}

#[cfg(not(windows))]
pub fn clipboard_files() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn collect(_kind: &str, paths: Vec<PathBuf>) -> Option<Vec<PathBuf>> {
    Some(paths)
}

#[cfg(not(windows))]
pub fn register_default(_base: &str, _exe: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(L!("只支援 Windows", "Only supported on Windows")))
}

#[cfg(not(windows))]
pub fn unregister_default(_base: &str) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
pub fn is_default_registered(_base: &str) -> bool {
    false
}

#[cfg(not(windows))]
pub fn open_default_apps_settings() {}

#[cfg(not(windows))]
pub fn install(_root: &str, _exe: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(L!("只支援 Windows", "Only supported on Windows")))
}

#[cfg(not(windows))]
pub fn uninstall(_root: &str) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
pub fn is_installed(_root: &str) -> bool {
    false
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    /// 寫到獨立的測試機碼，驗證後整個刪除，不影響實際的右鍵選單。
    #[test]
    fn install_and_uninstall_in_test_key() {
        let root = format!(r"Software\ZipToolTest-{}", std::process::id());
        let exe = Path::new(r"C:\Program Files\Zipo\zipo-gui.exe");
        install(&root, exe).unwrap();
        assert!(is_installed(&root));

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let cmd: String = hkcu.open_subkey(format!(r"{root}\SystemFileAssociations\.7z\shell\ZipTool.ExtractHere\command")).unwrap().get_value("").unwrap();
        assert_eq!(cmd, r#""C:\Program Files\Zipo\zipo-gui.exe" --extract-here "%1""#);
        let cmd: String = hkcu.open_subkey(format!(r"{root}\SystemFileAssociations\.zip\shell\ZipTool.ExtractTo\command")).unwrap().get_value("").unwrap();
        assert_eq!(cmd, r#""C:\Program Files\Zipo\zipo-gui.exe" --extract-to "%1""#);
        let label: String = hkcu.open_subkey(format!(r"{root}\Directory\shell\ZipTool.Add")).unwrap().get_value("MUIVerb").unwrap();
        assert_eq!(label, "用 Zipo 壓縮…");

        uninstall(&root).unwrap();
        assert!(!is_installed(&root));
        assert!(hkcu.open_subkey(format!(r"{root}\SystemFileAssociations\.zip\shell\ZipTool.Open")).is_err());
        // 再移除一次也不出錯
        uninstall(&root).unwrap();
        hkcu.delete_subkey_all(&root).unwrap();
    }

    /// 分割壓縮檔的第一個（.001）也有右鍵選單與「開啟方式」；舊版只註冊了部分副檔名時，啟動時會補上
    #[test]
    fn split_volumes_are_registered_and_refreshed() {
        let root = format!(r"Software\ZipToolSplitTest-{}", std::process::id());
        let base = format!(r"Software\ZipToolSplitDefaultTest-{}", std::process::id());
        let exe = Path::new(r"C:\Program Files\Zipo\zipo-gui.exe");
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        install(&root, exe).unwrap();
        register_default(&base, exe).unwrap();
        let cmd: String = hkcu.open_subkey(format!(r"{root}\SystemFileAssociations\.001\shell\ZipTool.ExtractHere\command")).unwrap().get_value("").unwrap();
        assert_eq!(cmd, r#""C:\Program Files\Zipo\zipo-gui.exe" --extract-here "%1""#);
        assert!(hkcu.open_subkey(format!(r"{base}\Classes\.001\OpenWithProgids")).unwrap().get_raw_value(PROG_ID).is_ok());

        // 模擬舊版的註冊（少了 .001）：啟動時補上
        hkcu.delete_subkey_all(format!(r"{root}\SystemFileAssociations\.001")).unwrap();
        hkcu.delete_subkey_all(format!(r"{base}\Classes\.001")).unwrap();
        refresh(&root, &base, exe);
        assert!(hkcu.open_subkey(format!(r"{root}\SystemFileAssociations\.001\shell\ZipTool.Open")).is_ok());
        assert!(hkcu.open_subkey(format!(r"{base}\Classes\.001\OpenWithProgids")).unwrap().get_raw_value(PROG_ID).is_ok());

        // 舊版的選單文字（或程式搬了位置）：啟動時更新
        hkcu.open_subkey_with_flags(format!(r"{root}\*\shell\ZipTool.Add"), winreg::enums::KEY_SET_VALUE).unwrap().set_value("MUIVerb", &"加入壓縮檔（Zipo）…").unwrap();
        refresh(&root, &base, exe);
        let label: String = hkcu.open_subkey(format!(r"{root}\*\shell\ZipTool.Add")).unwrap().get_value("MUIVerb").unwrap();
        assert_eq!(label, "用 Zipo 壓縮…");
        let moved = Path::new(r"D:\Apps\Zipo\zipo-gui.exe");
        refresh(&root, &base, moved);
        let cmd: String = hkcu.open_subkey(format!(r"{root}\SystemFileAssociations\.zip\shell\ZipTool.Open\command")).unwrap().get_value("").unwrap();
        assert_eq!(cmd, r#""D:\Apps\Zipo\zipo-gui.exe" "%1""#);

        // 沒有註冊過的不會自行加上
        uninstall(&root).unwrap();
        unregister_default(&base).unwrap();
        refresh(&root, &base, exe);
        assert!(!is_installed(&root));
        assert!(!is_default_registered(&base));
        let _ = hkcu.delete_subkey_all(&root);
        let _ = hkcu.delete_subkey_all(&base);
    }

    /// 檔案總管選取多個檔案「加入壓縮檔」時同時啟動多個 Zipo：只有一個收集到全部路徑，其餘直接結束
    #[test]
    fn simultaneous_add_requests_are_merged() {
        let t = tempfile::tempdir().unwrap();
        let queue = t.path().join("queue");
        let name = format!(r"Local\ZipoAddTest-{}", std::process::id());
        let names = ["c.txt", "a.txt", "資料夾 b"];
        let results: Vec<Option<Vec<std::path::PathBuf>>> = std::thread::scope(|s| {
            let handles: Vec<_> = names
                .iter()
                .map(|n| {
                    let (queue, name, p) = (queue.clone(), name.clone(), t.path().join(n));
                    s.spawn(move || collect_in(&queue, &name, vec![p], std::time::Duration::from_millis(300)))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let got: Vec<_> = results.into_iter().flatten().collect();
        assert_eq!(got.len(), 1, "只能有一個開啟對話框：{got:?}");
        let mut want: Vec<_> = names.iter().map(|n| t.path().join(n)).collect();
        want.sort();
        assert_eq!(got[0], want);
        assert_eq!(std::fs::read_dir(&queue).unwrap().count(), 0, "佇列應清空");

        // 只有一個：照常使用自己的路徑
        let one = collect_in(&queue, &name, vec![t.path().join("x")], std::time::Duration::from_millis(50));
        assert_eq!(one, Some(vec![t.path().join("x")]));
    }

    /// 實機測試（會改寫真正的剪貼簿，平常不執行）：`cargo test real_clipboard -- --ignored`
    /// 環境變數 ZIPO_CLIP_FILES 為以 | 分隔的檔案路徑；複製到剪貼簿後由 PowerShell 檢查檔案總管看得到，
    /// 也確認 Zipo 讀回同樣的檔案（Ctrl+V 用）
    #[test]
    #[ignore]
    fn real_clipboard_roundtrip() {
        let files: Vec<std::path::PathBuf> = std::env::var("ZIPO_CLIP_FILES").unwrap().split('|').map(Into::into).collect();
        copy_files(&files).unwrap();
        assert_eq!(clipboard_files(), files, "Zipo 讀回剪貼簿上的檔案");
    }

    /// 放到剪貼簿的檔案清單，Windows 自己（DragQueryFileW）讀得出同樣的路徑；不動到真正的剪貼簿
    #[test]
    fn dropfiles_are_readable_by_windows() {
        use windows::Win32::Foundation::GlobalFree;
        use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
        let paths = vec![std::path::PathBuf::from(r"C:\資料\報告.docx"), std::path::PathBuf::from(r"D:\a b\c.txt")];
        let bytes = dropfiles_bytes(&paths);
        // SAFETY: 測試用的記憶體，大小與內容一致，用完釋放
        unsafe {
            let mem = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).unwrap();
            let ptr = GlobalLock(mem) as *mut u8;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            let _ = GlobalUnlock(mem);
            let got = hdrop_paths(windows::Win32::UI::Shell::HDROP(mem.0));
            let _ = GlobalFree(Some(mem));
            assert_eq!(got, paths);
        }
    }

    /// 預設程式註冊同樣寫到測試機碼，驗證後整個刪除。
    #[test]
    fn register_and_unregister_default_in_test_key() {
        let base = format!(r"Software\ZipToolDefaultTest-{}", std::process::id());
        let exe = Path::new(r"C:\Program Files\Zipo\zipo-gui.exe");
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        // 別的程式原有的 OpenWithProgids 不可被移除
        hkcu.create_subkey(format!(r"{base}\Classes\.zip\OpenWithProgids")).unwrap().0.set_value("Other.Zip", &"").unwrap();

        register_default(&base, exe).unwrap();
        assert!(is_default_registered(&base));
        let cmd: String = hkcu.open_subkey(format!(r"{base}\Classes\{PROG_ID}\shell\open\command")).unwrap().get_value("").unwrap();
        assert_eq!(cmd, r#""C:\Program Files\Zipo\zipo-gui.exe" "%1""#);
        let assoc: String = hkcu.open_subkey(format!(r"{base}\{APP_NAME}\Capabilities\FileAssociations")).unwrap().get_value(".zip").unwrap();
        assert_eq!(assoc, PROG_ID);
        let name: String = hkcu.open_subkey(format!(r"{base}\{APP_NAME}\Capabilities")).unwrap().get_value("ApplicationName").unwrap();
        assert_eq!(name, "Zipo");
        let reg: String = hkcu.open_subkey(format!(r"{base}\RegisteredApplications")).unwrap().get_value(APP_NAME).unwrap();
        assert_eq!(reg, r"Software\ZipTool\Capabilities");
        let owp = hkcu.open_subkey(format!(r"{base}\Classes\.7z\OpenWithProgids")).unwrap();
        assert!(owp.get_raw_value(PROG_ID).is_ok());
        // 再註冊一次（例如程式搬了位置）也不出錯
        register_default(&base, exe).unwrap();

        unregister_default(&base).unwrap();
        assert!(!is_default_registered(&base));
        assert!(hkcu.open_subkey(format!(r"{base}\{APP_NAME}")).is_err());
        let zip_owp = hkcu.open_subkey(format!(r"{base}\Classes\.zip\OpenWithProgids")).unwrap();
        assert!(zip_owp.get_raw_value(PROG_ID).is_err());
        assert!(zip_owp.get_raw_value("Other.Zip").is_ok());
        unregister_default(&base).unwrap();

        // 同名機碼下其他程式的設定不可被刪除
        hkcu.create_subkey(format!(r"{base}\{APP_NAME}\Settings")).unwrap().0.set_value("Keep", &"1").unwrap();
        register_default(&base, exe).unwrap();
        unregister_default(&base).unwrap();
        assert!(hkcu.open_subkey(format!(r"{base}\{APP_NAME}\Capabilities")).is_err());
        let kept: String = hkcu.open_subkey(format!(r"{base}\{APP_NAME}\Settings")).unwrap().get_value("Keep").unwrap();
        assert_eq!(kept, "1");
        hkcu.delete_subkey_all(&base).unwrap();
    }
}
