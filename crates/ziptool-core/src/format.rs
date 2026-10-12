use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Zip,
    SevenZ,
    Tar,
    TarGz,
    TarBz2,
    TarXz,
    TarZst,
    Gz,
    Bz2,
    Xz,
    Zst,
    /// ISO 映像檔（只能讀取，不在 `ALL` 中）
    Iso,
}

impl Format {
    /// 可以建立的壓縮格式（ISO 只能讀取，不包含在內）
    pub const ALL: [Format; 11] = [
        Format::Zip,
        Format::SevenZ,
        Format::Tar,
        Format::TarGz,
        Format::TarBz2,
        Format::TarXz,
        Format::TarZst,
        Format::Gz,
        Format::Bz2,
        Format::Xz,
        Format::Zst,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Format::Zip => "zip",
            Format::SevenZ => "7z",
            Format::Tar => "tar",
            Format::TarGz => "tar.gz",
            Format::TarBz2 => "tar.bz2",
            Format::TarXz => "tar.xz",
            Format::TarZst => "tar.zst",
            Format::Gz => "gz",
            Format::Bz2 => "bz2",
            Format::Xz => "xz",
            Format::Zst => "zst",
            Format::Iso => "iso",
        }
    }

    /// 由名稱解析，例如 "zip"、"tar.gz"、"tgz"。
    pub fn from_name(s: &str) -> Option<Format> {
        Self::from_file_name(&format!("x.{}", s.trim_start_matches('.')))
    }

    /// 由檔名的副檔名判斷格式。
    pub fn from_file_name(name: &str) -> Option<Format> {
        Self::split_extension(name).map(|(_, f)| f)
    }

    /// 拆出「去掉壓縮副檔名後的檔名」與格式，例如 `a.tar.gz` → (`a`, TarGz)。
    pub fn split_extension(name: &str) -> Option<(&str, Format)> {
        // 分割檔的第一個（例如 a.7z.001）視為該格式
        if let Some(first) = name.strip_suffix(".001") {
            return Self::split_extension(first);
        }
        let n = name.to_ascii_lowercase();
        let table: [(&str, Format); 18] = [
            (".tar.gz", Format::TarGz),
            (".tgz", Format::TarGz),
            (".tar.bz2", Format::TarBz2),
            (".tbz2", Format::TarBz2),
            (".tbz", Format::TarBz2),
            (".tar.xz", Format::TarXz),
            (".txz", Format::TarXz),
            (".tar.zst", Format::TarZst),
            (".tzst", Format::TarZst),
            (".tar", Format::Tar),
            (".zip", Format::Zip),
            (".7z", Format::SevenZ),
            (".gz", Format::Gz),
            (".bz2", Format::Bz2),
            (".xz", Format::Xz),
            (".zst", Format::Zst),
            (".zstd", Format::Zst),
            (".iso", Format::Iso),
        ];
        table
            .iter()
            .find(|(ext, _)| n.ends_with(ext))
            .map(|(ext, f)| (&name[..name.len() - ext.len()], *f))
    }

    /// 先看副檔名，無法判斷時再讀檔頭 magic bytes。
    pub fn detect(path: &Path) -> Result<Format> {
        if let Some(f) = path.file_name().and_then(|n| n.to_str()).and_then(Self::from_file_name) {
            return Ok(f);
        }
        let mut buf = [0u8; 512];
        let mut file = File::open(path)?;
        let n = read_up_to(&mut file, &mut buf)?;
        let b = &buf[..n];
        let f = if b.starts_with(b"PK\x03\x04") || b.starts_with(b"PK\x05\x06") {
            Format::Zip
        } else if b.starts_with(b"7z\xBC\xAF\x27\x1C") {
            Format::SevenZ
        } else if b.starts_with(&[0x1F, 0x8B]) {
            Format::Gz
        } else if b.starts_with(b"BZh") {
            Format::Bz2
        } else if b.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
            Format::Xz
        } else if b.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
            Format::Zst
        } else if n >= 262 && &b[257..262] == b"ustar" {
            Format::Tar
        } else if is_iso(&mut file)? {
            Format::Iso
        } else {
            return Err(Error::UnknownFormat(path.display().to_string()));
        };
        Ok(f)
    }

    pub fn is_tar(self) -> bool {
        matches!(
            self,
            Format::Tar | Format::TarGz | Format::TarBz2 | Format::TarXz | Format::TarZst
        )
    }

    /// 單檔壓縮格式（gz/bz2/xz/zst）：只能壓縮一個檔案。
    pub fn is_single(self) -> bool {
        matches!(self, Format::Gz | Format::Bz2 | Format::Xz | Format::Zst)
    }

    /// 只能讀取（無法建立或修改）
    pub fn is_read_only(self) -> bool {
        self == Format::Iso
    }

    pub fn supports_password(self) -> bool {
        matches!(self, Format::Zip | Format::SevenZ)
    }

    /// 副檔名（不含前導點）。
    pub fn extension(self) -> &'static str {
        self.name()
    }
}

/// ISO 9660：第 16 個磁區（0x8000）開始的卷描述元以「CD001」標示
fn is_iso(file: &mut File) -> std::io::Result<bool> {
    use std::io::{Seek, SeekFrom};
    let mut buf = [0u8; 6];
    if file.seek(SeekFrom::Start(0x8000)).is_err() {
        return Ok(false);
    }
    Ok(read_up_to(file, &mut buf)? == 6 && &buf[1..6] == b"CD001")
}

fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}
