//! 分割壓縮檔（與 7-Zip 相同）：把壓縮檔依固定大小切成 `名稱.001`、`名稱.002`…，
//! 依序接起來就是原本的壓縮檔。讀取時先接回暫存檔再處理。

use crate::{F, L};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::{Error, Result};

/// 是否為分割檔的第一個（`名稱.001`）
pub fn is_first_volume(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "001")
}

/// `名稱.壓縮副檔名.NNN`
fn volume_path(first: &Path, n: usize) -> PathBuf {
    let base = first.with_extension("");
    let mut name = base.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{n:03}"));
    first.with_file_name(name)
}

/// 依序列出所有分割檔；中間有缺號時回報缺少哪一個
fn volume_paths(first: &Path) -> Result<Vec<PathBuf>> {
    let mut parts = Vec::new();
    let mut n = 1;
    while volume_path(first, n).is_file() {
        parts.push(volume_path(first, n));
        n += 1;
    }
    if parts.is_empty() {
        return Err(Error::Io(io::Error::new(io::ErrorKind::NotFound, F!("找不到 {}", "Cannot find {}", first.display()))));
    }
    // 後面還有編號較大的分割檔：代表中間缺了一個
    for later in n + 1..n + 1000 {
        if volume_path(first, later).is_file() {
            let missing = volume_path(first, n);
            return Err(Error::Unsupported(F!("缺少分割檔「{}」", "Missing volume \"{}\"", missing.file_name().unwrap_or_default().to_string_lossy())));
        }
    }
    Ok(parts)
}

/// 接回的暫存壓縮檔，用完自動刪除
pub(crate) struct Joined {
    dir: PathBuf,
    pub path: PathBuf,
}

impl Drop for Joined {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 是否保留最近一次的接合結果（圖形介面：瀏覽、預覽、解壓縮同一組分割檔時不必每次重新接合）
static KEEP: AtomicBool = AtomicBool::new(false);

/// 保留的接合結果：第一個分割檔、各分割檔的大小與修改時間
type Stamps = Vec<(u64, Option<SystemTime>)>;
static CACHE: Mutex<Option<(PathBuf, Stamps, Arc<Joined>)>> = Mutex::new(None);

/// 開啟或關閉保留接合結果；關閉時釋放保留的暫存檔（沒有其他地方使用時立即刪除）
pub fn keep_joined(on: bool) {
    KEEP.store(on, Ordering::Relaxed);
    if !on {
        let old = CACHE.lock().unwrap_or_else(|e| e.into_inner()).take();
        drop(old);
    }
}

/// `archive` 是分割檔時，接回暫存的完整壓縮檔；否則回傳 `None`
pub(crate) fn join_if_split(archive: &Path) -> Result<Option<Arc<Joined>>> {
    if !is_first_volume(archive) {
        return Ok(None);
    }
    let parts = volume_paths(archive)?;
    if !KEEP.load(Ordering::Relaxed) {
        return join(archive, &parts).map(|j| Some(Arc::new(j)));
    }
    let stamps: Stamps = parts.iter().map(|p| std::fs::metadata(p).map(|m| (m.len(), m.modified().ok())).unwrap_or_default()).collect();
    // 接合期間持有鎖：同時要用同一組分割檔的其他工作等這次完成後直接沿用
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((first, s, j)) = cache.as_ref()
        && first == archive
        && *s == stamps
    {
        return Ok(Some(j.clone()));
    }
    // 先釋放舊的，暫存空間不會同時被兩份佔用
    *cache = None;
    let j = Arc::new(join(archive, &parts)?);
    *cache = Some((archive.to_path_buf(), stamps, j.clone()));
    Ok(Some(j))
}

fn join(archive: &Path, parts: &[PathBuf]) -> Result<Joined> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    // Windows 的暫存資料夾每位使用者各一個，放在 Zipo 底下以便清理；Unix 的 /tmp 是共用的，
    // 其他使用者可以先建立 /tmp/Zipo 再替換其中的資料夾，所以直接放在 /tmp 並只允許自己存取
    let parent = if cfg!(windows) { std::env::temp_dir().join("Zipo") } else { std::env::temp_dir() };
    std::fs::create_dir_all(&parent)?;
    let dir = parent.join(format!("zipo-join-{}-{stamp}", std::process::id()));
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    // 不用 create_dir_all：資料夾已存在（被搶先建立）時失敗
    builder.create(&dir)?;
    let joined = Joined { path: dir.join(archive.with_extension("").file_name().unwrap_or_default()), dir };
    let mut out = io::BufWriter::new(File::create(&joined.path)?);
    for p in parts {
        io::copy(&mut File::open(p)?, &mut out)?;
    }
    out.flush()?;
    drop(out);
    // 「從網際網路下載」的標記在第一個分割檔上，一併帶到接回的檔案，解出的檔案才會有標記
    if let Some(z) = crate::util::zone_identifier(archive) {
        crate::util::set_zone_identifier(&joined.path, &z)?;
    }
    Ok(joined)
}

/// 把 `path` 切成每個 `size` 位元組的分割檔，完成後刪除原檔；回傳所有分割檔
pub(crate) fn split(path: &Path, size: u64) -> Result<Vec<PathBuf>> {
    if size == 0 {
        return Err(Error::Unsupported(L!("分割大小不可為 0", "The volume size cannot be 0").into()));
    }
    let first = {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".001");
        path.with_file_name(name)
    };
    let mut parts = Vec::new();
    let res = (|| -> Result<()> {
        let mut src = io::BufReader::new(File::open(path)?);
        let total = std::fs::metadata(path)?.len();
        let mut written = 0u64;
        let mut n = 1;
        // 空的壓縮檔也至少產生一個分割檔
        while n == 1 || written < total {
            let part = volume_path(&first, n);
            let mut out = io::BufWriter::new(File::create(&part)?);
            parts.push(part);
            let copied = io::copy(&mut (&mut src).take(size), &mut out)?;
            out.flush()?;
            written += copied;
            n += 1;
            if copied == 0 {
                break;
            }
        }
        Ok(())
    })();
    match res {
        Ok(()) => {
            std::fs::remove_file(path)?;
            Ok(parts)
        }
        Err(e) => {
            for p in &parts {
                let _ = std::fs::remove_file(p);
            }
            Err(e)
        }
    }
}

/// 解析分割大小，例如 `100m`、`700M`、`4g`、`64k`、`1000`（位元組）
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => (&s[..i], &s[i..]),
        None => (s.as_str(), ""),
    };
    let n: u64 = num.parse().ok()?;
    let mul: u64 = match unit {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        _ => return None,
    };
    n.checked_mul(mul).filter(|&v| v > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("100m"), Some(100 << 20));
        assert_eq!(parse_size("4G"), Some(4 << 30));
        assert_eq!(parse_size("64k"), Some(64 << 10));
        assert_eq!(parse_size("1000"), Some(1000));
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size("10x"), None);
        assert_eq!(parse_size(""), None);
    }

    /// 兩個測試都會改變全域的「保留接合結果」設定，依序執行
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 圖形介面保留接合結果：分割檔沒有變動時沿用，不重新接合；有變動或關閉保留時重新接合並刪除舊的
    #[test]
    fn joined_archive_is_reused_until_volumes_change() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("r.zip");
        std::fs::write(&f, vec![7u8; 30_000]).unwrap();
        let parts = split(&f, 10_000).unwrap();
        keep_joined(true);
        let a = join_if_split(&parts[0]).unwrap().unwrap();
        let b = join_if_split(&parts[0]).unwrap().unwrap();
        assert!(std::sync::Arc::ptr_eq(&a, &b), "沒有變動時應沿用");
        let first_dir = a.dir.clone();
        drop((a, b));
        assert!(first_dir.exists(), "保留中不可刪除");
        // 分割檔變動（內容與時間）：重新接合，舊的刪除
        std::fs::write(&parts[2], vec![8u8; 9_000]).unwrap();
        let c = join_if_split(&parts[0]).unwrap().unwrap();
        assert_ne!(c.dir, first_dir);
        assert_eq!(std::fs::metadata(&c.path).unwrap().len(), 29_000);
        assert!(!first_dir.exists(), "舊的接合結果應刪除");
        let dir = c.dir.clone();
        drop(c);
        keep_joined(false);
        assert!(!dir.exists(), "關閉保留後應刪除");
    }

    #[test]
    fn split_and_join() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.zip");
        let data: Vec<u8> = (0..250_000u32).map(|i| (i * 31 % 251) as u8).collect();
        std::fs::write(&f, &data).unwrap();
        let parts = split(&f, 100_000).unwrap();
        let names: Vec<String> = parts.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["a.zip.001", "a.zip.002", "a.zip.003"]);
        assert!(!f.exists(), "原檔應刪除");
        assert_eq!(std::fs::metadata(&parts[2]).unwrap().len(), 50_000);
        let joined = join_if_split(&parts[0]).unwrap().unwrap();
        assert_eq!(joined.path.file_name().unwrap(), "a.zip");
        assert_eq!(std::fs::read(&joined.path).unwrap(), data);
        let dir = joined.dir.clone();
        // Unix：不放在共用的 /tmp/Zipo 底下，且只有自己能存取
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(dir.parent().unwrap(), std::env::temp_dir());
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        drop(joined);
        assert!(!dir.exists(), "暫存檔應刪除");

        // 中間缺一個：說明缺少哪一個
        std::fs::remove_file(&parts[1]).unwrap();
        let e = join_if_split(&parts[0]).err().unwrap().to_string();
        assert!(e.contains("a.zip.002"), "{e}");

        // 不是分割檔：不處理
        assert!(join_if_split(&tmp.path().join("x.zip")).unwrap().is_none());
    }
}
