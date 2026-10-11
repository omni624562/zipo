use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use zip::result::ZipError;
use zip::write::SimpleFileOptions;
use zip::{AesMode, CompressionMethod, ZipArchive, ZipWriter};

use crate::progress::ProgressReader;
use crate::util::{Item, Planner, decode_zip_name, from_zip_time, to_zip_time};
use crate::{CompressOptions, Entry, Error, Progress, Result, TestReport};

pub(crate) fn compress(items: &[Item], output: &Path, opts: &CompressOptions, progress: &dyn Progress) -> Result<()> {
    let mut zip = ZipWriter::new(BufWriter::new(File::create(output)?));
    write_items(&mut zip, items, opts, progress)?;
    zip.finish()?.flush()?;
    Ok(())
}

/// 更新：原有項目直接複製壓縮後的資料（不重新壓縮、加密的項目不需要密碼），
/// 略過 `drop` 為真的項目，再加入新項目。
pub(crate) fn update(
    archive: &Path,
    output: &Path,
    items: &[Item],
    drop: impl Fn(&str) -> bool,
    opts: &CompressOptions,
    progress: &dyn Progress,
) -> Result<()> {
    let mut src = open(archive)?;
    let mut zip = ZipWriter::new(BufWriter::new(File::create(output)?));
    for i in 0..src.len() {
        let f = src.by_index_raw(i)?;
        let name = entry_name(&f);
        if drop(&crate::util::normalize_name(&name)) {
            continue;
        }
        progress.entry(&name);
        zip.raw_copy_file(f)?;
    }
    write_items(&mut zip, items, opts, progress)?;
    zip.finish()?.flush()?;
    Ok(())
}

/// 小於此大小的檔案會平行壓縮（先在記憶體中壓好，再依原順序寫入）；更大的檔案逐一串流壓縮。
const PARALLEL_MAX_FILE: u64 = 64 << 20;
/// 每一批平行壓縮的原始資料上限（壓好的結果會先放在記憶體），依記憶體預算再縮小。
const PARALLEL_BATCH_BYTES: u64 = 512 << 20;

fn entry_options(item: &Item, level: u32) -> SimpleFileOptions {
    let mut base = SimpleFileOptions::default()
        .compression_method(if level == 0 { CompressionMethod::Stored } else { CompressionMethod::Deflated })
        .compression_level(if level == 0 { None } else { Some(level as i64) })
        .large_file(item.size > u32::MAX as u64);
    if let Some(t) = to_zip_time(item.modified) {
        base = base.last_modified_time(t);
    }
    base
}

fn write_entry<W: Write + io::Seek>(zip: &mut ZipWriter<W>, item: &Item, opts: &CompressOptions, progress: &dyn Progress) -> Result<()> {
    let base = entry_options(item, opts.level.unwrap_or(5));
    match &opts.password {
        Some(pw) => zip.start_file(&item.name, base.with_aes_encryption(AesMode::Aes256, pw))?,
        None => zip.start_file(&item.name, base)?,
    }
    let mut reader = ProgressReader::new(BufReader::new(File::open(&item.path)?), progress);
    io::copy(&mut reader, zip)?;
    Ok(())
}

/// 把一個檔案壓成只含它的記憶體內 zip，之後再原封不動複製到輸出檔。
fn compress_to_memory(item: &Item, opts: &CompressOptions, progress: &dyn Progress) -> Result<Vec<u8>> {
    let mut z = ZipWriter::new(io::Cursor::new(Vec::with_capacity(item.size as usize / 2)));
    write_entry(&mut z, item, opts, progress)?;
    Ok(z.finish()?.into_inner())
}

fn write_items<W: Write + io::Seek>(zip: &mut ZipWriter<W>, items: &[Item], opts: &CompressOptions, progress: &dyn Progress) -> Result<()> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let batch_limit = PARALLEL_BATCH_BYTES.min(crate::util::memory_budget() / 4);
    let small = |i: &Item| !i.is_dir && i.size < PARALLEL_MAX_FILE;
    let mut i = 0;
    while i < items.len() {
        let item = &items[i];
        if threads == 1 || !small(item) {
            progress.entry(&item.name);
            if item.is_dir {
                zip.add_directory(&item.name, entry_options(item, opts.level.unwrap_or(5)))?;
            } else {
                write_entry(zip, item, opts, progress)?;
            }
            i += 1;
            continue;
        }
        // 連續的小檔案為一批，交給多個執行緒同時壓縮
        let start = i;
        let mut bytes = 0;
        while i < items.len() && small(&items[i]) && bytes < batch_limit {
            bytes += items[i].size;
            i += 1;
        }
        let batch = &items[start..i];
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results: Vec<std::sync::Mutex<Option<Result<Vec<u8>>>>> = batch.iter().map(|_| Default::default()).collect();
        std::thread::scope(|s| {
            for _ in 0..threads.min(batch.len()) {
                s.spawn(|| {
                    loop {
                        let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(item) = batch.get(k) else { break };
                        if progress.is_cancelled() {
                            break;
                        }
                        progress.entry(&item.name);
                        let r = compress_to_memory(item, opts, progress);
                        let failed = r.is_err();
                        *results[k].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                        if failed {
                            break;
                        }
                    }
                });
            }
        });
        // 依原本的順序寫入，輸出與逐一壓縮的結果相同
        for r in results {
            let Some(r) = r.into_inner().unwrap_or_else(|e| e.into_inner()) else {
                return Err(Error::Io(io::Error::other("cancelled")));
            };
            let mut mem = ZipArchive::new(io::Cursor::new(r?))?;
            zip.raw_copy_file(mem.by_index_raw(0)?)?;
        }
    }
    Ok(())
}

fn open(archive: &Path) -> Result<ZipArchive<BufReader<File>>> {
    Ok(ZipArchive::new(BufReader::new(File::open(archive)?))?)
}

/// 逐一處理（已選取的）項目；`f` 收到項目名稱、是否為資料夾、修改時間與資料讀取器。
fn for_each(
    archive: &Path,
    password: Option<&str>,
    select: impl Fn(&str) -> bool,
    progress: &dyn Progress,
    mut f: impl FnMut(&str, bool, Option<std::time::SystemTime>, &mut dyn Read) -> Result<()>,
) -> Result<()> {
    let mut zip = open(archive)?;
    let mut total = 0;
    for i in 0..zip.len() {
        let raw = zip.by_index_raw(i)?;
        if select(&entry_name(&raw)) {
            total += raw.size();
        }
    }
    progress.start(total);

    for i in 0..zip.len() {
        if !select(&entry_name(&zip.by_index_raw(i)?)) {
            continue;
        }
        let file = match password {
            Some(pw) => zip.by_index_decrypt(i, pw.as_bytes()),
            None => zip.by_index(i),
        };
        let mut file = file.map_err(map_err)?;
        let name = entry_name(&file);
        progress.entry(&name);
        let is_dir = file.is_dir();
        let mtime = from_zip_time(file.last_modified());
        f(&name, is_dir, mtime, &mut ProgressReader::new(&mut file, progress)).map_err(|e| match e {
            // AES 驗證碼錯誤時，讀取資料階段才會發現
            Error::Io(io) if io.kind() == io::ErrorKind::InvalidData && password.is_some() => Error::WrongPassword,
            e => e,
        })?;
    }
    Ok(())
}

pub(crate) fn extract(archive: &Path, plan: &Planner, password: Option<&str>, progress: &dyn Progress) -> Result<()> {
    for_each(archive, password, |n| plan.selected(n), progress, |name, is_dir, mtime, r| {
        let Some(out) = plan.path_for(name, is_dir) else { return Ok(()) };
        if is_dir {
            plan.make_dir(name, &out);
            return Ok(());
        }
        plan.write_file(name, &out, r, mtime, None)
    })
}

pub(crate) fn test(archive: &Path, password: Option<&str>, progress: &dyn Progress) -> Result<TestReport> {
    let mut report = TestReport::default();
    for_each(archive, password, |_| true, progress, |_, is_dir, _, r| {
        if !is_dir {
            report.files += 1;
            report.bytes += io::copy(r, &mut io::sink())?;
        }
        Ok(())
    })?;
    Ok(report)
}

pub(crate) fn list(archive: &Path) -> Result<Vec<Entry>> {
    let mut zip = open(archive)?;
    let mut entries = Vec::with_capacity(zip.len());
    for i in 0..zip.len() {
        let f = zip.by_index_raw(i)?;
        entries.push(Entry {
            name: entry_name(&f),
            is_dir: f.is_dir(),
            size: Some(f.size()),
            compressed_size: Some(f.compressed_size()),
            modified: from_zip_time(f.last_modified()),
            encrypted: f.encrypted(),
        });
    }
    Ok(entries)
}

/// 項目名稱：沒有 UTF-8 標記的舊式 zip（例如 Windows 內建壓縮產生、含中文檔名）依字碼頁解碼。
fn entry_name<R: Read + ?Sized>(f: &zip::read::ZipFile<'_, R>) -> String {
    decode_zip_name(f.name_raw(), f.name())
}

fn map_err(e: ZipError) -> Error {
    match e {
        ZipError::InvalidPassword => Error::WrongPassword,
        ZipError::UnsupportedArchive(msg) if msg.to_lowercase().contains("password") => Error::PasswordRequired,
        e => Error::Zip(e),
    }
}
