use crate::F;
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

/// 壓縮過的目錄資料（encoded header）解壓縮後的上限。數百萬個項目的目錄也遠小於此；
/// 惡意檔案可以用很小的檔案宣告巨大的目錄，讓函式庫解壓縮時耗盡記憶體。
const MAX_HEADER_UNPACK: u64 = 256 << 20;

/// 在交給函式庫之前，先讀出壓縮過的目錄資料宣告的解壓縮大小，過大就拒絕。
fn check_header_size(archive: &Path) -> Result<()> {
    let declared = match encoded_header_unpack_size(archive) {
        Ok(n) => n,
        // 格式不符的檔案交給函式庫回報正確的錯誤
        Err(_) => return Ok(()),
    };
    if declared > MAX_HEADER_UNPACK {
        return Err(Error::SevenZ(F!("目錄資料宣告為 {}，異常龐大，可能是惡意檔案", "The archive header claims {}, which is abnormally large; the file may be malicious", crate::format_size(declared))));
    }
    Ok(())
}

/// 依 7z 格式解析 encoded header 的 StreamsInfo，回傳其中最大的解壓縮大小；目錄未壓縮時回傳 0。
fn encoded_header_unpack_size(archive: &Path) -> io::Result<u64> {
    use std::io::{Seek, SeekFrom};
    let bad = || io::Error::from(io::ErrorKind::InvalidData);
    let mut f = File::open(archive)?;
    let len = f.metadata()?.len();
    let mut start = [0u8; 32];
    f.read_exact(&mut start)?;
    let offset = u64::from_le_bytes(start[12..20].try_into().unwrap());
    let size = u64::from_le_bytes(start[20..28].try_into().unwrap());
    let pos = offset.checked_add(32).filter(|p| p.checked_add(size).is_some_and(|end| end <= len)).ok_or_else(bad)?;
    // 只需要開頭的 StreamsInfo；最多讀 64 KB
    let mut buf = vec![0u8; size.min(64 << 10) as usize];
    f.seek(SeekFrom::Start(pos))?;
    f.read_exact(&mut buf)?;

    let mut r = buf.as_slice();
    let byte = |r: &mut &[u8]| -> io::Result<u8> {
        let (&b, rest) = r.split_first().ok_or_else(bad)?;
        *r = rest;
        Ok(b)
    };
    // 7z 的可變長度整數
    let number = |r: &mut &[u8]| -> io::Result<u64> {
        let first = byte(r)?;
        let mut mask = 0x80u8;
        let mut value = 0u64;
        for i in 0..8 {
            if first & mask == 0 {
                return Ok(value | (u64::from(first & mask.wrapping_sub(1)) << (8 * i)));
            }
            value |= u64::from(byte(r)?) << (8 * i);
            mask >>= 1;
        }
        Ok(value)
    };
    const K_END: u8 = 0x00;
    if byte(&mut r)? != 0x17 {
        return Ok(0); // 目錄未壓縮：大小已受檔案長度限制
    }
    let mut largest = 0u64;
    loop {
        match byte(&mut r)? {
            K_END => return Ok(largest),
            0x06 => {
                // PackInfo：位置、數量，以及可選的大小與 CRC，直到 kEnd
                number(&mut r)?;
                let n = number(&mut r)?;
                loop {
                    match byte(&mut r)? {
                        K_END => break,
                        0x09 => (0..n).try_for_each(|_| number(&mut r).map(drop))?,
                        0x0A => {
                            // CRC：全部都有，或以位元表標示哪些有；每個 4 位元組
                            let defined = if byte(&mut r)? != 0 {
                                n
                            } else {
                                let bits = r.get(..n.div_ceil(8) as usize).ok_or_else(bad)?;
                                r = &r[bits.len()..];
                                bits.iter().map(|b| u64::from(b.count_ones())).sum()
                            };
                            r = r.get(defined.saturating_mul(4) as usize..).ok_or_else(bad)?;
                        }
                        _ => return Err(bad()),
                    }
                }
            }
            0x07 => {
                // UnpackInfo：kFolder、各資料夾的編碼器定義，接著 kCodersUnpackSize
                if byte(&mut r)? != 0x0B {
                    return Err(bad());
                }
                let folders = number(&mut r)?;
                if byte(&mut r)? != 0 {
                    return Err(bad()); // 外部資料，正常的目錄不會用到
                }
                let mut outputs = 0u64;
                for _ in 0..folders {
                    let (mut ins, mut outs) = (0u64, 0u64);
                    for _ in 0..number(&mut r)? {
                        let flags = byte(&mut r)?;
                        let id_len = (flags & 0x0F) as usize;
                        r = r.get(id_len..).ok_or_else(bad)?;
                        if flags & 0x10 != 0 {
                            ins += number(&mut r)?;
                            outs += number(&mut r)?;
                        } else {
                            ins += 1;
                            outs += 1;
                        }
                        if flags & 0x20 != 0 {
                            let props = number(&mut r)? as usize;
                            r = r.get(props..).ok_or_else(bad)?;
                        }
                    }
                    let pairs = outs.checked_sub(1).ok_or_else(bad)?;
                    for _ in 0..pairs.saturating_mul(2) {
                        number(&mut r)?;
                    }
                    let packed = ins.checked_sub(pairs).ok_or_else(bad)?;
                    if packed > 1 {
                        for _ in 0..packed {
                            number(&mut r)?;
                        }
                    }
                    outputs = outputs.checked_add(outs).ok_or_else(bad)?;
                }
                if byte(&mut r)? != 0x0C {
                    return Err(bad());
                }
                for _ in 0..outputs {
                    largest = largest.max(number(&mut r)?);
                }
                return Ok(largest);
            }
            _ => return Err(bad()),
        }
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
    // 每個執行緒約需「字典 × 12（比對結構）＋ 工作區塊」的記憶體（以工作區塊最大為字典 × 4 預留）。
    // 固實區塊依序壓縮，區塊內的工作區塊才能平行處理：工作區塊依執行緒數平分固實區塊，
    // 所有核心都有工作（原本固定為字典 × 4，一個區塊只能分給 4～8 個執行緒）；最小為字典大小。
    // 實測 580 MB、等級 5、22 核心：59 秒 → 約 40 秒，壓縮檔只大 0.1%。
    let dict = dict_size(level);
    let (threads, _mem) = crate::util::reserve_threads(dict * 12 + dict * 4);
    let chunk = (SOLID_BLOCK_BYTES / u64::from(threads.max(1))).clamp(dict, dict * 4);
    let lzma2 = Lzma2Options::from_level_mt(level, threads, chunk);
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
    check_header_size(archive)?;
    let mut reader = ArchiveReader::new(BufReader::new(File::open(archive)?), password(pw)).map_err(map_err)?;
    // 解碼時每個執行緒最多需要一個字典（最大 64 MB）加上緩衝區
    let (threads, _mem) = crate::util::reserve_threads(160 << 20);
    reader.set_thread_count(threads);
    progress.start(reader.archive().files.iter().filter(|f| select(f.name())).map(|f| f.size()).fold(0u64, u64::saturating_add));

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
    check_header_size(archive)?;
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

    /// 宣告巨大目錄資料的惡意檔案要在解壓縮前就被拒絕；正常的壓縮檔不受影響
    #[test]
    fn huge_encoded_header_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        // 最小的 encoded header：一個 LZMA2 資料夾，宣告解壓縮後 1 GiB
        let header: Vec<u8> = vec![
            0x17, 0x06, 0x00, 0x01, 0x09, 0x05, 0x00, // PackInfo：位置 0、1 個資料流、大小 5
            0x07, 0x0B, 0x01, 0x00, // UnpackInfo：1 個資料夾
            0x01, 0x01, 0x21, // 1 個編碼器（LZMA2）
            0x0C, 0xF0, 0x00, 0x00, 0x00, 0x40, // 解壓縮大小 1 GiB
            0x00, 0x00,
        ];
        let mut bytes = vec![b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C, 0, 4, 0, 0, 0, 0];
        bytes.extend(0u64.to_le_bytes());
        bytes.extend((header.len() as u64).to_le_bytes());
        bytes.extend([0u8; 4]);
        bytes.extend(&header);
        let bomb = tmp.path().join("bomb.7z");
        std::fs::write(&bomb, &bytes).unwrap();
        assert_eq!(encoded_header_unpack_size(&bomb).unwrap(), 1 << 30);
        assert!(matches!(check_header_size(&bomb), Err(Error::SevenZ(_))));
        assert!(crate::list(&bomb, None).is_err());

        // 加密檔名的 7z 也使用 encoded header，大小應正常
        let src = tmp.path().join("a.txt");
        std::fs::write(&src, "x").unwrap();
        let ar = tmp.path().join("he.7z");
        let opts = CompressOptions { password: Some("pw".into()), encrypt_names: true, ..Default::default() };
        crate::compress(&[src], &ar, crate::Format::SevenZ, &opts, &crate::NoProgress).unwrap();
        let n = encoded_header_unpack_size(&ar).unwrap();
        assert!(n > 0 && n < 4096, "{n}");
        assert_eq!(crate::list(&ar, Some("pw")).unwrap().len(), 1);
    }

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
