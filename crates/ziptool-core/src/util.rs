use crate::{F, L};
use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::SystemTime;

use crate::{Error, ExtractOptions, ExtractReport, Issue, Overwrite, OverwriteAnswer, Progress, Result};

/// 要放進壓縮檔的一個項目。
pub(crate) struct Item {
    pub path: PathBuf,
    /// 壓縮檔內的名稱，以 `/` 分隔。
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// 展開輸入清單：資料夾會遞迴加入，名稱以「輸入項目本身」為根。
/// 例如輸入 `C:\a\docs`，裡面的檔案會是 `docs/x.txt`。
/// `filter` 不符合的項目不加入；被排除的資料夾不再往下找
pub(crate) fn collect(inputs: &[PathBuf], filter: &crate::Filter) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    for input in inputs {
        let meta = std::fs::metadata(input)?;
        let base = input.parent().unwrap_or(Path::new(""));
        if meta.is_dir() {
            let walk = walkdir::WalkDir::new(input).sort_by_file_name().into_iter();
            for e in walk.filter_entry(|e| !filter.excludes(&to_archive_name(e.path().strip_prefix(base).unwrap_or(e.path())))) {
                let e = e.map_err(|e| Error::Io(e.into()))?;
                let rel = e.path().strip_prefix(base).unwrap_or(e.path());
                if !filter.allows(&to_archive_name(rel)) {
                    continue;
                }
                let md = e.metadata().map_err(|e| Error::Io(e.into()))?;
                items.push(Item {
                    path: e.path().to_path_buf(),
                    name: to_archive_name(rel),
                    is_dir: md.is_dir(),
                    size: if md.is_file() { md.len() } else { 0 },
                    modified: md.modified().ok(),
                });
            }
        } else {
            let name = input
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or_else(|| Error::Unsupported(F!("無效的輸入路徑：{}", "Invalid input path: {}", input.display())))?;
            if !filter.allows(&name) {
                continue;
            }
            items.push(Item {
                path: input.clone(),
                name,
                is_dir: false,
                size: meta.len(),
                modified: meta.modified().ok(),
            });
        }
    }
    Ok(items)
}

fn to_archive_name(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// 把 `dir` 底下的所有內容列成項目（名稱為相對於 `dir` 的路徑），略過 `skip` 為真的項目。
pub(crate) fn collect_tree(dir: &Path, skip: impl Fn(&str) -> bool) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    for e in walkdir::WalkDir::new(dir).min_depth(1).sort_by_file_name() {
        let e = e.map_err(|e| Error::Io(e.into()))?;
        let name = to_archive_name(e.path().strip_prefix(dir).unwrap_or(e.path()));
        if skip(&name) {
            continue;
        }
        let md = e.metadata().map_err(|e| Error::Io(e.into()))?;
        items.push(Item {
            path: e.path().to_path_buf(),
            name,
            is_dir: md.is_dir(),
            size: if md.is_file() { md.len() } else { 0 },
            modified: md.modified().ok(),
        });
    }
    Ok(items)
}

pub(crate) fn total_size(items: &[Item]) -> u64 {
    items.iter().map(|i| i.size).sum()
}

/// 統一壓縮檔內的名稱：`\` 轉 `/`，去掉開頭的 `./` 與結尾的 `/`。
pub(crate) fn normalize_name(name: &str) -> String {
    let n = name.replace('\\', "/");
    let n = n.trim_start_matches("./").trim_matches('/');
    n.to_string()
}

/// 把 `name` 的每一段清理成本機可用的檔名後接到 `dest` 之後。
/// 含 `..` 的名稱一律拒絕（回傳 `None`）；開頭的 `/` 或磁碟代號會被當成一般資料夾名稱處理，
/// 因此結果永遠位於 `dest` 之內（防 Zip-Slip）。
pub(crate) fn safe_join(dest: &Path, name: &str) -> Option<PathBuf> {
    let mut out = dest.to_path_buf();
    let mut pushed = false;
    for c in name.split(['/', '\\']) {
        match c {
            "" | "." => {}
            ".." => return None,
            c => {
                out.push(sanitize_component(c));
                pushed = true;
            }
        }
    }
    pushed.then_some(out)
}

/// Windows 不允許或有特殊意義的檔名：`<>:"|?*`、控制字元、結尾的空白或句點、
/// 以及 CON、NUL、COM1 等保留名稱。不處理的話，`a.txt:x` 會寫進 NTFS 替代資料流（隱藏資料），
/// 保留名稱則會寫到裝置。做法與 7-Zip 相同：換成 `_`。
#[cfg(windows)]
pub(crate) fn sanitize_component(c: &str) -> String {
    let mut s: String = c.chars().map(|ch| if (ch as u32) < 0x20 || "<>:\"|?*".contains(ch) { '_' } else { ch }).collect();
    let trimmed = s.trim_end_matches([' ', '.']).len();
    if trimmed != s.len() {
        s.truncate(trimmed);
        s.push('_');
    }
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
        "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = s.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        s.insert(0, '_');
    }
    s
}

#[cfg(not(windows))]
pub(crate) fn sanitize_component(c: &str) -> String {
    c.replace('\0', "_")
}

/// 寫入中的暫存檔名：完成後才改名為正式名稱。
pub(crate) fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".ziptool-part");
    path.with_file_name(name)
}

/// 解壓縮用的暫存檔：名稱加上隨機部分，並以「必須是新檔案」的方式建立。
/// 固定的名稱會與壓縮檔內的同名項目或資料夾中原有的檔案相撞（覆寫它們），
/// 若該名稱是預先放好的符號連結，還會被導到解壓縮資料夾之外。
fn create_part(target: &Path) -> io::Result<(PathBuf, File)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    for _ in 0..100 {
        let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let tag = (u64::from(std::process::id()) << 32 | u64::from(nanos)) ^ SEQ.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        // 名稱不含原檔名：原檔名接近 255 字元上限時，加長的暫存檔名會超過上限
        let part = target.with_file_name(format!("{tag:016x}.ziptool-part"));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&part) {
            Ok(f) => return Ok((part, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::from(io::ErrorKind::AlreadyExists))
}

/// 人類可讀的大小，例如 `1.5 GB`。
pub fn format_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

/// 常見 I/O 錯誤的中文說明。
pub fn describe(e: &io::Error) -> String {
    use io::ErrorKind::*;
    if e.raw_os_error() == Some(206) {
        return L!("路徑太長", "The path is too long").into();
    }
    match e.kind() {
        PermissionDenied => L!("權限不足（檔案可能被其他程式使用中，或為唯讀）。請關閉使用中的程式，或改存到其他資料夾。", "Access denied (the file may be in use by another program, or read-only). Close the program using it, or save to another folder.").into(),
        NotFound => L!("找不到檔案或路徑，可能已被移動或刪除", "File or path not found; it may have been moved or deleted").into(),
        InvalidFilename => L!("檔名無效或路徑太長", "Invalid file name or path too long").into(),
        StorageFull => L!("磁碟空間不足，請清出空間後再試", "Not enough disk space; free up space and try again").into(),
        IsADirectory => L!("已有同名的資料夾", "A folder with the same name already exists").into(),
        AlreadyExists => L!("檔案已存在", "The file already exists").into(),
        InvalidData => F!("壓縮資料損壞或格式錯誤（{e}）。檔案可能沒有下載完整，請重新下載。", "Compressed data is damaged or invalid ({e}). It may not have downloaded completely; try downloading it again."),
        UnexpectedEof => L!("檔案不完整，可能下載或複製時中斷", "The file is incomplete; the download or copy may have been interrupted").into(),
        _ => e.to_string(),
    }
}

fn drain(r: &mut dyn Read) -> Result<()> {
    io::copy(r, &mut io::sink())?;
    Ok(())
}

/// 依解壓選項決定每個項目要不要解、寫到哪裡、遇到同名檔怎麼辦，並收集個別項目的問題。
///
/// 只在單一執行緒中使用（各格式依序處理項目），所以用 `Cell`／`RefCell` 保存狀態。
pub(crate) struct Planner<'a> {
    pub dest: &'a Path,
    opts: &'a ExtractOptions,
    progress: &'a dyn Progress,
    selection: Option<Vec<String>>,
    strip: String,
    mode: Cell<Overwrite>,
    report: RefCell<ExtractReport>,
    /// 最多可寫入的位元組數（目的磁碟的可用空間），防止串流型壓縮炸彈塞爆磁碟
    limit: Option<u64>,
    written: Cell<u64>,
    /// 壓縮檔大小與「大量」的門檻：實際寫出超過門檻且達壓縮檔的 [`crate::BOMB_RATIO`] 倍時視為壓縮炸彈。
    /// 宣告的大小可以造假，tar／gz 等格式也沒有清單可預先檢查，所以解壓時要即時判斷。
    packed: u64,
    bomb_min: u64,
    /// 壓縮檔的「網路下載」標記（Mark-of-the-Web），會一併加到解出的檔案上
    #[cfg_attr(not(windows), allow(dead_code))]
    motw: Option<Vec<u8>>,
}

impl<'a> Planner<'a> {
    pub fn new(archive: &Path, dest: &'a Path, opts: &'a ExtractOptions, progress: &'a dyn Progress, limit: Option<u64>) -> Self {
        let selection = opts.selection.as_ref().map(|s| s.iter().map(|n| normalize_name(n)).collect());
        Self {
            dest,
            opts,
            progress,
            selection,
            strip: normalize_name(&opts.strip_prefix),
            mode: Cell::new(opts.overwrite),
            report: RefCell::default(),
            limit,
            written: Cell::new(0),
            packed: std::fs::metadata(archive).map(|m| m.len()).unwrap_or(0),
            bomb_min: crate::BOMB_MIN_SIZE,
            motw: read_motw(archive),
        }
    }

    pub fn finish(self) -> ExtractReport {
        self.report.into_inner()
    }

    pub fn issue(&self, name: &str, reason: impl Into<String>) {
        self.report.borrow_mut().issues.push(Issue { name: name.to_string(), reason: reason.into() });
    }

    pub fn selected(&self, name: &str) -> bool {
        let name = normalize_name(name);
        (match &self.selection {
            None => true,
            Some(sel) => sel.iter().any(|s| name == *s || name.starts_with(&format!("{s}/"))),
        }) && self.opts.filter.allows(&name)
    }

    /// 回傳輸出路徑；未選取、不需輸出（例如 flatten 時的資料夾）或名稱不安全（記錄後略過）時回傳 `None`。
    pub fn path_for(&self, name: &str, is_dir: bool) -> Option<PathBuf> {
        if !self.selected(name) {
            return None;
        }
        let norm = normalize_name(name);
        let mut rel = norm.as_str();
        if !self.strip.is_empty() {
            rel = rel.strip_prefix(&self.strip)?.strip_prefix('/')?;
        }
        if self.opts.flatten {
            if is_dir {
                return None;
            }
            rel = rel.rsplit('/').next().unwrap_or(rel);
        }
        if rel.is_empty() {
            return None;
        }
        let out = safe_join(self.dest, rel);
        if out.is_none() {
            self.issue(name, L!("路徑含有「..」，可能試圖寫到目標資料夾之外，已略過", "The path contains \"..\" and might write outside the destination folder; skipped"));
        }
        out
    }

    /// 檢查路徑沒有經過目標資料夾內的符號連結（否則可能被導向資料夾之外），並建立資料夾。
    fn prepare_dir(&self, dir: &Path) -> std::result::Result<(), String> {
        for anc in dir.ancestors().take_while(|a| a.starts_with(self.dest) && *a != self.dest) {
            if std::fs::symlink_metadata(anc).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(L!("路徑經過符號連結，可能被導向目標資料夾之外，已略過", "The path goes through a symbolic link and might be redirected outside the destination folder; skipped").into());
            }
        }
        std::fs::create_dir_all(dir).map_err(|e| describe(&e))
    }

    pub fn make_dir(&self, name: &str, path: &Path) {
        if let Err(reason) = self.prepare_dir(path) {
            self.issue(name, reason);
        }
    }

    /// 決定實際寫入的位置；`Ok(None)` 代表略過此檔。
    fn resolve_target(&self, name: &str, path: &Path) -> Result<Option<PathBuf>> {
        if let Some(parent) = path.parent()
            && let Err(reason) = self.prepare_dir(parent)
        {
            self.issue(name, reason);
            return Ok(None);
        }
        let Ok(meta) = std::fs::symlink_metadata(path) else { return Ok(Some(path.to_path_buf())) };
        if meta.is_dir() {
            self.issue(name, L!("已有同名的資料夾，已略過", "A folder with the same name already exists; skipped"));
            return Ok(None);
        }
        let mode = match self.mode.get() {
            Overwrite::Ask => match self.progress.ask_overwrite(path) {
                OverwriteAnswer::Yes => Overwrite::Overwrite,
                OverwriteAnswer::No => Overwrite::Skip,
                OverwriteAnswer::YesAll => self.sticky(Overwrite::Overwrite),
                OverwriteAnswer::NoAll => self.sticky(Overwrite::Skip),
                OverwriteAnswer::RenameAll => self.sticky(Overwrite::Rename),
                OverwriteAnswer::Cancel => return Err(Error::Cancelled),
            },
            m => m,
        };
        Ok(match mode {
            Overwrite::Skip => {
                self.report.borrow_mut().skipped += 1;
                None
            }
            Overwrite::Rename => Some(unique_name(path)),
            _ => Some(path.to_path_buf()),
        })
    }

    fn sticky(&self, m: Overwrite) -> Overwrite {
        self.mode.set(m);
        m
    }

    /// 把 `r` 的內容寫到 `path`：先寫暫存檔，完成後才改名，取消或失敗都不會留下半個檔案。
    ///
    /// 讀取錯誤（壓縮檔損壞、密碼錯誤、取消）回傳 `Err` 中止整批；
    /// 寫入錯誤（權限、路徑）記錄後略過此檔，並把剩下的資料讀完，避免後續項目錯位。
    pub fn write_file(&self, name: &str, path: &Path, r: &mut dyn Read, mtime: Option<SystemTime>, unix_mode: Option<u32>) -> Result<()> {
        let Some(target) = self.resolve_target(name, path)? else { return drain(r) };
        let (part, mut file) = match create_part(&target) {
            Ok(x) => x,
            Err(e) => {
                self.issue(name, describe(&e));
                return drain(r);
            }
        };
        let mut buf = vec![0u8; 256 * 1024];
        let mut write_err: Option<io::Error> = None;
        let mut size = 0u64;
        loop {
            let n = match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    drop(file);
                    let _ = std::fs::remove_file(&part);
                    return Err(e.into());
                }
            };
            if write_err.is_some() {
                continue;
            }
            let total = self.written.get() + n as u64;
            if let Some(limit) = self.limit
                && total > limit
            {
                drop(file);
                let _ = std::fs::remove_file(&part);
                return Err(Error::NotEnoughSpace { need: total, available: limit });
            }
            if let Some(max) = self.opts.max_bytes
                && total > max
            {
                drop(file);
                let _ = std::fs::remove_file(&part);
                return Err(Error::TooLarge { limit: max });
            }
            if !self.opts.allow_high_ratio && total >= self.bomb_min && total / self.packed.max(1) >= crate::BOMB_RATIO {
                drop(file);
                let _ = std::fs::remove_file(&part);
                return Err(Error::PossibleBomb { size: total, packed: self.packed });
            }
            self.written.set(total);
            size += n as u64;
            if let Err(e) = file.write_all(&buf[..n]) {
                if e.kind() == io::ErrorKind::StorageFull {
                    drop(file);
                    let _ = std::fs::remove_file(&part);
                    return Err(e.into());
                }
                write_err = Some(e);
            }
        }
        if let Some(e) = write_err {
            drop(file);
            let _ = std::fs::remove_file(&part);
            self.issue(name, describe(&e));
            return Ok(());
        }
        if let Some(t) = mtime {
            let _ = file.set_modified(t);
        }
        #[cfg(unix)]
        if let Some(mode) = unix_mode {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(mode & 0o777));
        }
        #[cfg(not(unix))]
        let _ = unix_mode;
        drop(file);
        if let Err(e) = std::fs::rename(&part, &target) {
            let _ = std::fs::remove_file(&part);
            self.issue(name, describe(&e));
            return Ok(());
        }
        self.mark_of_the_web(&target);
        let mut rep = self.report.borrow_mut();
        rep.files += 1;
        rep.bytes += size;
        Ok(())
    }

    #[cfg(windows)]
    fn mark_of_the_web(&self, target: &Path) {
        if let Some(z) = &self.motw {
            let mut ads = target.as_os_str().to_os_string();
            ads.push(":Zone.Identifier");
            let _ = std::fs::write(ads, z);
        }
    }

    #[cfg(not(windows))]
    fn mark_of_the_web(&self, _target: &Path) {}
}

/// 檔案的 Zone.Identifier（Mark-of-the-Web）原始內容；用於更新壓縮檔後原樣寫回。
#[cfg(windows)]
pub(crate) fn zone_identifier(path: &Path) -> Option<Vec<u8>> {
    let mut ads = path.as_os_str().to_os_string();
    ads.push(":Zone.Identifier");
    std::fs::read(ads).ok()
}

#[cfg(windows)]
pub(crate) fn set_zone_identifier(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut ads = path.as_os_str().to_os_string();
    ads.push(":Zone.Identifier");
    std::fs::write(ads, data)
}

#[cfg(not(windows))]
pub(crate) fn zone_identifier(_path: &Path) -> Option<Vec<u8>> {
    None
}

#[cfg(not(windows))]
pub(crate) fn set_zone_identifier(_path: &Path, _data: &[u8]) -> std::io::Result<()> {
    Ok(())
}

/// 讀取壓縮檔的 Zone.Identifier；只有來自網際網路（3）或受限制網站（4）時才傳遞。
#[cfg(windows)]
fn read_motw(archive: &Path) -> Option<Vec<u8>> {
    let mut ads = archive.as_os_str().to_os_string();
    ads.push(":Zone.Identifier");
    let data = std::fs::read(ads).ok()?;
    let text = String::from_utf8_lossy(&data);
    text.lines().any(|l| matches!(l.trim(), "ZoneId=3" | "ZoneId=4")).then_some(data)
}

#[cfg(not(windows))]
fn read_motw(_archive: &Path) -> Option<Vec<u8>> {
    None
}

fn unique_name(path: &Path) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (1..)
        .map(|i| path.with_file_name(format!("{stem} ({i}){ext}")))
        .find(|p| std::fs::symlink_metadata(p).is_err())
        .unwrap_or_else(|| path.to_path_buf())
}

/// ZIP 檔名字碼頁；0 代表自動（Windows 的系統 ANSI 字碼頁，台灣為 950／Big5）。
static ZIP_CODEPAGE: AtomicU32 = AtomicU32::new(0);

/// 指定舊式 zip（沒有 UTF-8 標記）的檔名字碼頁，例如 950（Big5）、936（GBK）、932（Shift_JIS）；
/// `None` 為自動。對應 7-Zip 的 `-mcp`。
pub fn set_zip_codepage(cp: Option<u32>) {
    ZIP_CODEPAGE.store(cp.unwrap_or(0), Ordering::Relaxed);
}

pub fn zip_codepage() -> Option<u32> {
    match ZIP_CODEPAGE.load(Ordering::Relaxed) {
        0 => None,
        c => Some(c),
    }
}

fn encoding_for(cp: u32) -> Option<&'static encoding_rs::Encoding> {
    use encoding_rs::*;
    Some(match cp {
        950 => BIG5,
        936 => GBK,
        54936 => GB18030,
        932 => SHIFT_JIS,
        949 => EUC_KR,
        866 => IBM866,
        874 => WINDOWS_874,
        1250 => WINDOWS_1250,
        1251 => WINDOWS_1251,
        1252 => WINDOWS_1252,
        1253 => WINDOWS_1253,
        1254 => WINDOWS_1254,
        1255 => WINDOWS_1255,
        1256 => WINDOWS_1256,
        1257 => WINDOWS_1257,
        1258 => WINDOWS_1258,
        65001 => UTF_8,
        _ => return None,
    })
}

#[cfg(windows)]
fn system_codepage() -> u32 {
    unsafe extern "system" {
        fn GetACP() -> u32;
    }
    // SAFETY: GetACP 沒有參數、只回傳目前的 ANSI 字碼頁
    unsafe { GetACP() }
}

#[cfg(not(windows))]
fn system_codepage() -> u32 {
    0
}

/// 合法的 UTF-8 直接使用；否則依字碼頁解碼，解不出來時沿用 zip 套件的 CP437 結果。
pub(crate) fn decode_zip_name(raw: &[u8], fallback: &str) -> String {
    if let Ok(s) = std::str::from_utf8(raw) {
        return s.to_string();
    }
    let cp = zip_codepage().unwrap_or_else(system_codepage);
    if let Some(enc) = encoding_for(cp) {
        let (s, _, had_errors) = enc.decode(raw);
        if !had_errors {
            return s.into_owned();
        }
    }
    fallback.to_string()
}

/// 壓縮時可使用的記憶體預算：實體記憶體的一半，但不超過目前可用的記憶體；
/// 32 位元程式最多只能定址約 4 GB，再限制在 1.5 GB。
/// 高壓縮等級的 LZMA 每個執行緒要將近 1 GB，只依 CPU 核心數開執行緒會耗盡記憶體而當掉。
pub(crate) fn memory_budget() -> u64 {
    let (total, avail) = physical_memory().unwrap_or((4 << 30, 4 << 30));
    budget_from(total, avail, cfg!(target_pointer_width = "32"))
}

/// 其他程式佔用很多記憶體時，依總量開執行緒會配置失敗而當掉，所以以可用的為上限；
/// 至少保留 256 MB，記憶體很少時仍能以單執行緒進行
fn budget_from(total: u64, avail: u64, is_32bit: bool) -> u64 {
    let budget = (total / 2).min(avail).max(256 << 20);
    if is_32bit { budget.min(1536 << 20) } else { budget }
}

/// 剩餘預算可容納的執行緒數：至少 1 個（預算用完也要能進行）、最多 `cores` 個
fn plan_threads(budget: u64, used: u64, per_thread: u64, cores: u64) -> u64 {
    (budget.saturating_sub(used) / per_thread.max(1)).clamp(1, cores.max(1))
}

/// 同一個程式內同時進行的工作已保留的記憶體。各工作共用 [`memory_budget`]，
/// 否則每個工作都以為可以用掉全部預算，同時壓縮多個檔案時（32 位元程式尤其明顯）會耗盡記憶體。
static RESERVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 保留的記憶體；丟棄時歸還
#[must_use]
pub(crate) struct MemoryReservation(u64);

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        RESERVED.fetch_sub(self.0, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 依「尚未被其他工作保留」的記憶體預算決定執行緒數（每個執行緒約需 `per_thread` 位元組），
/// 至少 1 個、最多 CPU 核心數，並保留對應的記憶體直到回傳的保留物被丟棄。
pub(crate) fn reserve_threads(per_thread: u64) -> (u32, MemoryReservation) {
    use std::sync::atomic::Ordering;
    let cores = std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1);
    let per = per_thread.max(1);
    let budget = memory_budget();
    let mut used = RESERVED.load(Ordering::SeqCst);
    loop {
        let threads = plan_threads(budget, used, per, cores);
        let amount = threads.saturating_mul(per);
        match RESERVED.compare_exchange(used, used.saturating_add(amount), Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return (threads as u32, MemoryReservation(amount)),
            Err(now) => used = now,
        }
    }
}

/// 實體記憶體的總量與目前可用的量
#[cfg(windows)]
fn physical_memory() -> Option<(u64, u64)> {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(buf: *mut MemoryStatusEx) -> i32;
    }
    let mut m = MemoryStatusEx {
        length: std::mem::size_of::<MemoryStatusEx>() as u32,
        memory_load: 0,
        total_phys: 0,
        avail_phys: 0,
        total_page_file: 0,
        avail_page_file: 0,
        total_virtual: 0,
        avail_virtual: 0,
        avail_extended_virtual: 0,
    };
    // SAFETY: 傳入大小正確、已設定 length 的結構
    (unsafe { GlobalMemoryStatusEx(&mut m) } != 0).then_some((m.total_phys, m.avail_phys))
}

#[cfg(target_os = "linux")]
fn physical_memory() -> Option<(u64, u64)> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let read = |key: &str| -> Option<u64> { Some(info.lines().find(|l| l.starts_with(key))?.split_whitespace().nth(1)?.parse::<u64>().ok()? * 1024) };
    Some((read("MemTotal:")?, read("MemAvailable:")?))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn physical_memory() -> Option<(u64, u64)> {
    None
}

fn local_offset() -> time::UtcOffset {
    time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC)
}

/// 轉成本地時間；超出可表示範圍（例如損壞檔案中的異常時間戳）時回傳 `None`，不會 panic。
pub fn local_time(t: SystemTime) -> Option<time::OffsetDateTime> {
    let secs = match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).ok()?,
        Err(e) => -i64::try_from(e.duration().as_secs()).ok()?,
    };
    Some(time::OffsetDateTime::from_unix_timestamp(secs).ok()?.to_offset(local_offset()))
}

/// ZIP 時間以本地時間儲存（與 7-Zip、Windows 檔案總管一致）。
pub(crate) fn to_zip_time(t: Option<SystemTime>) -> Option<zip::DateTime> {
    let odt = local_time(t?)?;
    zip::DateTime::try_from(time::PrimitiveDateTime::new(odt.date(), odt.time())).ok()
}

pub(crate) fn from_zip_time(d: Option<zip::DateTime>) -> Option<SystemTime> {
    let p = time::PrimitiveDateTime::try_from(d?).ok()?;
    Some(p.assume_offset(local_offset()).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoProgress;

    #[test]
    fn write_limit_stops_stream_bombs() {
        let tmp = tempfile::tempdir().unwrap();
        let opts = ExtractOptions::default();
        let plan = Planner::new(&tmp.path().join("x.gz"), tmp.path(), &opts, &NoProgress, Some(1000));
        let mut data = &vec![0u8; 5000][..];
        let r = plan.write_file("big.bin", &tmp.path().join("big.bin"), &mut data, None, None);
        assert!(matches!(r, Err(Error::NotEnoughSpace { .. })), "{r:?}");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "不應留下任何檔案");
    }

    /// 呼叫端指定的寫入上限（預覽、拖曳）：依實際寫出的量判斷，不採信壓縮檔宣告的大小
    #[test]
    fn max_bytes_stops_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let opts = ExtractOptions { max_bytes: Some(1000), ..Default::default() };
        let plan = Planner::new(&tmp.path().join("x.gz"), tmp.path(), &opts, &NoProgress, None);
        let mut data = &vec![0u8; 5000][..];
        let r = plan.write_file("big.bin", &tmp.path().join("big.bin"), &mut data, None, None);
        assert!(matches!(r, Err(Error::TooLarge { limit: 1000 })), "{r:?}");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "不應留下任何檔案");
        let mut small = &vec![0u8; 1000][..];
        assert!(plan.write_file("ok.bin", &tmp.path().join("ok.bin"), &mut small, None, None).is_ok());
    }

    /// 宣告的大小可以造假，tar／gz 等格式也沒有清單可預先檢查：解壓時要依實際寫出的量即時判斷壓縮炸彈
    #[test]
    fn live_ratio_check_stops_bombs() {
        let tmp = tempfile::tempdir().unwrap();
        let ar = tmp.path().join("small.gz");
        std::fs::write(&ar, [0u8; 100]).unwrap();
        let out = tmp.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let opts = ExtractOptions::default();
        let mut plan = Planner::new(&ar, &out, &opts, &NoProgress, None);
        plan.bomb_min = 1 << 20; // 測試用：1 MB 就視為大量（實際為 1 GB）
        let mut zeros = io::repeat(0).take(10 << 20);
        let r = plan.write_file("big.bin", &out.join("big.bin"), &mut zeros, None, None);
        assert!(matches!(r, Err(Error::PossibleBomb { packed: 100, .. })), "{r:?}");
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0, "不應留下任何檔案");

        // 使用者確認要繼續時不中斷
        let allow = ExtractOptions { allow_high_ratio: true, ..Default::default() };
        let mut plan = Planner::new(&ar, &out, &allow, &NoProgress, None);
        plan.bomb_min = 1 << 20;
        let mut zeros = io::repeat(0).take(10 << 20);
        plan.write_file("big.bin", &out.join("big.bin"), &mut zeros, None, None).unwrap();
        assert_eq!(std::fs::metadata(out.join("big.bin")).unwrap().len(), 10 << 20);
    }

    #[test]
    fn memory_budget_limits_threads() {
        let budget = memory_budget();
        assert!(budget >= 256 << 20, "預算過小：{budget}");
        if cfg!(target_pointer_width = "32") {
            assert!(budget <= 1536 << 20, "32 位元程式的預算不可超過 1.5 GB");
        }
        // 每個執行緒需要的記憶體越多，執行緒越少，但至少 1 個、最多核心數
        const GB: u64 = 1 << 30;
        assert_eq!(plan_threads(4 * GB, 0, GB, 16), 4);
        assert_eq!(plan_threads(4 * GB, 0, 64 << 20, 16), 16);
        assert_eq!(plan_threads(4 * GB, 0, u64::MAX, 16), 1);
        // 其他工作已用掉的部分要扣除；用完時仍可用 1 個
        assert_eq!(plan_threads(4 * GB, 3 * GB, GB, 16), 1);
        assert_eq!(plan_threads(4 * GB, 5 * GB, GB, 16), 1);
        assert_eq!(plan_threads(4 * GB, GB, GB, 16), 3);
        // 32 位元的預算（1.5 GB）下，最高等級 7z（字典 64 MB，每執行緒約 1 GB）只能單執行緒
        assert_eq!(plan_threads(1536 << 20, 0, (64 << 20) * 16, 16), 1);
    }

    /// 其他程式佔用很多記憶體時，預算以目前可用的為上限（依總量開執行緒會配置失敗而當掉）
    #[test]
    fn budget_respects_available_memory() {
        const GB: u64 = 1 << 30;
        assert_eq!(budget_from(32 * GB, 30 * GB, false), 16 * GB, "可用的夠多：實體記憶體的一半");
        assert_eq!(budget_from(32 * GB, 5 * GB, false), 5 * GB, "可用的不多：以可用的為上限");
        assert_eq!(budget_from(32 * GB, 30 * GB, true), 1536 << 20, "32 位元最多 1.5 GB");
        assert_eq!(budget_from(32 * GB, 0, false), 256 << 20, "至少保留一點，仍能以單執行緒進行");
    }

    /// 同時進行的工作共用記憶體預算：預算被占用時，後來的工作只能用較少的執行緒；歸還後恢復
    #[test]
    fn concurrent_work_shares_the_budget() {
        // 其他測試可能同時保留記憶體，只會讓這裡分到更少，以下性質不受影響
        let per = memory_budget() / 4;
        let (first, held) = reserve_threads(per);
        let (second, held2) = reserve_threads(per);
        assert!((1..=4).contains(&first) && second >= 1);
        assert!(first + second <= 5, "合計不可超過預算太多：{first} + {second}");
        assert!(RESERVED.load(std::sync::atomic::Ordering::SeqCst) >= u64::from(first + second) * per);
        drop(held);
        drop(held2);
    }

    #[test]
    fn safe_join_rules() {
        let d = Path::new("dest");
        assert_eq!(safe_join(d, "a/b.txt"), Some(d.join("a").join("b.txt")));
        assert_eq!(safe_join(d, "/etc/passwd"), Some(d.join("etc").join("passwd")), "開頭的 / 視為相對路徑");
        assert_eq!(safe_join(d, "a/../../x"), None);
        assert_eq!(safe_join(d, r"..\x"), None);
        assert_eq!(safe_join(d, "./"), None);
    }

    #[cfg(windows)]
    #[test]
    fn sanitize_windows_names() {
        assert_eq!(sanitize_component("a:b"), "a_b");
        assert_eq!(sanitize_component("C:"), "C_");
        assert_eq!(sanitize_component("nul"), "_nul");
        assert_eq!(sanitize_component("Com1.txt"), "_Com1.txt");
        assert_eq!(sanitize_component("console.txt"), "console.txt");
        assert_eq!(sanitize_component("end. ."), "end_");
        assert_eq!(sanitize_component("tab\tname"), "tab_name");
        assert_eq!(sanitize_component("中文檔名.txt"), "中文檔名.txt");
    }
}
