//! 線上更新：從 GitHub Releases 檢查新版本，下載並驗證 SHA-256 後取代執行檔，再重新啟動。
//!
//! 連線使用系統內建的 curl（Windows 10 起內附，TLS 與 Proxy 設定由 Windows 處理），不另外加入網路函式庫。
//! 執行中的 exe 無法覆寫但可以改名：舊檔改名為 `*.old`、新檔放回原位，下次啟動時再刪除舊檔。

use ziptool_core::{F, L};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const API: &str = "https://api.github.com/repos/omni624562/zipo/releases/latest";
/// 下載網址與說明頁面只接受這個儲存庫底下的位置
const DOWNLOAD_PREFIX: &str = "https://github.com/omni624562/zipo/releases/download/";
const PAGE_PREFIX: &str = "https://github.com/omni624562/zipo/";
/// 更新檔大小上限，避免異常的回應塞爆磁碟
const MAX_DOWNLOAD: u64 = 200 << 20;
/// 自動檢查的間隔
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// 最新版本的資訊
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    /// 不含開頭的 `v`，例如 `0.2.1`
    pub version: String,
    /// 版本說明頁面
    pub page: String,
    /// 適用於這台電腦的更新檔；沒有時（例如 32 位元版）只能手動下載
    pub asset: Option<Asset>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    /// 小寫十六進位
    pub sha256: String,
    pub size: u64,
}

#[derive(serde::Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(serde::Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

/// `0.2.1`（可帶 `v` 開頭）→ (0, 2, 1)
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut it = s.trim().trim_start_matches('v').split('.').map(|p| p.parse::<u64>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// `latest` 是否比 `current` 新
pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse_version(latest), parse_version(current)), (Some(l), Some(c)) if l > c)
}

/// 這台電腦適用的更新檔名稱；目前只提供 64 位元 Windows 版
fn asset_name(version: &str) -> Option<String> {
    cfg!(all(windows, target_pointer_width = "64")).then(|| format!("zipo-{version}-windows-x64.zip"))
}

/// 解讀 GitHub API 的回應
pub fn parse_release(json: &[u8]) -> Result<Release, String> {
    let r: ApiRelease = serde_json::from_slice(json).map_err(|e| F!("無法解讀更新資訊，請稍後再試（{e}）", "Cannot read the update information; try again later ({e})"))?;
    let version = r.tag_name.trim_start_matches('v').to_string();
    if parse_version(&version).is_none() {
        return Err(F!("版本號碼格式不正確（{}），請稍後再試", "Invalid version number ({}); try again later", r.tag_name));
    }
    if !url_within(&r.html_url, PAGE_PREFIX) {
        return Err(L!("更新資訊中的網址不正確，請稍後再試", "The update information contains an invalid address; try again later").into());
    }
    let want = asset_name(&version);
    let asset = r.assets.into_iter().find(|a| Some(&a.name) == want.as_ref()).and_then(|a| {
        let sha256 = a.digest?.strip_prefix("sha256:")?.to_ascii_lowercase();
        let ok = sha256.len() == 64
            && sha256.bytes().all(|b| b.is_ascii_hexdigit())
            && url_within(&a.browser_download_url, DOWNLOAD_PREFIX)
            && a.size <= MAX_DOWNLOAD;
        ok.then_some(Asset { name: a.name, url: a.browser_download_url, sha256, size: a.size })
    });
    Ok(Release { version, page: r.html_url, asset })
}

/// 網址在 `prefix` 之下，且不含會被 curl 或伺服器解讀成其他位置的字元（../、{}、[]、%、反斜線、空白）
fn url_within(url: &str, prefix: &str) -> bool {
    url.strip_prefix(prefix).is_some_and(|rest| !rest.contains("..") && !rest.contains(['{', '}', '[', ']', '%', '\\']) && !rest.contains(char::is_whitespace))
}

/// 呼叫 curl 下載；`out` 為 `None` 時回傳內容
fn curl(url: &str, out: Option<&Path>) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    let exe = Path::new(&std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join(r"System32\curl.exe");
    #[cfg(not(windows))]
    let exe = PathBuf::from("curl");
    let mut cmd = Command::new(exe);
    // --globoff：網址中的 {} [] 不展開
    cmd.args(["--fail", "--silent", "--show-error", "--location", "--globoff"])
        // 只允許 HTTPS，轉址也一樣
        .args(["--proto", "=https", "--proto-redir", "=https", "--tlsv1.2"])
        .args(["--connect-timeout", "15", "--max-time", "900"])
        .args(["--max-filesize", &MAX_DOWNLOAD.to_string()])
        .args(["-H", "Accept: application/vnd.github+json", "-H", &format!("User-Agent: Zipo/{}", env!("CARGO_PKG_VERSION"))]);
    if let Some(out) = out {
        cmd.arg("--output").arg(out);
    }
    cmd.arg(url);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let o = cmd.output().map_err(|e| F!("無法執行 curl：{e}", "Cannot run curl: {e}"))?;
    if !o.status.success() {
        let why = String::from_utf8_lossy(&o.stderr).trim().to_string();
        return Err(F!("無法連線到 GitHub，請確認網路連線後再試。（{why}）", "Cannot connect to GitHub; check your internet connection and try again. ({why})"));
    }
    Ok(o.stdout)
}

/// 檢查最新版本
pub fn check() -> Result<Release, String> {
    parse_release(&curl(API, None)?)
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// 下載、驗證並安裝更新；成功時回傳要重新啟動的執行檔
pub fn download_and_install(release: &Release, current_exe: &Path) -> Result<PathBuf, String> {
    let asset = release.asset.as_ref().ok_or(L!("這個版本沒有適用於這台電腦的更新檔，請到下載頁面手動更新", "This version has no update package for this computer; please update manually from the download page"))?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let work = std::env::temp_dir().join("Zipo").join(format!("update-{}-{stamp}", std::process::id()));
    let r = (|| {
        std::fs::create_dir_all(&work).map_err(|e| F!("無法建立暫存資料夾：{e}。請確認磁碟還有空間。", "Cannot create a temporary folder: {e}. Check that the disk has free space."))?;
        let zip = work.join(&asset.name);
        curl(&asset.url, Some(&zip))?;
        install_from_zip(&zip, asset, &work.join("x"), current_exe)
    })();
    let _ = std::fs::remove_dir_all(&work);
    r
}

/// 驗證下載的檔案後解壓縮並取代執行檔
fn install_from_zip(zip: &Path, asset: &Asset, unpack: &Path, current_exe: &Path) -> Result<PathBuf, String> {
    let size = std::fs::metadata(zip).map(|m| m.len()).unwrap_or(0);
    if size != asset.size {
        return Err(F!("下載的檔案大小不符（{size}，應為 {}）", "The downloaded file has the wrong size ({size}, expected {})", asset.size));
    }
    let digest = sha256_file(zip).map_err(|e| F!("無法讀取下載的檔案：{e}。請再試一次。", "Cannot read the downloaded file: {e}. Please try again."))?;
    if digest != asset.sha256 {
        return Err(L!("下載的檔案驗證失敗（SHA-256 不符），可能已損毀或遭竄改，已取消更新。請稍後再試，或到下載頁面手動下載。", "The downloaded file failed verification (SHA-256 mismatch); it may be damaged or tampered with. The update was cancelled. Try again later, or download it manually from the download page.").into());
    }
    ziptool_core::extract(zip, unpack, &Default::default(), &ziptool_core::NoProgress).map_err(|e| F!("無法解開更新檔：{e}。請再試一次，或到下載頁面手動更新。", "Cannot unpack the update: {e}. Try again, or update manually from the download page."))?;
    let gui = find_file(unpack, "zipo-gui.exe").ok_or(L!("更新檔中找不到 zipo-gui.exe", "zipo-gui.exe was not found in the update"))?;
    let dir = current_exe.parent().ok_or(L!("找不到程式所在的資料夾，請到下載頁面手動更新", "Cannot find the program's folder; update manually from the download page"))?;
    let mut pairs = vec![(gui, current_exe.to_path_buf())];
    // 同一個資料夾中的命令列版本一併更新
    let cli_target = dir.join("zipo.exe");
    if cli_target.is_file()
        && let Some(cli) = find_file(unpack, "zipo.exe")
    {
        pairs.push((cli, cli_target));
    }
    replace_all(&pairs)?;
    Ok(current_exe.to_path_buf())
}

/// 依序以 `(新檔, 目標)` 取代執行檔，全部成功或全部不變：任何一個失敗時，已換掉的全部還原，
/// 不會只更新圖形介面、命令列仍是舊版。
fn replace_all(pairs: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    // 先清掉上次更新留下的舊執行檔。刪不掉通常是上次更新前開啟的 Zipo 還在執行（Windows 無法刪除執行中的程式），
    // 這時取代一定失敗，所以在動到任何檔案之前就停下
    for (_, target) in pairs {
        let old = old_path(target);
        if let Err(e) = std::fs::remove_file(&old)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(F!(
                "無法刪除上次更新留下的「{}」：可能有上次更新前開啟的 Zipo 還在執行，請關閉所有 Zipo 視窗後再更新；或沒有權限，請到下載頁面手動更新。",
                "Cannot delete \"{}\" left over from the last update: a Zipo window opened before the last update may still be running, so close all Zipo windows and update again; or you lack permission, so update manually from the download page.",
                old.display()
            ));
        }
    }
    for (i, (new, target)) in pairs.iter().enumerate() {
        if let Err(e) = replace_file(new, target) {
            for (_, done) in pairs[..i].iter().rev() {
                let _ = std::fs::remove_file(done);
                let _ = std::fs::rename(old_path(done), done);
            }
            return Err(e);
        }
    }
    Ok(())
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if e.file_name().to_string_lossy().eq_ignore_ascii_case(name) {
                return Some(p);
            }
        }
    }
    None
}

fn old_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    target.with_file_name(name)
}

/// 以 `new` 取代 `target`：執行中的 exe 只能改名，所以先把舊檔改名為 `*.old` 再放入新檔；失敗時還原
fn replace_file(new: &Path, target: &Path) -> Result<(), String> {
    let old = old_path(target);
    let _ = std::fs::remove_file(&old);
    let denied = |e: std::io::Error| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            F!("沒有權限更新「{}」，請到下載頁面手動更新", "No permission to update \"{}\"; please update manually from the download page", target.parent().unwrap_or(target).display())
        } else {
            F!("無法更新「{}」：{e}。請關閉其他開啟中的 Zipo 後再試，或到下載頁面手動更新。", "Cannot update \"{}\": {e}. Close any other open Zipo windows and try again, or update manually from the download page.", target.display())
        }
    };
    std::fs::rename(target, &old).map_err(denied)?;
    if let Err(e) = std::fs::copy(new, target) {
        let _ = std::fs::remove_file(target);
        let _ = std::fs::rename(&old, target);
        return Err(denied(e));
    }
    Ok(())
}

/// 刪除上次更新留下的舊執行檔（啟動時呼叫；仍在執行時刪除會失敗，下次再試）
pub fn cleanup_old(current_exe: &Path) {
    let _ = std::fs::remove_file(old_path(current_exe));
    if let Some(dir) = current_exe.parent() {
        let _ = std::fs::remove_file(old_path(&dir.join("zipo.exe")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.2.1"), Some((0, 2, 1)));
        assert_eq!(parse_version("1.10.0"), Some((1, 10, 0)));
        assert_eq!(parse_version("0.2"), None);
        assert_eq!(parse_version("0.2.1.4"), None);
        assert_eq!(parse_version("0.2.x"), None);
        assert!(is_newer("0.2.2", "0.2.1"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(!is_newer("0.2.1", "0.2.1"));
        assert!(!is_newer("0.2.0", "0.2.1"));
        assert!(!is_newer("壞掉", "0.2.1"));
    }

    fn api_json(name: &str, url: &str, digest: &str, page: &str) -> Vec<u8> {
        format!(
            r#"{{"tag_name":"v9.8.7","html_url":"{page}","assets":[{{"name":"{name}","browser_download_url":"{url}","size":123,"digest":"{digest}"}}]}}"#
        )
        .into_bytes()
    }

    #[test]
    fn parses_github_response() {
        let sha = "ED0E0D208AB3E0B30756B73A9B9A5D824ED0A1C305F2F5B63EFBD9F176CE5279";
        let name = "zipo-9.8.7-windows-x64.zip";
        let url = format!("{DOWNLOAD_PREFIX}v9.8.7/{name}");
        let page = "https://github.com/omni624562/zipo/releases/tag/v9.8.7";
        let r = parse_release(&api_json(name, &url, &format!("sha256:{sha}"), page)).unwrap();
        assert_eq!(r.version, "9.8.7");
        assert_eq!(r.page, page);
        if cfg!(all(windows, target_pointer_width = "64")) {
            let a = r.asset.unwrap();
            assert_eq!((a.url.as_str(), a.sha256.as_str(), a.size), (url.as_str(), sha.to_ascii_lowercase().as_str(), 123));
        } else {
            assert_eq!(r.asset, None);
        }

        // 不在本儲存庫的下載網址、缺少或格式錯誤的 SHA-256：不提供自動更新
        for (u, d) in [("https://evil.example/x.zip".to_string(), format!("sha256:{sha}")), (url.clone(), String::new()), (url.clone(), "sha256:1234".into())] {
            let r = parse_release(&api_json(name, &u, &d, page)).unwrap();
            assert_eq!(r.asset, None, "{u} {d}");
        }
        // 前綴相同、但用 ../、{}、[]、%2e 或反斜線跳到其他位置的網址也不採用
        for tail in ["../../../../other/repo/releases/download/v9.8.7/x.zip", "v9.8.7/{a,b}.zip", "v9.8.7/[1-2].zip", "%2e%2e/%2e%2e/x.zip", "v9.8.7/a b.zip"] {
            let u = format!("{DOWNLOAD_PREFIX}{tail}");
            let r = parse_release(&api_json(name, &u, &format!("sha256:{sha}"), page)).unwrap();
            assert_eq!(r.asset, None, "{u}");
        }
        assert!(parse_release(&api_json(name, &url, &format!("sha256:{sha}"), "https://github.com/omni624562/zipo/../../evil/x")).is_err());
        // 說明頁面不在本儲存庫：整個回應不採用
        assert!(parse_release(&api_json(name, &url, &format!("sha256:{sha}"), "https://evil.example/")).is_err());
        assert!(parse_release(b"not json").is_err());
    }

    /// 上次更新留下的舊執行檔刪不掉（上次更新前開啟的 Zipo 還在執行）時，什麼都不換，並說明原因；
    /// 不可只換掉圖形介面、命令列維持舊版
    #[test]
    fn install_refuses_while_old_exe_is_in_use() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir(&app).unwrap();
        let (gui, cli) = (app.join("zipo-gui.exe"), app.join("zipo.exe"));
        std::fs::write(&gui, "舊版圖形介面").unwrap();
        std::fs::write(&cli, "舊版命令列").unwrap();
        // 刪不掉的 zipo.exe.old（以非空資料夾模擬執行中的舊檔）
        std::fs::create_dir(old_path(&cli)).unwrap();
        std::fs::write(old_path(&cli).join("x"), "").unwrap();
        let (new_gui, new_cli) = (tmp.path().join("new-gui"), tmp.path().join("new-cli"));
        std::fs::write(&new_gui, "新版圖形介面").unwrap();
        std::fs::write(&new_cli, "新版命令列").unwrap();

        let e = replace_all(&[(new_gui, gui.clone()), (new_cli, cli.clone())]).unwrap_err();
        // 要指出是哪個舊檔刪不掉（否則 Windows 上只會看到誤導的「沒有權限」）
        assert!(e.contains(&old_path(&cli).display().to_string()), "{e}");
        assert_eq!(std::fs::read_to_string(&gui).unwrap(), "舊版圖形介面", "圖形介面不可先被換掉");
        assert_eq!(std::fs::read_to_string(&cli).unwrap(), "舊版命令列");
    }

    /// 換到一半失敗（例如命令列複製失敗）時，已換掉的圖形介面要還原
    #[test]
    fn install_rolls_back_when_a_later_file_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let (gui, cli) = (tmp.path().join("zipo-gui.exe"), tmp.path().join("zipo.exe"));
        std::fs::write(&gui, "舊版圖形介面").unwrap();
        std::fs::write(&cli, "舊版命令列").unwrap();
        let new_gui = tmp.path().join("new-gui");
        std::fs::write(&new_gui, "新版圖形介面").unwrap();
        let missing = tmp.path().join("不存在的新版命令列");

        replace_all(&[(new_gui, gui.clone()), (missing, cli.clone())]).unwrap_err();
        assert_eq!(std::fs::read_to_string(&gui).unwrap(), "舊版圖形介面", "已換掉的要還原");
        assert_eq!(std::fs::read_to_string(&cli).unwrap(), "舊版命令列");
        assert!(!old_path(&gui).exists() && !old_path(&cli).exists(), "不留下舊檔");
    }

    /// 驗證與取代：SHA-256 不符時不動到任何檔案；正確時取代執行檔並保留舊檔，可清除
    #[test]
    fn install_verifies_and_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir(&app).unwrap();
        let exe = app.join("zipo-gui.exe");
        std::fs::write(&exe, "舊版圖形介面").unwrap();
        std::fs::write(app.join("zipo.exe"), "舊版命令列").unwrap();

        // 做一個與正式版相同結構的更新檔
        let src = tmp.path().join("zipo-9.8.7-windows-x64");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("zipo-gui.exe"), "新版圖形介面").unwrap();
        std::fs::write(src.join("zipo.exe"), "新版命令列").unwrap();
        let zip = tmp.path().join("u.zip");
        ziptool_core::compress(&[src], &zip, ziptool_core::Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();
        let size = std::fs::metadata(&zip).unwrap().len();
        let good = Asset { name: "u.zip".into(), url: String::new(), sha256: sha256_file(&zip).unwrap(), size };

        let bad = Asset { sha256: "0".repeat(64), ..good.clone() };
        let e = install_from_zip(&zip, &bad, &tmp.path().join("x1"), &exe).unwrap_err();
        assert!(e.contains("SHA-256"), "{e}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "舊版圖形介面");

        let launched = install_from_zip(&zip, &good, &tmp.path().join("x2"), &exe).unwrap();
        assert_eq!(launched, exe);
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "新版圖形介面");
        assert_eq!(std::fs::read_to_string(app.join("zipo.exe")).unwrap(), "新版命令列");
        assert_eq!(std::fs::read_to_string(old_path(&exe)).unwrap(), "舊版圖形介面");

        cleanup_old(&exe);
        assert!(!old_path(&exe).exists() && !old_path(&app.join("zipo.exe")).exists());
    }
}
