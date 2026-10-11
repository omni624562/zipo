use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::progress::ProgressReader;
use crate::single::{decoder, encoder};
use crate::util::{Item, Planner};
use crate::{Entry, Format, Progress, Result, TestReport};

/// tar 本身不壓縮，外層再套 gz/bz2/xz/zst。
fn inner_format(f: Format) -> Option<Format> {
    match f {
        Format::TarGz => Some(Format::Gz),
        Format::TarBz2 => Some(Format::Bz2),
        Format::TarXz => Some(Format::Xz),
        Format::TarZst => Some(Format::Zst),
        _ => None,
    }
}

pub(crate) fn compress(items: &[Item], output: &Path, format: Format, level: Option<u32>, progress: &dyn Progress) -> Result<()> {
    let file = BufWriter::new(File::create(output)?);
    match inner_format(format) {
        None => write_tar(file, items, progress)?.flush()?,
        Some(f) => {
            let enc = encoder(f, file, level)?;
            write_tar(enc, items, progress)?.finish()?.flush()?;
        }
    }
    Ok(())
}

fn write_tar<W: Write>(w: W, items: &[Item], progress: &dyn Progress) -> Result<W> {
    let mut builder = tar::Builder::new(w);
    builder.follow_symlinks(false);
    for item in items {
        progress.entry(&item.name);
        // 非 ASCII 檔名另外寫入 PAX 擴充標頭（規格上一律是 UTF-8）。只寫在 GNU 標頭時，
        // Windows 內建的 tar 與檔案總管（libarchive）會用系統字碼頁解讀，讀不出中文檔名。
        if !item.name.is_ascii() {
            builder.append_pax_extensions([("path", item.name.as_bytes())])?;
        }
        if item.is_dir {
            builder.append_dir(&item.name, &item.path)?;
        } else {
            let file = File::open(&item.path)?;
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&file.metadata()?);
            let reader = ProgressReader::new(BufReader::new(file), progress);
            builder.append_data(&mut header, &item.name, reader)?;
        }
    }
    Ok(builder.into_inner()?)
}

fn open_reader<'a>(archive: &Path, format: Format, progress: &'a dyn Progress) -> Result<Box<dyn Read + 'a>> {
    // 以「已讀取的壓縮檔位元組」計算進度
    progress.start(std::fs::metadata(archive)?.len());
    let raw = ProgressReader::new(BufReader::new(File::open(archive)?), progress);
    Ok(match inner_format(format) {
        None => Box::new(raw),
        Some(f) => decoder(f, raw)?,
    })
}

fn path_lossy<R: Read>(e: &tar::Entry<'_, R>) -> String {
    String::from_utf8_lossy(&e.path_bytes()).into_owned()
}

fn mtime<R: Read>(e: &tar::Entry<'_, R>) -> Option<SystemTime> {
    // 異常的時間戳（超出 SystemTime 範圍）視為未知
    e.header().mtime().ok().and_then(|s| UNIX_EPOCH.checked_add(Duration::from_secs(s)))
}

pub(crate) fn extract(archive: &Path, plan: &Planner, format: Format, progress: &dyn Progress) -> Result<()> {
    let mut ar = tar::Archive::new(open_reader(archive, format, progress)?);
    for entry in ar.entries()? {
        let mut entry = entry?;
        let name = path_lossy(&entry);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            if let Some(out) = plan.path_for(&name, true) {
                plan.make_dir(&name, &out);
            }
        } else if kind.is_file() {
            let Some(out) = plan.path_for(&name, false) else { continue };
            progress.entry(&name);
            let (t, mode) = (mtime(&entry), entry.header().mode().ok());
            plan.write_file(&name, &out, &mut entry, t, mode)?;
        } else if plan.selected(&name) {
            // 與 7-Zip 預設相同：不建立符號連結、硬連結與裝置檔。
            // 連結可能指向目標資料夾之外，後續項目再經由它寫入就會逃出資料夾。
            plan.issue(&name, "符號連結或特殊檔案，基於安全考量不建立");
        }
    }
    Ok(())
}

pub(crate) fn test(archive: &Path, format: Format, progress: &dyn Progress) -> Result<TestReport> {
    let mut ar = tar::Archive::new(open_reader(archive, format, progress)?);
    let mut report = TestReport::default();
    for entry in ar.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_file() {
            progress.entry(&path_lossy(&entry));
            report.files += 1;
            report.bytes += io::copy(&mut entry, &mut io::sink())?;
        }
    }
    Ok(report)
}

pub(crate) fn list(archive: &Path, format: Format) -> Result<Vec<Entry>> {
    let mut ar = tar::Archive::new(open_reader(archive, format, &crate::NoProgress)?);
    let mut out = Vec::new();
    for entry in ar.entries()? {
        let entry = entry?;
        out.push(Entry {
            name: path_lossy(&entry),
            is_dir: entry.header().entry_type().is_dir(),
            size: Some(entry.header().size()?),
            compressed_size: None,
            modified: mtime(&entry),
            encrypted: false,
        });
    }
    Ok(out)
}
