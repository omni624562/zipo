//! 檔案總管右鍵選單整合（Windows）。
//!
//! 只寫入目前使用者的設定（`HKCU\Software\Classes`），不需要系統管理員權限，
//! 也不會更改壓縮檔的預設開啟程式；隨時可以從「工具」選單移除。
//! 預設程式則另外註冊（`register_default`），最後由使用者在 Windows 設定中選取。

#![cfg_attr(not(windows), allow(dead_code))]

use ziptool_core::L;
use std::path::Path;

/// 實際使用的位置；測試時改用其他機碼，不影響真實設定。
pub const ROOT: &str = r"Software\Classes";

/// 右鍵選單要出現的壓縮檔副檔名
pub const ARCHIVE_EXTS: [&str; 11] = [".zip", ".7z", ".tar", ".gz", ".tgz", ".bz2", ".tbz2", ".xz", ".txz", ".zst", ".tzst"];

/// （機碼路徑, 顯示文字, 命令列參數）
fn verbs(exe: &Path) -> Vec<(String, &'static str, String)> {
    let exe = exe.display();
    let mut v = vec![
        (r"*\shell\ZipTool.Add".to_string(), L!("加入壓縮檔（Zipo）…", "Add to archive (Zipo)…"), format!("\"{exe}\" --add \"%1\"")),
        (r"Directory\shell\ZipTool.Add".to_string(), L!("加入壓縮檔（Zipo）…", "Add to archive (Zipo)…"), format!("\"{exe}\" --add \"%1\"")),
    ];
    for ext in ARCHIVE_EXTS {
        let base = format!(r"SystemFileAssociations\{ext}\shell");
        v.push((format!(r"{base}\ZipTool.Open"), L!("用 Zipo 開啟", "Open with Zipo"), format!("\"{exe}\" \"%1\"")));
        v.push((format!(r"{base}\ZipTool.ExtractHere"), L!("解壓縮到這裡（Zipo）", "Extract here (Zipo)"), format!("\"{exe}\" --extract-here \"%1\"")));
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
        let label: String = hkcu.open_subkey(format!(r"{root}\Directory\shell\ZipTool.Add")).unwrap().get_value("MUIVerb").unwrap();
        assert_eq!(label, "加入壓縮檔（Zipo）…");

        uninstall(&root).unwrap();
        assert!(!is_installed(&root));
        assert!(hkcu.open_subkey(format!(r"{root}\SystemFileAssociations\.zip\shell\ZipTool.Open")).is_err());
        // 再移除一次也不出錯
        uninstall(&root).unwrap();
        hkcu.delete_subkey_all(&root).unwrap();
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
