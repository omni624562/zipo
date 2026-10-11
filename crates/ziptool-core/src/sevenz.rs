use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::time::SystemTime;

use sevenz_rust2::encoder_options::{AesEncoderOptions, Lzma2Options};
use sevenz_rust2::{Archive, ArchiveEntry, ArchiveReader, ArchiveWriter, EncoderMethod, Password, SourceReader};

use crate::progress::ProgressReader;
use crate::util::{Item, Planner};
use crate::{CompressOptions, Entry, Error, Progress, Result, TestReport};

fn map_err(e: sevenz_rust2::Error) -> Error {
    match e {
        sevenz_rust2::Error::PasswordRequired => Error::PasswordRequired,
        sevenz_rust2::Error::MaybeBadPassword(_) => Error::WrongPassword,
        e => Error::SevenZ(e.to_string()),
    }
}

fn password(pw: Option<&str>) -> Password {
    pw.map(Password::from).unwrap_or_else(Password::empty)
}

fn mtime(e: &ArchiveEntry) -> Option<SystemTime> {
    e.has_last_modified_date.then(|| nt_time(e.last_modified_date().into())).flatten()
}

/// Windows 檔案時間（自 1601 年起的 100 奈秒數）轉為 SystemTime。
/// 函式庫內建的轉換沒有檢查溢位，惡意壓縮檔的極端時間會讓程式當掉；超出範圍時視為未知。
fn nt_time(ticks: u64) -> Option<SystemTime> {
    const TO_UNIX_SECS: u64 = 11_644_473_600;
    let (secs, nanos) = (ticks / 10_000_000, (ticks % 10_000_000) as u32 * 100);
    if secs >= TO_UNIX_SECS {
        SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::new(secs - TO_UNIX_SECS, nanos))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(std::time::Duration::new(TO_UNIX_SECS - secs, 0))?.checked_add(std::time::Duration::new(0, nanos))
    }
}

/// 各壓縮等級的 LZMA2 字典大小（與 xz／7-Zip 的預設值相同）。
fn dict_size(level: u32) -> u64 {
    const MB: u64 = 1 << 20;
    match level {
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

/// 一個固實區塊最多放多少原始資料。區塊越大壓縮率越好，但解出單一檔案時要解碼的資料越多。
const SOLID_BLOCK_BYTES: u64 = 256 << 20;
const SOLID_BLOCK_FILES: usize = 10_000;

/// 開始讀取時才開檔：固實區塊可能包含上萬個檔案，不能同時全部開啟。
struct LazyFile<'a> {
    path: &'a Path,
    name: &'a str,
    file: Option<BufReader<File>>,
    progress: &'a dyn Progress,
}

impl Read for LazyFile<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.file.is_none() {
            self.progress.entry(self.name);
            self.file = Some(BufReader::new(File::open(self.path)?));
        }
        let n = ProgressReader::new(self.file.as_mut().unwrap(), self.progress).read(buf)?;
        if n == 0 {
            self.file = None; // 讀完就關檔
        }
        Ok(n)
    }
}

pub(crate) fn compress(items: &[Item], output: &Path, opts: &CompressOptions, progress: &dyn Progress) -> Result<()> {
    let mut w = ArchiveWriter::new(BufWriter::new(File::create(output)?)).map_err(map_err)?;
    let level = opts.level.unwrap_or(5);
    // 與 7-Zip 相同：多執行緒時每個工作區塊為字典大小的 4 倍。
    // 每個執行緒約需「字典 × 12（比對結構）＋ 工作區塊」的記憶體。
    let dict = dict_size(level);
    let threads = crate::util::threads_for(dict * 12 + dict * 4);
    let lzma2 = Lzma2Options::from_level_mt(level, threads, dict * 4);
    match &opts.password {
        Some(pw) => {
            // 金鑰延伸次數與 7-Zip 相同（2^19 次 SHA-256）；函式庫預設只有 2^8，密碼容易被暴力破解
            let mut aes = AesEncoderOptions::new(Password::from(pw.as_str()));
            aes.num_cycles_power = 19;
            w.set_content_methods(vec![aes.into(), lzma2.into()])
        }
        None => w.set_content_methods(vec![lzma2.into()]),
    };
    w.set_encrypt_header(opts.encrypt_names);

    // 資料夾與空檔案沒有資料流，個別加入；其餘檔案依序放進固實區塊
    let mut block: Vec<&Item> = Vec::new();
    let mut block_bytes = 0u64;
    for item in items {
        if item.is_dir || item.size == 0 {
            progress.entry(&item.name);
            let entry = ArchiveEntry::from_path(&item.path, item.name.clone());
            let empty = (!item.is_dir).then_some(io::empty());
            w.push_archive_entry(entry, empty).map_err(map_err)?;
            continue;
        }
        block.push(item);
        block_bytes += item.size;
        if block_bytes >= SOLID_BLOCK_BYTES || block.len() >= SOLID_BLOCK_FILES {
            push_solid(&mut w, &block, progress)?;
            block.clear();
            block_bytes = 0;
        }
    }
    if !block.is_empty() {
        push_solid(&mut w, &block, progress)?;
    }
    w.finish()?.flush()?;
    Ok(())
}

fn push_solid<W: Write + io::Seek>(w: &mut ArchiveWriter<W>, items: &[&Item], progress: &dyn Progress) -> Result<()> {
    let entries = items.iter().map(|i| ArchiveEntry::from_path(&i.path, i.name.clone())).collect();
    let readers = items
        .iter()
        .map(|i| SourceReader::new(LazyFile { path: &i.path, name: &i.name, file: None, progress }))
        .collect();
    w.push_archive_entries(entries, readers).map_err(map_err)?;
    Ok(())
}

/// 逐一處理項目。未選取的項目也必須讀完（固實壓縮的資料是連續的），但不計入進度。
fn for_each(
    archive: &Path,
    pw: Option<&str>,
    select: impl Fn(&str) -> bool,
    progress: &dyn Progress,
    mut f: impl FnMut(&ArchiveEntry, &mut dyn Read) -> Result<()>,
) -> Result<()> {
    let mut reader = ArchiveReader::new(BufReader::new(File::open(archive)?), password(pw)).map_err(map_err)?;
    // 解碼時每個執行緒最多需要一個字典（最大 64 MB）加上緩衝區
    reader.set_thread_count(crate::util::threads_for(160 << 20));
    progress.start(reader.archive().files.iter().filter(|f| select(f.name())).map(|f| f.size()).sum());

    let mut own_err = None;
    let res = reader.for_each_entries(|entry, r| {
        if !select(entry.name()) {
            io::copy(r, &mut io::sink())?;
            return Ok(true);
        }
        progress.entry(entry.name());
        match f(entry, &mut ProgressReader::new(r, progress)) {
            Ok(()) => Ok(true),
            Err(Error::Io(e)) => Err(e.into()),
            Err(e) => {
                own_err = Some(e);
                Err(io::Error::other("aborted").into())
            }
        }
    });
    if let Some(e) = own_err {
        return Err(e);
    }
    res.map(|_| ()).map_err(map_err)
}

pub(crate) fn extract(archive: &Path, plan: &Planner, pw: Option<&str>, progress: &dyn Progress) -> Result<()> {
    for_each(archive, pw, |n| plan.selected(n), progress, |entry, r| {
        let Some(out) = plan.path_for(entry.name(), entry.is_directory()) else {
            // 固實壓縮的資料是連續的，略過的項目也要讀完
            io::copy(r, &mut io::sink())?;
            return Ok(());
        };
        if entry.is_directory() {
            plan.make_dir(entry.name(), &out);
            return Ok(());
        }
        plan.write_file(entry.name(), &out, r, mtime(entry), None)
    })
}

pub(crate) fn test(archive: &Path, pw: Option<&str>, progress: &dyn Progress) -> Result<TestReport> {
    let mut report = TestReport::default();
    for_each(archive, pw, |_| true, progress, |entry, r| {
        if !entry.is_directory() {
            report.files += 1;
            report.bytes += io::copy(r, &mut io::sink())?;
        }
        Ok(())
    })?;
    Ok(report)
}

pub(crate) fn list(archive: &Path, pw: Option<&str>) -> Result<Vec<Entry>> {
    let mut file = BufReader::new(File::open(archive)?);
    let ar = Archive::read(&mut file, &password(pw)).map_err(map_err)?;
    let encrypted_block = |i: usize| {
        ar.stream_map.file_block_index[i].is_some_and(|b| {
            ar.blocks[b].coders.iter().any(|c| c.encoder_method_id() == EncoderMethod::ID_AES256_SHA256)
        })
    };
    Ok(ar
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| Entry {
            name: f.name().to_string(),
            is_dir: f.is_directory(),
            size: Some(f.size()),
            compressed_size: None,
            modified: mtime(f),
            encrypted: encrypted_block(i),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 惡意壓縮檔的極端時間不可讓程式當掉
    #[test]
    fn nt_time_never_panics() {
        assert_eq!(nt_time(116_444_736_000_000_000), Some(SystemTime::UNIX_EPOCH));
        assert_eq!(nt_time(116_444_736_000_000_000 + 15), SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_nanos(1500)));
        for t in [0, 1, u64::MAX - 5, u64::MAX] {
            let _ = nt_time(t);
        }
    }

    /// 加密的金鑰延伸次數要與 7-Zip 相同（2^19 次 SHA-256），否則密碼容易被暴力破解
    #[test]
    fn aes_key_stretching_matches_7zip() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.txt");
        std::fs::write(&src, "secret").unwrap();
        let ar = tmp.path().join("a.7z");
        let opts = CompressOptions { password: Some("pw".into()), ..Default::default() };
        crate::compress(&[src], &ar, crate::Format::SevenZ, &opts, &crate::NoProgress).unwrap();

        let archive = Archive::read(&mut File::open(&ar).unwrap(), &Password::from("pw")).unwrap();
        let aes = archive.blocks.iter().flat_map(|b| &b.coders).find(|c| c.encoder_method_id() == [0x06, 0xF1, 0x07, 0x01]).expect("應有 AES 加密");
        assert_eq!(aes.properties()[0] & 0x3F, 19, "金鑰延伸次數應為 2^19");
    }
}
