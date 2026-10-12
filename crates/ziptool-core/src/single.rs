//! 單檔壓縮格式：gz、bz2、xz、zst。也提供 tar.* 共用的編碼／解碼器。

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use crate::progress::ProgressReader;
use crate::util::{Item, Planner};
use crate::{Entry, Error, Format, Progress, Result, TestReport};

pub(crate) enum Encoder<W: Write> {
    Gz(flate2::write::GzEncoder<W>),
    Bz2(bzip2::write::BzEncoder<W>),
    /// 多執行緒 xz 保留的記憶體與編碼器一起存在，壓縮完成後才歸還
    Xz(xz2::write::XzEncoder<W>, crate::util::MemoryReservation),
    Zst(zstd::stream::write::Encoder<'static, W>),
}

impl<W: Write> Encoder<W> {
    pub fn finish(self) -> io::Result<W> {
        match self {
            Encoder::Gz(e) => e.finish(),
            Encoder::Bz2(e) => e.finish(),
            Encoder::Xz(e, _mem) => e.finish(),
            Encoder::Zst(e) => e.finish(),
        }
    }
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Encoder::Gz(e) => e.write(buf),
            Encoder::Bz2(e) => e.write(buf),
            Encoder::Xz(e, _) => e.write(buf),
            Encoder::Zst(e) => e.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Encoder::Gz(e) => e.flush(),
            Encoder::Bz2(e) => e.flush(),
            Encoder::Xz(e, _) => e.flush(),
            Encoder::Zst(e) => e.flush(),
        }
    }
}

/// `level` 為 0–9，依各演算法換算。
pub(crate) fn encoder<W: Write>(f: Format, w: W, level: Option<u32>) -> Result<Encoder<W>> {
    Ok(match f {
        Format::Gz => Encoder::Gz(flate2::write::GzEncoder::new(
            w,
            flate2::Compression::new(level.unwrap_or(6)),
        )),
        Format::Bz2 => Encoder::Bz2(bzip2::write::BzEncoder::new(
            w,
            bzip2::Compression::new(level.unwrap_or(9).clamp(1, 9)),
        )),
        Format::Xz => {
            let preset = level.unwrap_or(6);
            // 多執行緒 xz：輸出仍是標準 .xz，任何 xz 工具都能解開；建立失敗時退回單執行緒。
            // 每個執行緒約需「字典 × 12 ＋ 2 個區塊（區塊為字典的 3 倍）」的記憶體。
            let dict = xz_dict_size(preset);
            let (threads, mem) = crate::util::reserve_threads(dict * 18);
            let mt = xz2::stream::MtStreamBuilder::new()
                .threads(threads)
                .block_size(dict * 3)
                .preset(preset)
                .check(xz2::stream::Check::Crc64)
                .encoder();
            match mt {
                Ok(stream) => Encoder::Xz(xz2::write::XzEncoder::new_stream(w, stream), mem),
                Err(_) => Encoder::Xz(xz2::write::XzEncoder::new(w, preset), mem),
            }
        }
        Format::Zst => {
            // zstd 等級 1–19；未指定時用 zstd 預設 3，否則 0–9 對應到 1–19
            let lv = level.map(|l| (l as i32 * 2).clamp(1, 19)).unwrap_or(3);
            let mut enc = zstd::stream::write::Encoder::new(w, lv)?;
            let _ = enc.multithread(threads());
            Encoder::Zst(enc)
        }
        _ => unreachable!("not a stream format"),
    })
}

/// xz 各預設等級的字典大小
fn xz_dict_size(preset: u32) -> u64 {
    const MB: u64 = 1 << 20;
    match preset {
        0 => MB / 4,
        1 => MB,
        2 => 2 * MB,
        3 | 4 => 4 * MB,
        5 | 6 => 8 * MB,
        7 => 16 * MB,
        8 => 32 * MB,
        _ => 64 * MB,
    }
}

fn threads() -> u32 {
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1)
}

pub(crate) fn decoder<'a, R: Read + 'a>(f: Format, r: R) -> Result<Box<dyn Read + 'a>> {
    Ok(match f {
        Format::Gz => Box::new(flate2::read::MultiGzDecoder::new(r)),
        Format::Bz2 => Box::new(bzip2::read::MultiBzDecoder::new(r)),
        Format::Xz => Box::new(xz2::read::XzDecoder::new_multi_decoder(r)),
        Format::Zst => Box::new(zstd::stream::read::Decoder::new(r)?),
        _ => unreachable!("not a stream format"),
    })
}

pub(crate) fn compress(
    item: &Item,
    output: &Path,
    format: Format,
    level: Option<u32>,
    progress: &dyn Progress,
) -> Result<()> {
    progress.entry(&item.name);
    let mut enc = encoder(format, BufWriter::new(File::create(output)?), level)?;
    let mut r = ProgressReader::new(BufReader::new(File::open(&item.path)?), progress);
    io::copy(&mut r, &mut enc)?;
    enc.finish()?.flush()?;
    Ok(())
}

/// 解壓後的檔名：去掉副檔名；無法判斷時加上 `.out`。
fn output_name(archive: &Path, format: Format) -> Result<String> {
    let name = archive
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| Error::Unsupported("無效的檔名".into()))?;
    Ok(match Format::split_extension(&name) {
        Some((stem, f)) if f == format && !stem.is_empty() => stem.to_string(),
        _ => format!("{name}.out"),
    })
}

pub(crate) fn extract(archive: &Path, plan: &Planner, format: Format, progress: &dyn Progress) -> Result<()> {
    progress.start(std::fs::metadata(archive)?.len());
    let name = output_name(archive, format)?;
    let Some(out) = plan.path_for(&name, false) else { return Ok(()) };
    progress.entry(&name);
    let raw = ProgressReader::new(BufReader::new(File::open(archive)?), progress);
    let mut dec = decoder(format, raw)?;
    plan.write_file(&name, &out, &mut dec, std::fs::metadata(archive)?.modified().ok(), None)
}

pub(crate) fn test(archive: &Path, format: Format, progress: &dyn Progress) -> Result<TestReport> {
    progress.start(std::fs::metadata(archive)?.len());
    progress.entry(&output_name(archive, format)?);
    let raw = ProgressReader::new(BufReader::new(File::open(archive)?), progress);
    let bytes = io::copy(&mut decoder(format, raw)?, &mut io::sink())?;
    Ok(TestReport { files: 1, bytes })
}

pub(crate) fn list(archive: &Path, format: Format) -> Result<Vec<Entry>> {
    let meta = std::fs::metadata(archive)?;
    Ok(vec![Entry {
        name: output_name(archive, format)?,
        is_dir: false,
        size: None,
        compressed_size: Some(meta.len()),
        modified: meta.modified().ok(),
        encrypted: false,
    }])
}
