//! 線上更新：從 GitHub Releases 檢查新版本，下載並驗證 SHA-256 後取代執行檔，再重新啟動。
//!
//! 連線使用系統內建的 curl（Windows 10 起內附，TLS 與 Proxy 設定由 Windows 處理），不另外加入網路函式庫。
//! 執行中的 exe 無法覆寫但可以改名：舊檔改名為 `*.old`、新檔放回原位，下次啟動時再刪除舊檔。

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
    let r: ApiRelease = serde_json::from_slice(json).map_err(|e| format!("無法解讀更新資訊：{e}"))?;
    let version = r.tag_name.trim_start_matches('v').to_string();
    if parse_version(&version).is_none() {
        return Err(format!("版本號碼格式不正確：{}", r.tag_name));
    }
    if !r.html_url.starts_with(PAGE_PREFIX) {
        return Err("更新資訊中的網址不正確".into());
    }
    let want = asset_name(&version);
    let asset = r.assets.into_iter().find(|a| Some(&a.name) == want.as_ref()).and_then(|a| {
        let sha256 = a.digest?.strip_prefix("sha256:")?.to_ascii_lowercase();
        let ok = sha256.len() == 64
            && sha256.bytes().all(|b| b.is_ascii_hexdigit())
            && a.browser_download_url.starts_with(DOWNLOAD_PREFIX)
            && a.size <= MAX_DOWNLOAD;
        ok.then_some(Asset { name: a.name, url: a.browser_download_url, sha256, size: a.size })
    });
    Ok(Release { version, page: r.html_url, asset })
}

/// 呼叫 curl 下載；`out` 為 `None` 時回傳內容
fn curl(url: &str, out: Option<&Path>) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    let exe = Path::new(&std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join(r"System32\curl.exe");
    #[cfg(not(windows))]
    let exe = PathBuf::from("curl");
    let mut cmd = Command::new(exe);
    cmd.args(["--fail", "--silent", "--show-error", "--location"])
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
    let o = cmd.output().map_err(|e| format!("無法執行 curl：{e}"))?;
    if !o.status.success() {
        let why = String::from_utf8_lossy(&o.stderr).trim().to_string();
        return Err(format!("無法連線到 GitHub：{why}"));
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
    let asset = release.asset.as_ref().ok_or("這個版本沒有適用於這台電腦的更新檔，請到下載頁面手動更新")?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let work = std::env::temp_dir().join("Zipo").join(format!("update-{}-{stamp}", std::process::id()));
    let r = (|| {
        std::fs::create_dir_all(&work).map_err(|e| format!("無法建立暫存資料夾：{e}"))?;
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
        return Err(format!("下載的檔案大小不符（{size}，應為 {}）", asset.size));
    }
    let digest = sha256_file(zip).map_err(|e| format!("無法讀取下載的檔案：{e}"))?;
    if digest != asset.sha256 {
        return Err("下載的檔案驗證失敗（SHA-256 不符），可能已損毀或遭竄改，已取消更新".into());
    }
    ziptool_core::extract(zip, unpack, &Default::default(), &ziptool_core::NoProgress).map_err(|e| format!("無法解開更新檔：{e}"))?;
    let gui = find_file(unpack, "zipo-gui.exe").ok_or("更新檔中找不到 zipo-gui.exe")?;
    let dir = current_exe.parent().ok_or("找不到程式所在的資料夾")?;
    replace_file(&gui, current_exe)?;
    // 同一個資料夾中的命令列版本一併更新
    let cli_target = dir.join("zipo.exe");
    if cli_target.is_file()
        && let Some(cli) = find_file(unpack, "zipo.exe")
    {
        replace_file(&cli, &cli_target)?;
    }
    Ok(current_exe.to_path_buf())
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
            format!("沒有權限更新「{}」，請到下載頁面手動更新", target.parent().unwrap_or(target).display())
        } else {
            format!("無法更新「{}」：{e}", target.display())
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
        // 說明頁面不在本儲存庫：整個回應不採用
        assert!(parse_release(&api_json(name, &url, &format!("sha256:{sha}"), "https://evil.example/")).is_err());
        assert!(parse_release(b"not json").is_err());
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
