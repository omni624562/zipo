//! 分割壓縮檔（與 7-Zip 相同）：把壓縮檔依固定大小切成 `名稱.001`、`名稱.002`…，
//! 依序接起來就是原本的壓縮檔。讀取時先接回暫存檔再處理。

use crate::{F, L};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::{Error, Result};

/// 是否為分割檔的第一個（`名稱.001`）
pub fn is_first_volume(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "001")
}

/// `名稱.壓縮副檔名.NNN`
fn volume_path(first: &Path, n: usize) -> PathBuf {
    let base = first.with_extension("");
    let mut name = base.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{n:03}"));
    first.with_file_name(name)
}

/// 依序列出所有分割檔；中間有缺號時回報缺少哪一個
pub fn volume_paths(first: &Path) -> Result<Vec<PathBuf>> {
    let mut parts = Vec::new();
    let mut n = 1;
    while volume_path(first, n).is_file() {
        parts.push(volume_path(first, n));
        n += 1;
    }
    if parts.is_empty() {
        return Err(Error::Io(io::Error::new(io::ErrorKind::NotFound, F!("找不到 {}", "Cannot find {}", first.display()))));
    }
    // 後面還有編號較大的分割檔：代表中間缺了一個
    for later in n + 1..n + 1000 {
        if volume_path(first, later).is_file() {
            let missing = volume_path(first, n);
            return Err(Error::Unsupported(F!("缺少分割檔「{}」，請把所有分割檔放在同一個資料夾", "Missing volume \"{}\"; put all volumes in the same folder", missing.file_name().unwrap_or_default().to_string_lossy())));
        }
    }
    Ok(parts)
}

/// 接回的暫存壓縮檔，用完自動刪除
pub(crate) struct Joined {
    dir: PathBuf,
    pub path: PathBuf,
}

impl Drop for Joined {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 是否保留最近一次的接合結果（圖形介面：瀏覽、預覽、解壓縮同一組分割檔時不必每次重新接合）
static KEEP: AtomicBool = AtomicBool::new(false);

/// 保留的接合結果：第一個分割檔、各分割檔的大小與修改時間
type Stamps = Vec<(u64, Option<SystemTime>)>;
static CACHE: Mutex<Option<(PathBuf, Stamps, Arc<Joined>)>> = Mutex::new(None);

/// 開啟或關閉保留接合結果；關閉時釋放保留的暫存檔（沒有其他地方使用時立即刪除）
pub fn keep_joined(on: bool) {
    KEEP.store(on, Ordering::Relaxed);
    if !on {
        let old = CACHE.lock().unwrap_or_else(|e| e.into_inner()).take();
        drop(old);
    }
}

/// `archive` 是分割檔時，接回暫存的完整壓縮檔；否則回傳 `None`
pub(crate) fn join_if_split(archive: &Path, progress: &dyn crate::Progress) -> Result<Option<Arc<Joined>>> {
    if !is_first_volume(archive) {
        return Ok(None);
    }
    let parts = volume_paths(archive)?;
    if !KEEP.load(Ordering::Relaxed) {
        return join(archive, &parts, progress).map(|j| Some(Arc::new(j)));
    }
    let stamps: Stamps = parts.iter().map(|p| std::fs::metadata(p).map(|m| (m.len(), m.modified().ok())).unwrap_or_default()).collect();
    // 接合期間持有鎖：同時要用同一組分割檔的其他工作等這次完成後直接沿用
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((first, s, j)) = cache.as_ref()
        && first == archive
        && *s == stamps
    {
        return Ok(Some(j.clone()));
    }
    // 先釋放舊的，暫存空間不會同時被兩份佔用
    *cache = None;
    let j = Arc::new(join(archive, &parts, progress)?);
    *cache = Some((archive.to_path_buf(), stamps, j.clone()));
    Ok(Some(j))
}

/// `dir` 所在的磁碟至少要有 `need` 位元組的可用空間（無法得知時不檢查）
fn ensure_space(dir: &Path, need: u64) -> Result<()> {
    match fs4::available_space(dir) {
        Ok(available) if need > available => Err(Error::NotEnoughSpace { need, available }),
        _ => Ok(()),
    }
}

/// 接回暫存的完整壓縮檔；可以取消，失敗或取消時刪除暫存檔
fn join(archive: &Path, parts: &[PathBuf], progress: &dyn crate::Progress) -> Result<Joined> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    // Windows 的暫存資料夾每位使用者各一個，放在 Zipo 底下以便清理；Unix 的 /tmp 是共用的，
    // 其他使用者可以先建立 /tmp/Zipo 再替換其中的資料夾，所以直接放在 /tmp 並只允許自己存取
    let parent = if cfg!(windows) { std::env::temp_dir().join("Zipo") } else { std::env::temp_dir() };
    std::fs::create_dir_all(&parent)?;
    let dir = parent.join(format!("zipo-join-{}-{stamp}", std::process::id()));
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    // 不用 create_dir_all：資料夾已存在（被搶先建立）時失敗
    builder.create(&dir)?;
    let joined = Joined { path: dir.join(archive.with_extension("").file_name().unwrap_or_default()), dir };
    let need = parts.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).fold(0u64, u64::saturating_add);
    ensure_space(&joined.dir, need)?;
    let mut out = io::BufWriter::new(File::create(&joined.path)?);
    let mut buf = vec![0u8; 1 << 20];
    for p in parts {
        let mut src = File::open(p)?;
        loop {
            if progress.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
        }
    }
    out.flush()?;
    drop(out);
    // 「從網際網路下載」的標記在第一個分割檔上，一併帶到接回的檔案，解出的檔案才會有標記
    if let Some(z) = crate::util::zone_identifier(archive) {
        crate::util::set_zone_identifier(&joined.path, &z)?;
    }
    Ok(joined)
}

/// 不是壓縮檔的分割檔：接回的檔案本身就是唯一的項目（名稱為去掉 .001 的檔名，時間取第一個分割檔）
pub(crate) fn list_plain(joined: &Path, first: &Path) -> Result<Vec<crate::Entry>> {
    let size = std::fs::metadata(joined)?.len();
    Ok(vec![crate::Entry {
        name: plain_name(first),
        is_dir: false,
        size: Some(size),
        compressed_size: Some(size),
        modified: std::fs::metadata(first)?.modified().ok(),
        encrypted: false,
    }])
}

pub(crate) fn extract_plain(joined: &Path, first: &Path, plan: &crate::util::Planner, progress: &dyn crate::Progress) -> Result<()> {
    progress.start(std::fs::metadata(joined)?.len());
    let name = plain_name(first);
    let Some(out) = plan.path_for(&name, false) else { return Ok(()) };
    progress.entry(&name);
    let mut r = crate::progress::ProgressReader::new(io::BufReader::new(File::open(joined)?), progress);
    plan.write_file(&name, &out, &mut r, std::fs::metadata(first)?.modified().ok(), None)
}

pub(crate) fn test_plain(joined: &Path, progress: &dyn crate::Progress) -> Result<crate::TestReport> {
    progress.start(std::fs::metadata(joined)?.len());
    let mut r = crate::progress::ProgressReader::new(io::BufReader::new(File::open(joined)?), progress);
    let bytes = io::copy(&mut r, &mut io::sink())?;
    Ok(crate::TestReport { files: 1, bytes })
}

fn plain_name(first: &Path) -> String {
    first.with_extension("").file_name().unwrap_or_default().to_string_lossy().into_owned()
}

/// 壓縮的輸出：一般檔案，或直接寫入一組分割檔（不必先產生完整的壓縮檔）
pub(crate) enum OutFile {
    Plain(File),
    Volumes(VolumeWriter),
}

impl OutFile {
    /// `part`：一般輸出時的暫存檔；`output`：正式的壓縮檔名稱（分割檔依此命名）
    pub(crate) fn create(part: &Path, output: &Path, volume: Option<u64>) -> Result<Self> {
        match volume {
            None => Ok(OutFile::Plain(File::create(part)?)),
            Some(0) => Err(Error::Unsupported(L!("分割大小不可為 0", "The volume size cannot be 0").into())),
            Some(size) => Ok(OutFile::Volumes(VolumeWriter::new(output, size)?)),
        }
    }
}

impl Write for OutFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            OutFile::Plain(f) => f.write(buf),
            OutFile::Volumes(v) => v.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            OutFile::Plain(f) => f.flush(),
            OutFile::Volumes(v) => v.flush(),
        }
    }
}

impl io::Seek for OutFile {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        match self {
            OutFile::Plain(f) => f.seek(pos),
            OutFile::Volumes(v) => v.seek(pos),
        }
    }
}

/// 依序寫入 名稱.001、名稱.002…（每個 `size` 位元組），也能移回前面的位置改寫（zip、7z 會回頭更新標頭）。
/// 寫入時使用暫存名稱，壓縮成功後由 [`finish_volumes`] 一次改成正式名稱。
/// 同時只開啟一個分割檔，分割得再細也不會開啟大量檔案。
pub(crate) struct VolumeWriter {
    first: PathBuf,
    size: u64,
    pos: u64,
    len: u64,
    /// 已建立的分割檔數量
    created: u64,
    /// 目前開啟的分割檔：（索引, 檔案, 檔案內的位置）
    current: Option<(u64, File, u64)>,
}

impl VolumeWriter {
    fn new(output: &Path, size: u64) -> Result<Self> {
        let mut w = VolumeWriter { first: first_volume(output), size, pos: 0, len: 0, created: 0, current: None };
        // 空的壓縮檔也至少有一個分割檔
        w.open(0)?;
        Ok(w)
    }

    fn open(&mut self, idx: u64) -> io::Result<()> {
        if self.current.as_ref().is_some_and(|(i, _, _)| *i == idx) {
            return Ok(());
        }
        self.current = None;
        while self.created <= idx {
            File::create(tmp_volume(&volume_path(&self.first, self.created as usize + 1)))?;
            self.created += 1;
        }
        let f = std::fs::OpenOptions::new().write(true).open(tmp_volume(&volume_path(&self.first, idx as usize + 1)))?;
        self.current = Some((idx, f, 0));
        Ok(())
    }
}

impl Write for VolumeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let (idx, off) = (self.pos / self.size, self.pos % self.size);
        self.open(idx)?;
        let (_, f, at) = self.current.as_mut().expect("已開啟");
        if *at != off {
            io::Seek::seek(f, io::SeekFrom::Start(off))?;
        }
        let n = f.write(&buf[..buf.len().min((self.size - off) as usize)])?;
        *at = off + n as u64;
        self.pos += n as u64;
        self.len = self.len.max(self.pos);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        match &mut self.current {
            Some((_, f, _)) => f.flush(),
            None => Ok(()),
        }
    }
}

impl io::Seek for VolumeWriter {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        let to = match pos {
            io::SeekFrom::Start(p) => Some(p),
            io::SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            io::SeekFrom::End(d) => self.len.checked_add_signed(d),
        };
        self.pos = to.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        Ok(self.pos)
    }
}

fn first_volume(output: &Path) -> PathBuf {
    let mut name = output.file_name().unwrap_or_default().to_os_string();
    name.push(".001");
    output.with_file_name(name)
}

/// 寫入中的分割檔名稱
fn tmp_volume(p: &Path) -> PathBuf {
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(".zipo-part");
    p.with_file_name(name)
}

/// 壓縮成功：把暫存名稱的分割檔全部改成正式名稱，回傳所有分割檔
pub(crate) fn finish_volumes(output: &Path) -> Result<Vec<PathBuf>> {
    let first = first_volume(output);
    let parts: Vec<PathBuf> = (1..).map(|n| volume_path(&first, n)).take_while(|p| tmp_volume(p).is_file()).collect();
    for p in &parts {
        std::fs::rename(tmp_volume(p), p)?;
    }
    Ok(parts)
}

/// 壓縮失敗或取消：刪除寫到一半的分割檔
pub(crate) fn discard_volumes(output: &Path) {
    let first = first_volume(output);
    for p in (1..).map(|n| volume_path(&first, n)).take_while(|p| tmp_volume(p).is_file()) {
        let _ = std::fs::remove_file(tmp_volume(&p));
    }
}

/// 把 `path` 切成每個 `size` 位元組的分割檔，完成後刪除原檔；回傳所有分割檔。
/// 回報進度、可以取消；分割檔先以暫存名稱寫入，全部完成才改成正式名稱，
/// 中途失敗、取消或程式被結束時不會留下看似完整、其實缺檔的分割檔。
#[cfg(test)]
pub(crate) fn split(path: &Path, size: u64, progress: &dyn crate::Progress) -> Result<Vec<PathBuf>> {
    if size == 0 {
        return Err(Error::Unsupported(L!("分割大小不可為 0", "The volume size cannot be 0").into()));
    }
    let first = {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".001");
        path.with_file_name(name)
    };
    let tmp_of = |p: &Path| {
        let mut name = p.file_name().unwrap_or_default().to_os_string();
        name.push(".zipo-part");
        p.with_file_name(name)
    };
    let mut parts = Vec::new();
    let res = (|| -> Result<()> {
        let mut src = File::open(path)?;
        let total = std::fs::metadata(path)?.len();
        progress.start(total);
        let mut buf = vec![0u8; 1 << 20];
        let mut written = 0u64;
        let mut n = 1;
        // 空的壓縮檔也至少產生一個分割檔
        while n == 1 || written < total {
            let part = volume_path(&first, n);
            progress.entry(&part.file_name().unwrap_or_default().to_string_lossy());
            let mut out = File::create(tmp_of(&part))?;
            parts.push(part);
            let mut left = size.min(total - written);
            while left > 0 {
                if progress.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let want = left.min(buf.len() as u64) as usize;
                let n = src.read(&mut buf[..want])?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
                left -= n as u64;
                written += n as u64;
                progress.advance(n as u64);
            }
            n += 1;
            if written >= total {
                break;
            }
        }
        // 全部寫完才改成正式名稱
        for p in &parts {
            std::fs::rename(tmp_of(p), p)?;
        }
        Ok(())
    })();
    match res {
        Ok(()) => {
            std::fs::remove_file(path)?;
            Ok(parts)
        }
        Err(e) => {
            for p in &parts {
                let _ = std::fs::remove_file(tmp_of(p));
                let _ = std::fs::remove_file(p);
            }
            Err(e)
        }
    }
}

/// 解析分割大小，例如 `100m`、`700M`、`4g`、`64k`、`1000`（位元組）
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => (&s[..i], &s[i..]),
        None => (s.as_str(), ""),
    };
    let n: u64 = num.parse().ok()?;
    let mul: u64 = match unit {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        _ => return None,
    };
    n.checked_mul(mul).filter(|&v| v > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoProgress;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("100m"), Some(100 << 20));
        assert_eq!(parse_size("4G"), Some(4 << 30));
        assert_eq!(parse_size("64k"), Some(64 << 10));
        assert_eq!(parse_size("1000"), Some(1000));
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size("10x"), None);
        assert_eq!(parse_size(""), None);
    }

    /// 接合期間可以取消，取消後不留下暫存檔；暫存磁碟空間不足時先回報，不開始接合
    #[test]
    fn join_can_be_cancelled_and_checks_space() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        struct Cancel;
        impl crate::Progress for Cancel {
            fn is_cancelled(&self) -> bool {
                true
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("c.zip");
        std::fs::write(&f, vec![1u8; 30_000]).unwrap();
        let parts = split(&f, 10_000, &NoProgress).unwrap();
        let mine = || -> Vec<String> {
            let parent = if cfg!(windows) { std::env::temp_dir().join("Zipo") } else { std::env::temp_dir() };
            let prefix = format!("zipo-join-{}-", std::process::id());
            std::fs::read_dir(parent).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with(&prefix)).collect()).unwrap_or_default()
        };
        let before = mine();
        assert!(matches!(join_if_split(&parts[0], &Cancel), Err(Error::Cancelled)));
        assert_eq!(mine(), before, "取消後不應留下暫存檔");
        assert!(matches!(ensure_space(tmp.path(), u64::MAX), Err(Error::NotEnoughSpace { .. })));
        assert!(ensure_space(tmp.path(), 1).is_ok());
    }

    /// 分割途中取消：不留下任何分割檔（也不留下暫存的分割檔），原檔保留；進度依分割的量回報
    #[test]
    fn split_reports_progress_and_can_be_cancelled() {
        use std::sync::atomic::{AtomicU64, Ordering};
        struct P {
            done: AtomicU64,
            cancel_after: u64,
            /// 分割途中看到的正式名稱分割檔（程式在這時被結束，就會留下看似完整的分割檔）
            first: PathBuf,
            seen_early: std::sync::atomic::AtomicBool,
        }
        impl crate::Progress for P {
            fn advance(&self, n: u64) {
                self.done.fetch_add(n, Ordering::Relaxed);
                if self.first.exists() {
                    self.seen_early.store(true, Ordering::Relaxed);
                }
            }
            fn is_cancelled(&self) -> bool {
                self.done.load(Ordering::Relaxed) >= self.cancel_after
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("p.zip");
        std::fs::write(&f, vec![3u8; 3_500_000]).unwrap();
        let first = tmp.path().join("p.zip.001");
        let all = P { done: AtomicU64::new(0), cancel_after: u64::MAX, first: first.clone(), seen_early: Default::default() };
        let parts = split(&f, 1_000_000, &all).unwrap();
        assert_eq!(parts.len(), 4);
        assert!(!all.seen_early.load(Ordering::Relaxed), "全部完成前不可出現正式名稱的分割檔");
        assert_eq!(all.done.load(Ordering::Relaxed), 3_500_000, "進度依分割的量回報");

        std::fs::write(&f, vec![3u8; 3_500_000]).unwrap();
        for p in &parts {
            std::fs::remove_file(p).unwrap();
        }
        let stop = P { done: AtomicU64::new(0), cancel_after: 1_500_000, first, seen_early: Default::default() };
        assert!(matches!(split(&f, 1_000_000, &stop), Err(Error::Cancelled)));
        let left: Vec<String> = std::fs::read_dir(tmp.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["p.zip"], "取消後只剩原檔：{left:?}");
    }

    /// 直接寫入分割檔：可以移回前面改寫（跨越兩個分割檔也正確），完成後才出現正式名稱
    #[test]
    fn volume_writer_handles_seek_back_across_volumes() {
        use std::io::Seek;
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("w.zip");
        let mut w = OutFile::create(&tmp.path().join("w.zip.part"), &out, Some(100)).unwrap();
        let data: Vec<u8> = (0..250u32).map(|i| i as u8).collect();
        w.write_all(&data).unwrap();
        // 回頭改寫第 50～149 位元組（橫跨 .001 與 .002）
        w.seek(io::SeekFrom::Start(50)).unwrap();
        w.write_all(&[0xAA; 100]).unwrap();
        assert_eq!(w.seek(io::SeekFrom::End(0)).unwrap(), 250);
        w.write_all(&[1, 2, 3]).unwrap();
        w.flush().unwrap();
        drop(w);
        assert!(!tmp.path().join("w.zip.001").exists(), "完成前不可出現正式名稱");
        let parts = finish_volumes(&out).unwrap();
        assert_eq!(parts.len(), 3);
        let mut joined = Vec::new();
        for p in &parts {
            joined.extend(std::fs::read(p).unwrap());
        }
        let mut want = data.clone();
        want[50..150].fill(0xAA);
        want.extend([1, 2, 3]);
        assert_eq!(joined, want);
        assert_eq!(std::fs::metadata(&parts[0]).unwrap().len(), 100);

        // 失敗時刪除寫到一半的分割檔
        let mut w = OutFile::create(&tmp.path().join("x.part"), &tmp.path().join("x.zip"), Some(100)).unwrap();
        w.write_all(&[0; 150]).unwrap();
        drop(w);
        discard_volumes(&tmp.path().join("x.zip"));
        let left: Vec<String> = std::fs::read_dir(tmp.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("x.")).collect();
        assert!(left.is_empty(), "{left:?}");
    }

    /// 兩個測試都會改變全域的「保留接合結果」設定，依序執行
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 圖形介面保留接合結果：分割檔沒有變動時沿用，不重新接合；有變動或關閉保留時重新接合並刪除舊的
    #[test]
    fn joined_archive_is_reused_until_volumes_change() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("r.zip");
        std::fs::write(&f, vec![7u8; 30_000]).unwrap();
        let parts = split(&f, 10_000, &NoProgress).unwrap();
        keep_joined(true);
        let a = join_if_split(&parts[0], &NoProgress).unwrap().unwrap();
        let b = join_if_split(&parts[0], &NoProgress).unwrap().unwrap();
        assert!(std::sync::Arc::ptr_eq(&a, &b), "沒有變動時應沿用");
        let first_dir = a.dir.clone();
        drop((a, b));
        assert!(first_dir.exists(), "保留中不可刪除");
        // 分割檔變動（內容與時間）：重新接合，舊的刪除
        std::fs::write(&parts[2], vec![8u8; 9_000]).unwrap();
        let c = join_if_split(&parts[0], &NoProgress).unwrap().unwrap();
        assert_ne!(c.dir, first_dir);
        assert_eq!(std::fs::metadata(&c.path).unwrap().len(), 29_000);
        assert!(!first_dir.exists(), "舊的接合結果應刪除");
        let dir = c.dir.clone();
        drop(c);
        keep_joined(false);
        assert!(!dir.exists(), "關閉保留後應刪除");
    }

    #[test]
    fn split_and_join() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.zip");
        let data: Vec<u8> = (0..250_000u32).map(|i| (i * 31 % 251) as u8).collect();
        std::fs::write(&f, &data).unwrap();
        let parts = split(&f, 100_000, &NoProgress).unwrap();
        let names: Vec<String> = parts.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["a.zip.001", "a.zip.002", "a.zip.003"]);
        assert!(!f.exists(), "原檔應刪除");
        assert_eq!(std::fs::metadata(&parts[2]).unwrap().len(), 50_000);
        let joined = join_if_split(&parts[0], &NoProgress).unwrap().unwrap();
        assert_eq!(joined.path.file_name().unwrap(), "a.zip");
        assert_eq!(std::fs::read(&joined.path).unwrap(), data);
        let dir = joined.dir.clone();
        // Unix：不放在共用的 /tmp/Zipo 底下，且只有自己能存取
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(dir.parent().unwrap(), std::env::temp_dir());
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        drop(joined);
        assert!(!dir.exists(), "暫存檔應刪除");

        // 中間缺一個：說明缺少哪一個
        std::fs::remove_file(&parts[1]).unwrap();
        let e = join_if_split(&parts[0], &NoProgress).err().unwrap().to_string();
        assert!(e.contains("a.zip.002"), "{e}");

        // 不是分割檔：不處理
        assert!(join_if_split(&tmp.path().join("x.zip"), &NoProgress).unwrap().is_none());
    }
}
