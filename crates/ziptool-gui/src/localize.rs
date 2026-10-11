//! 資料夾與磁碟機的顯示名稱，與檔案總管相同的規則：
//!
//! - 資料夾有「唯讀」或「系統」屬性時，檔案總管才會讀取其中的 `desktop.ini`；
//!   `LocalizedResourceName` 可能是文字，或是 `@%SystemRoot%\system32\shell32.dll,-21798` 這種資源參照。
//! - 資源參照優先從**繁體中文**資源檔（`zh-TW\shell32.dll.mui`）取出，與 Zipo 的介面語言一致；
//!   即使 Windows 顯示語言是英文，也會顯示「下載」「使用者」。沒有中文資源時才用系統語言。
//! - 磁碟機顯示「磁碟標籤 (C:)」，沒有標籤時顯示「本機磁碟 (C:)」。

use std::path::Path;

/// 介面語言；資源參照會先找這個語言的 .mui 檔
const UI_LANGUAGE: &str = "zh-TW";

#[cfg(windows)]
mod win {
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
        fn GetVolumeInformationW(
            root: *const u16,
            name: *mut u16,
            name_len: u32,
            serial: *mut u32,
            max_component: *mut u32,
            flags: *mut u32,
            fs_name: *mut u16,
            fs_len: u32,
        ) -> i32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn LoadStringW(module: *mut c_void, id: u32, buf: *mut u16, max: i32) -> i32;
    }

    const LOAD_LIBRARY_AS_DATAFILE: u32 = 0x2;
    const LOAD_LIBRARY_AS_IMAGE_RESOURCE: u32 = 0x20;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// 從 DLL 或 .mui 檔讀取字串資源（只當成資料載入，不執行其中的程式碼）。
    pub fn load_string(module_path: &Path, id: u32) -> Option<String> {
        if !module_path.is_file() {
            return None;
        }
        let name = wide(module_path.as_os_str());
        // SAFETY: 以資料檔模式載入，只讀取資源；用完立即釋放
        unsafe {
            let h = LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE);
            if h.is_null() {
                return None;
            }
            let mut buf = [0u16; 512];
            let n = LoadStringW(h, id, buf.as_mut_ptr(), buf.len() as i32);
            FreeLibrary(h);
            (n > 0).then(|| String::from_utf16_lossy(&buf[..n as usize]))
        }
    }

    pub fn volume_label(root: &Path) -> Option<String> {
        let r = wide(root.as_os_str());
        let mut name = [0u16; 261];
        // SAFETY: 緩衝區大小正確；不需要的輸出參數傳入 null
        let ok = unsafe {
            GetVolumeInformationW(
                r.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        };
        if ok == 0 {
            return None;
        }
        let len = name.iter().position(|&c| c == 0).unwrap_or(0);
        Some(String::from_utf16_lossy(&name[..len]))
    }

    /// 同一個資源參照（例如 shell32.dll,-21798）只讀一次
    pub fn cache() -> &'static Mutex<HashMap<String, Option<String>>> {
        static C: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
        C.get_or_init(Default::default)
    }
}

/// 展開 `%SystemRoot%` 之類的環境變數
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(a) = rest.find('%') {
        out.push_str(&rest[..a]);
        let after = &rest[a + 1..];
        match after.find('%') {
            Some(b) => {
                let var = &after[..b];
                out.push_str(&std::env::var(var).unwrap_or_else(|_| format!("%{var}%")));
                rest = &after[b + 1..];
            }
            None => {
                out.push_str(&rest[a..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// 解析 desktop.ini 的內容（可能是 UTF-16 或 ANSI），取出 [.ShellClassInfo] 的 LocalizedResourceName。
fn localized_resource_name(ini: &[u8]) -> Option<String> {
    let text = if ini.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = ini[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(ini.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(ini)).into_owned()
    };
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case("[.ShellClassInfo]");
        } else if in_section
            && let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case("LocalizedResourceName")
        {
            let v = v.trim();
            return (!v.is_empty()).then(|| v.to_string());
        }
    }
    None
}

/// 32 位元程式存取 `C:\Windows\System32` 會被 Windows 自動導向 `SysWOW64`（那裡通常沒有中文資源檔），
/// 改用特殊路徑 `Sysnative` 才能讀到真正的 System32。64 位元程式不受影響。
fn real_system_path(path: &Path) -> std::path::PathBuf {
    if cfg!(target_pointer_width = "32")
        && let Ok(root) = std::env::var("SystemRoot")
    {
        let sys32 = Path::new(&root).join("System32");
        let lower = |p: &Path| p.to_string_lossy().to_lowercase();
        if lower(path).starts_with(&lower(&sys32)) {
            let rest = &path.to_string_lossy()[sys32.as_os_str().len()..];
            let native = Path::new(&root).join("Sysnative");
            if native.is_dir() {
                return std::path::PathBuf::from(format!("{}{rest}", native.display()));
            }
        }
    }
    path.to_path_buf()
}

/// 介面語言的資源檔（例如 `System32\zh-TW\shell32.dll.mui`）
fn ui_language_mui(module: &Path) -> Option<std::path::PathBuf> {
    Some(real_system_path(&module.parent()?.join(UI_LANGUAGE).join(format!("{}.mui", module.file_name()?.to_string_lossy()))))
}

/// 把資源參照（`@檔案路徑,-編號`）轉成文字；先找繁體中文的 .mui 檔，再找系統語言。
#[cfg(windows)]
fn resolve_resource(reference: &str) -> Option<String> {
    if let Some(hit) = win::cache().lock().ok().and_then(|c| c.get(reference).cloned()) {
        return hit;
    }
    let result = (|| {
        let (file, id) = reference.strip_prefix('@')?.rsplit_once(',')?;
        let id: u32 = id.trim().trim_start_matches('-').parse().ok()?;
        let module = std::path::PathBuf::from(expand_env(file.trim()));
        let mui = ui_language_mui(&module)?;
        win::load_string(&mui, id).or_else(|| win::load_string(&real_system_path(&module), id))
    })();
    if let Ok(mut c) = win::cache().lock() {
        c.insert(reference.to_string(), result.clone());
    }
    result
}

#[cfg(not(windows))]
fn resolve_resource(_reference: &str) -> Option<String> {
    None
}

/// 資料夾的顯示名稱；`attributes` 為 Windows 檔案屬性（其他平台傳 0）。
/// 先看資料夾裡的 desktop.ini，沒有時再看是不是 Windows 的已知資料夾（文件、下載、桌面…）。
pub fn folder_display_name(dir: &Path, attributes: u32) -> Option<String> {
    const READONLY: u32 = 0x1;
    const SYSTEM: u32 = 0x4;
    if attributes & (READONLY | SYSTEM) != 0
        && let Some(name) = desktop_ini_name(dir)
    {
        return Some(name);
    }
    known_folder_name(dir)
}

fn desktop_ini_name(dir: &Path) -> Option<String> {
    let ini = std::fs::read(dir.join("desktop.ini")).ok()?;
    let value = localized_resource_name(&ini)?;
    if value.starts_with('@') { resolve_resource(&value) } else { Some(value) }
}

/// Windows 已知資料夾：使用者可能搬移過位置，或資料夾裡沒有 desktop.ini（例如「文件」），
/// 所以依登錄檔的實際路徑比對，名稱取自登錄檔記錄的資源。
#[cfg(windows)]
fn known_folder_name(dir: &Path) -> Option<String> {
    use std::sync::OnceLock;
    static KNOWN: OnceLock<Vec<(String, String)>> = OnceLock::new();
    let known = KNOWN.get_or_init(|| {
        use winreg::RegKey;
        use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY};
        // （User Shell Folders 中的值名稱, 已知資料夾 GUID）
        const FOLDERS: [(&str, &str); 6] = [
            ("Desktop", "{B4BFCC3A-DB2C-424C-B029-7FE99A87C641}"),
            ("Personal", "{FDD39AD0-238F-46AF-ADB4-6C85480369C7}"),
            ("{374DE290-123F-4565-9164-39C4925E467B}", "{374DE290-123F-4565-9164-39C4925E467B}"),
            ("My Music", "{4BD8D571-6D19-48D3-BE97-422220080E43}"),
            ("My Pictures", "{33E28130-4E1E-4676-835A-98395C3BC3BB}"),
            ("My Video", "{18989B1D-99B5-455B-841C-AB7C74E4DDFC}"),
        ];
        let Ok(usf) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders") else {
            return Vec::new();
        };
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        FOLDERS
            .iter()
            .filter_map(|(value, guid)| {
                let raw: String = usf.get_value(value).ok()?;
                let path = expand_env(&raw).trim_end_matches(['\\', '/']).to_lowercase();
                // 32 位元程式也要讀 64 位元的登錄檔區段
                let desc = hklm
                    .open_subkey_with_flags(format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\FolderDescriptions\{guid}"), KEY_READ | KEY_WOW64_64KEY)
                    .ok()?;
                let localized: String = desc.get_value("LocalizedName").ok()?;
                Some((path, resolve_resource(&localized)?))
            })
            .collect()
    });
    let key = dir.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    known.iter().find(|(p, _)| *p == key).map(|(_, n)| n.clone())
}

#[cfg(not(windows))]
fn known_folder_name(_dir: &Path) -> Option<String> {
    None
}

/// 磁碟機的顯示名稱，例如「OS (C:)」「本機磁碟 (D:)」。
pub fn drive_display_name(root: &Path) -> String {
    let letter = root.display().to_string().trim_end_matches(['\\', '/']).to_string();
    #[cfg(windows)]
    let label = win::volume_label(root).filter(|l| !l.is_empty());
    #[cfg(not(windows))]
    let label: Option<String> = None;
    format!("{} ({letter})", label.unwrap_or_else(|| "本機磁碟".into()))
}

/// 路徑本身的顯示名稱（給網址列使用）：資料夾有在地化名稱就用它，否則用原本的名稱。
pub fn path_display_name(path: &Path) -> Option<String> {
    #[cfg(windows)]
    let attrs = std::fs::metadata(path).map(|m| std::os::windows::fs::MetadataExt::file_attributes(&m)).unwrap_or(0);
    #[cfg(not(windows))]
    let attrs = 0;
    folder_display_name(path, attrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_desktop_ini() {
        let ini = "[.ShellClassInfo]\r\nLocalizedResourceName=@%SystemRoot%\\system32\\shell32.dll,-21798\r\nIconResource=x\r\n";
        assert_eq!(localized_resource_name(ini.as_bytes()).as_deref(), Some("@%SystemRoot%\\system32\\shell32.dll,-21798"));
        // UTF-16LE（Windows 常見的寫法）
        let mut utf16 = vec![0xFF, 0xFE];
        for u in "[.ShellClassInfo]\r\nLocalizedResourceName=測試名稱\r\n".encode_utf16() {
            utf16.extend(u.to_le_bytes());
        }
        assert_eq!(localized_resource_name(&utf16).as_deref(), Some("測試名稱"));
        // 其他區段的同名設定不算
        assert_eq!(localized_resource_name(b"[Other]\nLocalizedResourceName=x\n"), None);
    }

    #[test]
    fn expands_environment_variables() {
        let root = std::env::var("SystemRoot").unwrap_or_default();
        if !root.is_empty() {
            assert_eq!(expand_env("%SystemRoot%\\a.dll"), format!("{root}\\a.dll"));
        }
        assert_eq!(expand_env("%不存在的變數%\\x"), "%不存在的變數%\\x");
        assert_eq!(expand_env("no vars"), "no vars");
    }

    #[cfg(windows)]
    #[test]
    fn folder_names_follow_explorer_rules() {
        let cmd = |a: &[&str]| std::process::Command::new("cmd").arg("/C").args(a).output().is_ok_and(|o| o.status.success());
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("Downloads");
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("desktop.ini"), "[.ShellClassInfo]\r\nLocalizedResourceName=@%SystemRoot%\\system32\\shell32.dll,-21798\r\n").unwrap();
        // 沒有唯讀或系統屬性：檔案總管不讀 desktop.ini
        assert_eq!(path_display_name(&d), None);
        assert!(cmd(&["attrib", "+r", &d.display().to_string()]));
        // 用與程式相同的方式判斷有沒有中文資源檔（32 位元時要繞過 System32 的重新導向），
        // 有的話就一定要取到「下載」，不可默默略過
        let shell32 = std::path::Path::new(&std::env::var("SystemRoot").unwrap()).join(r"System32\shell32.dll");
        let zh_mui = ui_language_mui(&shell32).unwrap();
        eprintln!("中文資源檔：{}，存在 = {}", zh_mui.display(), zh_mui.is_file());
        if zh_mui.is_file() {
            assert_eq!(path_display_name(&d).as_deref(), Some("下載"), "應取繁體中文資源，不受系統顯示語言影響");
        }
        // 文字型的在地化名稱
        let e = t.path().join("plain");
        std::fs::create_dir(&e).unwrap();
        std::fs::write(e.join("desktop.ini"), "[.ShellClassInfo]\r\nLocalizedResourceName=我的資料\r\n").unwrap();
        assert!(cmd(&["attrib", "+r", &e.display().to_string()]));
        assert_eq!(path_display_name(&e).as_deref(), Some("我的資料"));
        let _ = cmd(&["attrib", "-r", &d.display().to_string()]);
        let _ = cmd(&["attrib", "-r", &e.display().to_string()]);
    }

    /// 已知資料夾（例如沒有 desktop.ini 的「文件」）也要顯示中文
    #[cfg(windows)]
    #[test]
    fn known_folders_are_localized() {
        let storage = std::path::Path::new(&std::env::var("SystemRoot").unwrap()).join(r"System32\windows.storage.dll");
        let zh = ui_language_mui(&storage).is_some_and(|m| m.is_file());
        let docs = std::path::PathBuf::from(std::env::var_os("USERPROFILE").unwrap()).join("Documents");
        eprintln!("中文資源 = {zh}，文件 → {:?}", known_folder_name(&docs));
        if zh && docs.is_dir() {
            assert_eq!(known_folder_name(&docs).as_deref(), Some("文件"));
        }
        // 一般資料夾不受影響
        assert_eq!(known_folder_name(std::path::Path::new(r"C:\沒有這個資料夾")), None);
    }

    #[test]
    fn drive_names() {
        let n = drive_display_name(Path::new("C:\\"));
        assert!(n.ends_with("(C:)"), "{n}");
    }
}
