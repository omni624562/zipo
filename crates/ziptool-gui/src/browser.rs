//! 檔案瀏覽：本機磁碟、資料夾，以及壓縮檔內的虛擬資料夾。

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ziptool_core::{Entry, Format};

#[derive(Clone, Debug, PartialEq)]
pub enum Location {
    /// 磁碟清單（Windows 的「電腦」）
    Computer,
    Dir(PathBuf),
    /// `inner` 為壓縮檔內目前所在的資料夾，根目錄為空字串
    Archive { path: PathBuf, inner: String },
}

impl Location {
    pub fn display(&self) -> String {
        match self {
            Location::Computer => "電腦".into(),
            Location::Dir(p) => p.display().to_string(),
            Location::Archive { path, inner } if inner.is_empty() => format!("{}{}", path.display(), std::path::MAIN_SEPARATOR),
            Location::Archive { path, inner } => {
                let sep = std::path::MAIN_SEPARATOR.to_string();
                format!("{}{sep}{}{sep}", path.display(), inner.replace('/', &sep))
            }
        }
    }

    pub fn parent(&self) -> Option<Location> {
        match self {
            Location::Computer => None,
            Location::Dir(p) => match p.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => Some(Location::Dir(parent.to_path_buf())),
                _ => Some(Location::Computer),
            },
            Location::Archive { path, inner } if inner.is_empty() => {
                Some(Location::Dir(path.parent().map(Path::to_path_buf).unwrap_or_default()))
            }
            Location::Archive { path, inner } => Some(Location::Archive {
                path: path.clone(),
                inner: inner.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default(),
            }),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    /// 實際的檔名（用來組路徑）
    pub name: String,
    /// 顯示用的名稱：資料夾的在地化名稱（例如 Downloads → 下載）、磁碟機的「標籤 (C:)」；其餘與 `name` 相同
    pub display: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub packed: Option<u64>,
    pub modified: Option<SystemTime>,
    pub encrypted: bool,
    /// 本機上可辨識的壓縮檔
    pub is_archive: bool,
    pub is_drive: bool,
}

impl Row {
    pub fn type_label(&self) -> String {
        if self.is_drive {
            return "本機磁碟".into();
        }
        if self.is_dir {
            return "資料夾".into();
        }
        if let Some(f) = Format::from_file_name(&self.name) {
            return format!("{} 壓縮檔", f.name());
        }
        match self.name.rsplit_once('.') {
            Some((_, ext)) if !ext.is_empty() => format!("{} 檔案", ext.to_ascii_uppercase()),
            _ => "檔案".into(),
        }
    }
}

#[cfg_attr(not(windows), allow(clippy::vec_init_then_push))]
pub fn list_drives() -> Vec<Row> {
    let mut rows = Vec::new();
    #[cfg(windows)]
    for letter in b'C'..=b'Z' {
        let root = format!("{}:\\", letter as char);
        if Path::new(&root).exists() {
            rows.push(Row {
                display: crate::localize::drive_display_name(Path::new(&root)),
                name: root,
                is_dir: true,
                size: None,
                packed: None,
                modified: None,
                encrypted: false,
                is_archive: false,
                is_drive: true,
            });
        }
    }
    #[cfg(not(windows))]
    rows.push(Row {
        display: "/".into(),
        name: "/".into(),
        is_dir: true,
        size: None,
        packed: None,
        modified: None,
        encrypted: false,
        is_archive: false,
        is_drive: true,
    });
    rows
}

/// 列出資料夾內容。`show_system` 為假時，與檔案總管相同，不顯示同時具有「隱藏＋系統」屬性的項目
/// （例如家目錄中為了相容舊程式而存在的 My Documents、Application Data 等接合點）。
pub fn list_dir(dir: &Path, show_system: bool) -> std::io::Result<Vec<Row>> {
    let mut rows = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let Ok(e) = e else { continue };
        // 這是連結本身的資訊（不跟隨連結）
        let Ok(link_meta) = e.metadata() else { continue };
        if !show_system && is_protected_system(&link_meta) {
            continue;
        }
        // 符號連結與目錄接合點：依指向的目標判斷；目標讀不到時，看連結本身是否帶有「目錄」屬性
        let target = link_meta.file_type().is_symlink().then(|| std::fs::metadata(e.path()).ok()).flatten();
        let meta = target.as_ref().unwrap_or(&link_meta);
        let is_dir = meta.is_dir() || (target.is_none() && has_directory_attr(&link_meta));
        let name = e.file_name().to_string_lossy().into_owned();
        let display = if is_dir {
            crate::localize::folder_display_name(&e.path(), attribute_bits(&link_meta)).unwrap_or_else(|| name.clone())
        } else {
            name.clone()
        };
        rows.push(Row {
            display,
            is_archive: !is_dir && Format::from_file_name(&name).is_some(),
            name,
            is_dir,
            size: (!is_dir).then_some(meta.len()),
            packed: None,
            modified: meta.modified().ok(),
            encrypted: false,
            is_drive: false,
        });
    }
    Ok(rows)
}

#[cfg(windows)]
fn attributes(m: &std::fs::Metadata) -> u32 {
    std::os::windows::fs::MetadataExt::file_attributes(m)
}

fn attribute_bits(m: &std::fs::Metadata) -> u32 {
    #[cfg(windows)]
    return attributes(m);
    #[cfg(not(windows))]
    {
        let _ = m;
        0
    }
}

#[cfg(windows)]
fn is_protected_system(m: &std::fs::Metadata) -> bool {
    const HIDDEN: u32 = 0x2;
    const SYSTEM: u32 = 0x4;
    attributes(m) & (HIDDEN | SYSTEM) == HIDDEN | SYSTEM
}

#[cfg(windows)]
fn has_directory_attr(m: &std::fs::Metadata) -> bool {
    attributes(m) & 0x10 != 0
}

#[cfg(not(windows))]
fn is_protected_system(_m: &std::fs::Metadata) -> bool {
    false
}

#[cfg(not(windows))]
fn has_directory_attr(_m: &std::fs::Metadata) -> bool {
    false
}

/// 取得壓縮檔內 `inner` 資料夾的直接子項目；資料夾的大小為其內容的總和。
pub fn archive_rows(entries: &[Entry], inner: &str) -> Vec<Row> {
    let prefix = if inner.is_empty() { String::new() } else { format!("{inner}/") };
    let mut map: BTreeMap<String, Row> = BTreeMap::new();
    for e in entries {
        let Some(rest) = e.name.strip_prefix(&prefix) else { continue };
        if rest.is_empty() {
            continue;
        }
        let (first, deeper) = match rest.split_once('/') {
            Some((f, _)) => (f, true),
            None => (rest, false),
        };
        let is_dir = deeper || e.is_dir;
        let row = map.entry(first.to_string()).or_insert_with(|| Row {
            name: first.to_string(),
            display: first.to_string(),
            is_dir,
            size: if is_dir { Some(0) } else { None },
            packed: if is_dir { Some(0) } else { None },
            modified: None,
            encrypted: false,
            is_archive: false,
            is_drive: false,
        });
        if is_dir && !row.is_dir {
            // 先前以檔案（例如 tar 中的連結）出現，後面又有子項目：其實是資料夾
            *row = Row { is_dir: true, size: Some(0), packed: Some(0), encrypted: false, ..row.clone() };
        } else if row.is_dir && !is_dir {
            // 已知是資料夾，同名的非資料夾項目不覆蓋它
            continue;
        }
        if deeper || !e.is_dir {
            if is_dir {
                row.size = Some(row.size.unwrap_or(0) + e.size.unwrap_or(0));
                row.packed = match (row.packed, e.compressed_size) {
                    (Some(a), Some(b)) => Some(a + b),
                    _ => None,
                };
                row.encrypted |= e.encrypted;
            } else {
                row.size = e.size;
                row.packed = e.compressed_size;
                row.encrypted = e.encrypted;
            }
        }
        if !deeper {
            row.modified = e.modified;
        }
    }
    map.into_values().collect()
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SortKey {
    Name,
    Size,
    Packed,
    Modified,
    Type,
}

/// 資料夾永遠排在檔案前面；名稱比較不分大小寫。
pub fn sort(rows: &mut [Row], key: SortKey, ascending: bool) {
    rows.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            let o = match key {
                SortKey::Name => Ordering::Equal,
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Packed => a.packed.cmp(&b.packed),
                SortKey::Modified => a.modified.cmp(&b.modified),
                SortKey::Type => a.type_label().cmp(&b.type_label()),
            }
            .then_with(|| a.display.to_lowercase().cmp(&b.display.to_lowercase()));
            if ascending { o } else { o.reverse() }
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str, is_dir: bool, size: u64) -> Entry {
        Entry { name: name.into(), is_dir, size: Some(size), compressed_size: Some(size / 2), modified: None, encrypted: false }
    }

    #[test]
    fn virtual_folders() {
        let entries = vec![e("a", true, 0), e("a/x.txt", false, 10), e("a/b/y.txt", false, 30), e("z.txt", false, 5), e("implied/q", false, 1)];
        let root = archive_rows(&entries, "");
        let names: Vec<_> = root.iter().map(|r| (r.name.as_str(), r.is_dir, r.size)).collect();
        assert_eq!(names, [("a", true, Some(40)), ("implied", true, Some(1)), ("z.txt", false, Some(5))]);

        let a = archive_rows(&entries, "a");
        let names: Vec<_> = a.iter().map(|r| (r.name.as_str(), r.is_dir, r.size)).collect();
        assert_eq!(names, [("b", true, Some(30)), ("x.txt", false, Some(10))]);
    }

    #[test]
    fn entry_later_seen_as_folder() {
        let entries = vec![e("lnk", false, 5), e("lnk/evil.txt", false, 5), e("ok.txt", false, 2)];
        let root = archive_rows(&entries, "");
        let names: Vec<_> = root.iter().map(|r| (r.name.as_str(), r.is_dir, r.size)).collect();
        assert_eq!(names, [("lnk", true, Some(5)), ("ok.txt", false, Some(2))]);
    }

    #[test]
    fn parents() {
        let a = Location::Archive { path: PathBuf::from("/d/x.7z"), inner: "a/b".into() };
        assert_eq!(a.parent(), Some(Location::Archive { path: PathBuf::from("/d/x.7z"), inner: "a".into() }));
        let root = Location::Archive { path: PathBuf::from("/d/x.7z"), inner: String::new() };
        assert_eq!(root.parent(), Some(Location::Dir(PathBuf::from("/d"))));
    }
}
