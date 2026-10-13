//! ISO 映像檔（ISO 9660，含 Joliet 中文檔名與 Rock Ridge）：只能讀取。
//!
//! 只使用 UDF 的映像檔（例如 Windows 安裝光碟）目前無法讀取，會明確告知。
//! 映像檔可能是惡意的：目錄互相指向形成迴圈、層數過深都會被擋下。

use crate::{F, L};
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::time::SystemTime;

use hadris_iso::file::EntryType;
use hadris_iso::joliet::decode_joliet_name;
use hadris_iso::read::{DirEntry, FileChunkIterator, IsoDir, IsoImage};

use crate::progress::ProgressReader;
use crate::util::Planner;
use crate::{Entry, Error, Progress, Result, TestReport};

type Image = IsoImage<BufReader<File>>;

/// 目錄的最大層數（防止惡意映像檔讓遞迴過深）
const MAX_DEPTH: usize = 64;

/// 一筆目錄記錄最少 34 位元組；項目數超過「映像檔大小 ÷ 34」代表目錄互相重疊、同一筆記錄被重複計算
const MIN_RECORD: u64 = 34;

/// 開啟映像檔；同時回傳項目數量的上限
fn open(archive: &Path) -> Result<(Image, usize)> {
    let file = File::open(archive)?;
    let limit = usize::try_from(file.metadata()?.len() / MIN_RECORD).unwrap_or(usize::MAX).saturating_add(16);
    let image = IsoImage::open(BufReader::new(file)).map_err(|e| iso_err(&e, archive))?;
    Ok((image, limit))
}

/// 函式庫的讀取錯誤轉為一般的 I/O 錯誤
fn ioe(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

fn iso_err(e: &dyn std::fmt::Display, archive: &Path) -> Error {
    if is_udf_only(archive) {
        Error::Unsupported(L!("這個 ISO 只使用 UDF 格式（例如 Windows 安裝光碟），目前還無法讀取。可以在檔案總管對它按右鍵選「掛接」來開啟。", "This ISO uses only the UDF format (for example, Windows installation media) and cannot be read yet. In File Explorer, right-click it and choose \"Mount\" to open it.").into())
    } else {
        Error::Unsupported(F!("無法讀取 ISO：{e}", "Cannot read the ISO: {e}"))
    }
}

/// 有 UDF 標記（NSR02／NSR03）的映像檔
fn is_udf_only(archive: &Path) -> bool {
    let Ok(mut f) = File::open(archive) else { return false };
    let mut buf = vec![0u8; 2048 * 32];
    let n = f.read(&mut buf).unwrap_or(0);
    buf[..n].windows(5).any(|w| w == b"NSR02" || w == b"NSR03")
}

/// 檔名：Joliet 為 UTF-16，其餘使用 Rock Ridge 名稱或 ISO 9660 名稱；去掉版本號（`;1`）與結尾的點
fn entry_name(entry: &DirEntry, joliet: bool) -> String {
    let raw = if joliet { decode_joliet_name(entry.name()) } else { entry.display_name().into_owned() };
    let name = raw.split(';').next().unwrap_or(&raw);
    let name = name.strip_suffix('.').unwrap_or(name);
    name.to_string()
}

/// ISO 9660 目錄記錄的時間（年份自 1900、月、日、時、分、秒、與 UTC 的差距以 15 分鐘為單位）
fn entry_time(entry: &DirEntry) -> Option<SystemTime> {
    let d = &entry.header().date_time;
    // SAFETY: DirDateTime 是 7 個位元組的 repr(C) 結構（bytemuck::Pod），逐位元組讀取
    let b: [u8; 7] = unsafe { std::mem::transmute_copy(d) };
    let (year, month, day, hour, minute, second, offset) = (1900 + i32::from(b[0]), b[1], b[2], b[3], b[4], b[5], b[6] as i8);
    let date = time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()?;
    let t = time::Time::from_hms(hour, minute, second).ok()?;
    let offset = time::UtcOffset::from_whole_seconds(i32::from(offset) * 15 * 60).ok()?;
    let dt = time::PrimitiveDateTime::new(date, t).assume_offset(offset);
    Some(dt.into())
}

/// 依序走訪所有項目：`f(完整名稱, 項目)`；項目數超過 `limit` 時停止並回報錯誤
fn walk(image: &Image, limit: usize, f: &mut dyn FnMut(&str, &DirEntry) -> Result<()>) -> Result<()> {
    let root = image.root_dirs().try_best_choice().ok_or_else(|| Error::Unsupported(L!("ISO 中沒有目錄", "The ISO has no directory").into()))?;
    let joliet = matches!(root.entry_type(), EntryType::Joliet { .. });
    let mut seen = HashSet::new();
    let mut count = 0usize;
    let mut counted = |name: &str, e: &DirEntry| {
        count += 1;
        if count > limit {
            return Err(Error::Unsupported(L!("ISO 的目錄互相重疊，項目數量異常，可能是損壞或惡意的檔案", "The ISO directories overlap and contain an abnormal number of items; the file may be damaged or malicious").into()));
        }
        f(name, e)
    };
    walk_dir(image, image.open_dir(root.dir_ref()), "", joliet, 0, &mut seen, &mut counted)
}

fn walk_dir(
    image: &Image,
    dir: IsoDir<'_, BufReader<File>>,
    prefix: &str,
    joliet: bool,
    depth: usize,
    seen: &mut HashSet<usize>,
    f: &mut dyn FnMut(&str, &DirEntry) -> Result<()>,
) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(Error::Unsupported(L!("ISO 的目錄層數過深，可能是損壞或惡意的檔案", "The ISO directories are nested too deeply; the file may be damaged or malicious").into()));
    }
    let entries: Vec<DirEntry> = dir.entries().collect::<std::result::Result<_, _>>().map_err(ioe)?;
    for entry in entries {
        if entry.is_special() {
            continue;
        }
        let name = entry_name(&entry, joliet);
        if name.is_empty() {
            continue;
        }
        let full = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        f(&full, &entry)?;
        if entry.is_directory() {
            let dref = entry.as_dir_ref(image).map_err(ioe)?;
            // 目錄指向已走訪過的位置（迴圈）時不再進入
            if !seen.insert(dref.extent.0) {
                continue;
            }
            walk_dir(image, image.open_dir(dref), &full, joliet, depth + 1, seen, f)?;
        }
    }
    Ok(())
}

/// 檔案內容的讀取器（一段一段讀，不會一次載入整個檔案）
struct FileReader<'a> {
    chunks: FileChunkIterator<'a, BufReader<File>, 65536>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for FileReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() {
            match self.chunks.next_chunk().map_err(ioe)? {
                Some(c) => {
                    self.buf = c;
                    self.pos = 0;
                }
                None => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn reader<'a>(image: &'a Image, entry: &DirEntry) -> Result<FileReader<'a>> {
    Ok(FileReader { chunks: image.read_file_chunked::<65536>(entry).map_err(ioe)?, buf: Vec::new(), pos: 0 })
}

pub(crate) fn list(archive: &Path) -> Result<Vec<Entry>> {
    let (image, limit) = open(archive)?;
    let mut out = Vec::new();
    walk(&image, limit, &mut |name, e| {
        let size = (!e.is_directory()).then(|| e.total_size());
        out.push(Entry { name: name.to_string(), is_dir: e.is_directory(), size, compressed_size: size, modified: entry_time(e), encrypted: false });
        Ok(())
    })?;
    // 只有 UDF 的映像檔，ISO 9660 部分通常只放一個說明檔
    if out.len() <= 1 && is_udf_only(archive) {
        return Err(Error::Unsupported(L!("這個 ISO 只使用 UDF 格式（例如 Windows 安裝光碟），目前還無法讀取。可以在檔案總管對它按右鍵選「掛接」來開啟。", "This ISO uses only the UDF format (for example, Windows installation media) and cannot be read yet. In File Explorer, right-click it and choose \"Mount\" to open it.").into()));
    }
    Ok(out)
}

pub(crate) fn extract(archive: &Path, plan: &Planner, progress: &dyn Progress) -> Result<()> {
    let (image, limit) = open(archive)?;
    let mut total = 0u64;
    walk(&image, limit, &mut |name, e| {
        if !e.is_directory() && plan.selected(name) {
            total = total.saturating_add(e.total_size());
        }
        Ok(())
    })?;
    progress.start(total);
    walk(&image, limit, &mut |name, e| {
        if !plan.selected(name) {
            return Ok(());
        }
        let Some(out) = plan.path_for(name, e.is_directory()) else { return Ok(()) };
        progress.entry(name);
        if e.is_directory() {
            plan.make_dir(name, &out);
            return Ok(());
        }
        let mut r = ProgressReader::new(reader(&image, e)?, progress);
        plan.write_file(name, &out, &mut r, entry_time(e), None)
    })
}

pub(crate) fn test(archive: &Path, progress: &dyn Progress) -> Result<TestReport> {
    let (image, limit) = open(archive)?;
    let mut report = TestReport::default();
    walk(&image, limit, &mut |name, e| {
        if !e.is_directory() {
            progress.entry(name);
            report.files += 1;
            report.bytes += io::copy(&mut ProgressReader::new(reader(&image, e)?, progress), &mut io::sink())?;
        }
        Ok(())
    })?;
    Ok(report)
}
