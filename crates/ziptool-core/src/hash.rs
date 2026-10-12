//! 檔案雜湊值：讀取一次，同時計算 CRC32、MD5、SHA-1、SHA-256，用來核對下載的檔案是否正確。

use std::io::Read;
use std::path::PathBuf;

use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::{Error, Progress, Result};

/// 一個檔案的雜湊值（小寫十六進位）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashes {
    pub size: u64,
    pub crc32: String,
    pub md5: String,
    pub sha1: String,
    pub sha256: String,
}

impl Hashes {
    /// 依序列出（演算法名稱, 值）
    pub fn all(&self) -> [(&'static str, &str); 4] {
        [("CRC32", &self.crc32), ("MD5", &self.md5), ("SHA-1", &self.sha1), ("SHA-256", &self.sha256)]
    }

    /// `expected` 符合哪一種雜湊值（忽略大小寫、前後空白與常見的 `sha256:` 前綴）
    pub fn matches(&self, expected: &str) -> Option<&'static str> {
        let e = expected.trim().to_ascii_lowercase();
        let e = e.rsplit_once(':').map(|(_, v)| v.trim()).unwrap_or(&e);
        if e.is_empty() {
            return None;
        }
        self.all().into_iter().find(|(_, v)| *v == e).map(|(name, _)| name)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 讀完 `r` 並計算雜湊值；每讀一段就回報進度，取消時中止
pub fn hash_reader(r: &mut dyn Read, progress: &dyn Progress) -> Result<Hashes> {
    let (mut crc, mut md5, mut sha1, mut sha256) = (crc32fast::Hasher::new(), Md5::new(), Sha1::new(), Sha256::new());
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        if progress.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        let chunk = &buf[..n];
        crc.update(chunk);
        md5.update(chunk);
        sha1.update(chunk);
        sha256.update(chunk);
        size += n as u64;
        progress.advance(n as u64);
    }
    Ok(Hashes {
        size,
        crc32: format!("{:08x}", crc.finalize()),
        md5: hex(&md5.finalize()),
        sha1: hex(&sha1.finalize()),
        sha256: hex(&sha256.finalize()),
    })
}

/// 計算檔案（資料夾則為其中所有檔案）的雜湊值；回傳（顯示名稱, 雜湊值）
pub fn hash_files(inputs: &[PathBuf], progress: &dyn Progress) -> Result<Vec<(String, Hashes)>> {
    let files: Vec<_> = crate::util::collect(inputs)?.into_iter().filter(|i| !i.is_dir).collect();
    progress.start(crate::util::total_size(&files));
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        progress.entry(&f.name);
        let mut file = std::fs::File::open(&f.path)?;
        out.push((f.name, hash_reader(&mut file, progress)?));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoProgress;

    /// 以標準測試向量（"abc"）驗證四種演算法
    #[test]
    fn known_vectors() {
        let h = hash_reader(&mut &b"abc"[..], &NoProgress).unwrap();
        assert_eq!(h.size, 3);
        assert_eq!(h.crc32, "352441c2");
        assert_eq!(h.md5, "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(h.sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(h.sha256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let empty = hash_reader(&mut &b""[..], &NoProgress).unwrap();
        assert_eq!(empty.sha256, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn compares_expected_values() {
        let h = hash_reader(&mut &b"abc"[..], &NoProgress).unwrap();
        assert_eq!(h.matches("  BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD \n"), Some("SHA-256"));
        assert_eq!(h.matches("sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"), Some("SHA-256"));
        assert_eq!(h.matches("900150983cd24fb0d6963f7d28e17f72"), Some("MD5"));
        assert_eq!(h.matches("352441C2"), Some("CRC32"));
        assert_eq!(h.matches("deadbeef"), None);
        assert_eq!(h.matches(""), None);
    }

    #[test]
    fn hashes_folders_recursively() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("d");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("a.txt"), "abc").unwrap();
        std::fs::write(d.join("sub/b.txt"), "").unwrap();
        let r = hash_files(&[d], &NoProgress).unwrap();
        let names: Vec<&str> = r.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["d/a.txt", "d/sub/b.txt"]);
        assert_eq!(r[0].1.md5, "900150983cd24fb0d6963f7d28e17f72");
    }
}
