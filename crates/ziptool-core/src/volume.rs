//! 分割壓縮檔（與 7-Zip 相同）：把壓縮檔依固定大小切成 `名稱.001`、`名稱.002`…，
//! 依序接起來就是原本的壓縮檔。讀取時先接回暫存檔再處理。

use crate::{F, L};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

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

/// `archive` 是分割檔時，接回暫存的完整壓縮檔；否則回傳 `None`
pub(crate) fn join_if_split(archive: &Path) -> Result<Option<Joined>> {
    if !is_first_volume(archive) {
        return Ok(None);
    }
    let parts = volume_paths(archive)?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let dir = std::env::temp_dir().join("Zipo").join(format!("join-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let joined = Joined { path: dir.join(archive.with_extension("").file_name().unwrap_or_default()), dir };
    let mut out = io::BufWriter::new(File::create(&joined.path)?);
    for p in &parts {
        io::copy(&mut File::open(p)?, &mut out)?;
    }
    out.flush()?;
    drop(out);
    // 「從網際網路下載」的標記在第一個分割檔上，一併帶到接回的檔案，解出的檔案才會有標記
    if let Some(z) = crate::util::zone_identifier(archive) {
        crate::util::set_zone_identifier(&joined.path, &z)?;
    }
    Ok(Some(joined))
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

    #[test]
    fn split_and_join() {
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
