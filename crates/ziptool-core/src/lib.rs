//! Zipo 核心：以 7-Zip 的功能為藍本，提供壓縮、解壓縮、列出內容與測試完整性。
//! CLI 與 GUI 共用此 crate。

mod format;
mod hash;
pub mod i18n;
mod iso;
mod progress;
mod sevenz;
mod single;
mod tar_fmt;
mod util;
mod volume;
mod zip_fmt;

use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub use format::Format;
pub use hash::{Hashes, hash_files, hash_reader};
pub use progress::{NoProgress, OverwriteAnswer, Progress};
pub use volume::{is_first_volume, keep_joined as keep_joined_volumes, parse_size as parse_volume_size};
pub use util::{describe, format_size, local_time, set_zip_codepage, zip_codepage};

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Zip(zip::result::ZipError),
    SevenZ(String),
    UnknownFormat(String),
    Unsupported(String),
    PasswordRequired,
    WrongPassword,
    NotEnoughSpace { need: u64, available: u64 },
    PossibleBomb { size: u64, packed: u64 },
    /// 超過 [`ExtractOptions::max_bytes`]
    TooLarge { limit: u64 },
    Cancelled,
}

// 錯誤訊息依介面語言顯示，所以手動實作（不使用固定文字的 #[error]）
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Error::Io(e) => describe(e),
            Error::Zip(e) => zip_message(e),
            Error::SevenZ(m) => F!("7z 錯誤：{m}", "7z error: {m}"),
            Error::UnknownFormat(m) => F!("無法辨識的壓縮格式：{m}", "Unrecognized archive format: {m}"),
            Error::Unsupported(m) => m.clone(),
            Error::PasswordRequired => L!("此壓縮檔需要密碼", "This archive requires a password").to_string(),
            Error::WrongPassword => L!("密碼錯誤", "Wrong password").to_string(),
            Error::NotEnoughSpace { need, available } => F!(
                "磁碟空間不足：需要 {}，目前可用 {}",
                "Not enough disk space: {} needed, {} available",
                format_size(*need),
                format_size(*available)
            ),
            Error::TooLarge { limit } => F!("解出的資料超過 {} 的上限", "The extracted data exceeds the {} limit", format_size(*limit)),
            Error::PossibleBomb { size, packed } => F!(
                "解壓縮後約 {}，是壓縮檔的 {} 倍，可能是惡意的「壓縮炸彈」",
                "Extracts to about {}, {} times the archive size; it may be a malicious \"zip bomb\"",
                format_size(*size),
                size / (*packed).max(1)
            ),
            Error::Cancelled => L!("已取消", "Cancelled").to_string(),
        };
        f.write_str(&text)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Zip(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<zip::result::ZipError> for Error {
    fn from(e: zip::result::ZipError) -> Self {
        Error::Zip(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn zip_message(e: &zip::result::ZipError) -> String {
    use zip::result::ZipError::*;
    match e {
        Io(e) => describe(e),
        InvalidArchive(_) => L!("不是有效的 ZIP 檔，或檔案已損壞", "Not a valid ZIP file, or the file is damaged").into(),
        UnsupportedArchive(m) => F!("不支援的 ZIP 功能（{m}）", "Unsupported ZIP feature ({m})"),
        FileNotFound => L!("壓縮檔中找不到指定的項目", "The specified item was not found in the archive").into(),
        InvalidPassword => L!("密碼錯誤", "Wrong password").into(),
        other => F!("ZIP 錯誤：{other}", "ZIP error: {other}"),
    }
}

/// 壓縮選項。
#[derive(Clone, Default)]
pub struct CompressOptions {
    /// 壓縮等級 0–9；`None` 表示使用各格式的預設值。
    pub level: Option<u32>,
    /// 密碼（僅 ZIP 與 7z 支援）。
    pub password: Option<String>,
    /// 加密檔名（僅 7z，需搭配密碼）；對應 7-Zip 的 `-mhe=on`。
    pub encrypt_names: bool,
    /// 分割大小（位元組）；設定時產生 `名稱.001`、`名稱.002`…（與 7-Zip 的 `-v` 相同）
    pub volume_size: Option<u64>,
}

/// 目的檔已存在時的處理方式；對應 7-Zip 的 `-aoa`／`-aos`／`-aou`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overwrite {
    #[default]
    Overwrite,
    Skip,
    /// 自動改名為「名稱 (1).副檔名」
    Rename,
    /// 每次透過 [`Progress::ask_overwrite`] 詢問
    Ask,
}

/// 解壓縮選項。
#[derive(Clone, Default)]
pub struct ExtractOptions {
    pub password: Option<String>,
    pub overwrite: Overwrite,
    /// 只解壓這些項目（壓縮檔內的完整路徑；資料夾會連同內容）；`None` 為全部。
    pub selection: Option<Vec<String>>,
    /// 從輸出路徑去掉的前置資料夾，例如在 GUI 中瀏覽到 `docs/` 時解壓選取項目。
    pub strip_prefix: String,
    /// 不保留資料夾結構（7-Zip 的 `e` 指令）。
    pub flatten: bool,
    /// 允許極高壓縮比（使用者已確認不是壓縮炸彈）。
    pub allow_high_ratio: bool,
    /// 最多寫出的位元組數（預覽、拖曳等只需要小檔案的用途）；依實際寫出的量計算，不採信壓縮檔宣告的大小
    pub max_bytes: Option<u64>,
}

/// 除錯輸出不顯示密碼本身，避免被記錄到日誌或錯誤報告中
fn redacted(pw: &Option<String>) -> &'static str {
    if pw.is_some() { L!("Some(<已隱藏>)", "Some(<hidden>)") } else { "None" }
}

impl std::fmt::Debug for CompressOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompressOptions")
            .field("level", &self.level)
            .field("password", &format_args!("{}", redacted(&self.password)))
            .field("encrypt_names", &self.encrypt_names)
            .field("volume_size", &self.volume_size)
            .finish()
    }
}

impl std::fmt::Debug for ExtractOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtractOptions")
            .field("password", &format_args!("{}", redacted(&self.password)))
            .field("overwrite", &self.overwrite)
            .field("selection", &self.selection)
            .field("strip_prefix", &self.strip_prefix)
            .field("flatten", &self.flatten)
            .field("allow_high_ratio", &self.allow_high_ratio)
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

/// 壓縮檔內的一個項目。
#[derive(Debug, Clone)]
pub struct Entry {
    /// 以 `/` 分隔的完整路徑，資料夾不含結尾的 `/`。
    pub name: String,
    pub is_dir: bool,
    /// 原始大小；單檔壓縮格式無法事先得知時為 `None`。
    pub size: Option<u64>,
    pub compressed_size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub encrypted: bool,
}

/// 被略過的項目與原因（例如權限不足、不安全的路徑、符號連結）。
#[derive(Debug, Clone)]
pub struct Issue {
    pub name: String,
    pub reason: String,
}

/// 解壓縮結果。個別項目的錯誤不會中斷整批，而是記錄在 `issues`。
#[derive(Debug, Clone, Default)]
pub struct ExtractReport {
    pub files: u64,
    pub bytes: u64,
    pub issues: Vec<Issue>,
    /// 目的地已有同名檔案而略過（使用者選擇的，不算錯誤）
    pub skipped: u64,
}

/// 測試結果。
#[derive(Debug, Clone, Default)]
pub struct TestReport {
    pub files: u64,
    pub bytes: u64,
}

/// 壓縮比超過此倍數且解壓後超過 [`BOMB_MIN_SIZE`] 時，視為可能的壓縮炸彈。
/// deflate 的理論最大壓縮比約 1032:1，門檻必須遠低於此才有防護效果；
/// 一般資料（文字、程式碼、文件）極少同時超過 100 倍與 1 GB。
pub const BOMB_RATIO: u64 = 100;
pub const BOMB_MIN_SIZE: u64 = 1 << 30;

/// 將 `inputs`（檔案或資料夾）壓縮成 `output`。
///
/// 先寫到暫存檔，全部成功後才改名為 `output`；中途失敗或取消不會留下不完整的檔案，
/// 也不會破壞已存在的同名檔。
pub fn compress(
    inputs: &[PathBuf],
    output: &Path,
    format: Format,
    opts: &CompressOptions,
    progress: &dyn Progress,
) -> Result<()> {
    if let Some(l) = opts.level
        && l > 9
    {
        return Err(Error::Unsupported(F!("壓縮等級須為 0–9，收到 {l}", "Compression level must be 0–9, got {l}")));
    }
    if format.is_read_only() {
        return Err(Error::Unsupported(F!("{} 只能讀取，無法建立", "{} is read-only and cannot be created", format.name())));
    }
    if opts.password.is_some() && !format.supports_password() {
        return Err(Error::Unsupported(F!("{} 格式不支援密碼", "The {} format does not support passwords", format.name())));
    }
    if opts.encrypt_names && (format != Format::SevenZ || opts.password.is_none()) {
        return Err(Error::Unsupported(L!("加密檔名只適用於有設定密碼的 7z", "Encrypting file names only applies to 7z archives with a password").into()));
    }
    let items = util::collect(inputs)?;
    if format.is_single() && (items.len() != 1 || items[0].is_dir) {
        return Err(Error::Unsupported(F!(
            "{} 只能壓縮單一檔案；多個檔案或資料夾請用 7z、zip 或 tar.{}", "{} can only compress a single file; use 7z, zip or tar.{} for multiple files or folders",
            format.name(),
            format.name()
        )));
    }
    let part = util::part_path(output);
    progress.start(util::total_size(&items));
    let res = match format {
        Format::Zip => zip_fmt::compress(&items, &part, opts, progress),
        Format::SevenZ => sevenz::compress(&items, &part, opts, progress),
        f if f.is_tar() => tar_fmt::compress(&items, &part, f, opts.level, progress),
        f => single::compress(&items[0], &part, f, opts.level, progress),
    }
    .and_then(|()| Ok(std::fs::rename(&part, output)?))
    .and_then(|()| match opts.volume_size {
        Some(size) => volume::split(output, size).map(drop),
        None => Ok(()),
    });
    if res.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    cancelled_or(res, progress)
}

/// 更新既有的壓縮檔：把 `add` 加到壓縮檔內的 `into` 資料夾（空字串為根目錄；同名項目會被取代），
/// 並刪除 `remove` 列出的項目（資料夾會連同內容）。對應 7-Zip 的 `u`／`d` 指令。
///
/// 與壓縮相同，先寫入暫存檔，全部成功後才取代原檔；失敗或取消時原本的壓縮檔完全不受影響。
/// zip 直接複製原有項目的壓縮資料；其他格式先解到同一磁碟的暫存資料夾再重新壓縮。
pub fn update(
    archive: &Path,
    add: &[PathBuf],
    into: &str,
    remove: &[String],
    opts: &CompressOptions,
    progress: &dyn Progress,
) -> Result<()> {
    if volume::is_first_volume(archive) {
        return Err(Error::Unsupported(L!("分割的壓縮檔無法直接修改；請先解壓縮，再重新壓縮", "Split archives cannot be modified directly; extract and compress again").into()));
    }
    let format = Format::detect(archive)?;
    if format.is_read_only() {
        return Err(Error::Unsupported(F!("{} 只能讀取，無法加入或刪除項目", "{} is read-only; items cannot be added or deleted", format.name())));
    }
    if format.is_single() {
        return Err(Error::Unsupported(F!("{} 只能包含單一檔案，無法加入或刪除項目", "{} can contain only a single file; items cannot be added or deleted", format.name())));
    }
    if opts.password.is_some() && !format.supports_password() {
        return Err(Error::Unsupported(F!("{} 格式不支援密碼", "The {} format does not support passwords", format.name())));
    }
    let into = util::normalize_name(into);
    let mut items = util::collect(add)?;
    if !into.is_empty() {
        for it in &mut items {
            it.name = format!("{into}/{}", it.name);
        }
    }
    let remove: Vec<String> = remove.iter().map(|r| util::normalize_name(r)).filter(|r| !r.is_empty()).collect();
    let new_names: Vec<String> = items.iter().map(|i| i.name.clone()).collect();
    let drop = |name: &str| {
        remove.iter().any(|r| name == r || name.starts_with(&format!("{r}/"))) || new_names.iter().any(|n| n == name)
    };

    let part = util::part_path(archive);
    // 取代檔案會遺失「從網際網路下載」的標記；之後解出的檔案就不會被 Windows 警告，所以要寫回
    let zone = util::zone_identifier(archive);
    let res = (|| -> Result<()> {
        if format == Format::Zip {
            progress.start(util::total_size(&items));
            return zip_fmt::update(archive, &part, &items, drop, opts, progress);
        }
        // 解到同一磁碟上的暫存資料夾，與新項目一起重新壓縮
        let mut tmp_name = archive.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(".ziptool-update");
        let tmp = archive.with_file_name(tmp_name);
        let _ = std::fs::remove_dir_all(&tmp);
        let r = (|| -> Result<()> {
            let x = ExtractOptions { password: opts.password.clone(), ..Default::default() };
            let report = extract(archive, &tmp, &x, progress)?;
            if let Some(i) = report.issues.first() {
                return Err(Error::Unsupported(F!("無法完整讀出原有項目 {}：{}", "Cannot fully read the existing item {}: {}", i.name, i.reason)));
            }
            let mut all = util::collect_tree(&tmp, drop)?;
            // 原有項目必須全部原樣讀出。Windows 上無法保存的名稱（只差大小寫、含 `:` 等字元、CON 等保留名稱）
            // 會被改名或互相覆蓋，重新壓縮後就會遺失，因此拒絕更新。
            let kept: std::collections::HashSet<&str> = all.iter().map(|i| i.name.as_str()).collect();
            if let Some(lost) = list(archive, opts.password.as_deref())?
                .iter()
                .map(|e| util::normalize_name(&e.name))
                .find(|n| !n.is_empty() && !drop(n) && !kept.contains(n.as_str()))
            {
                return Err(Error::Unsupported(F!(
                    "壓縮檔中的「{lost}」無法在這台電腦上原樣保存（例如名稱只差大小寫、含有不允許的字元或是保留名稱），為避免遺失資料，無法加入或刪除項目", "\"{lost}\" in the archive cannot be stored as-is on this computer (for example, names differing only in case, disallowed characters or reserved names); to avoid losing data, items cannot be added or deleted"
                )));
            }
            all.extend(items.iter().map(|i| util::Item { path: i.path.clone(), name: i.name.clone(), ..*i }));
            let mut o = opts.clone();
            // 原本加密了檔名的 7z，更新後也維持加密檔名
            if format == Format::SevenZ && o.password.is_some() && matches!(list(archive, None), Err(Error::PasswordRequired)) {
                o.encrypt_names = true;
            }
            progress.start(util::total_size(&all));
            match format {
                Format::SevenZ => sevenz::compress(&all, &part, &o, progress),
                f => tar_fmt::compress(&all, &part, f, o.level, progress),
            }
        })();
        let _ = std::fs::remove_dir_all(&tmp);
        r
    })()
    .and_then(|()| {
        if let Some(z) = &zone {
            util::set_zone_identifier(&part, z)?;
        }
        Ok(std::fs::rename(&part, archive)?)
    });
    if res.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    cancelled_or(res, progress)
}

/// 將 `archive` 解壓縮到 `dest` 資料夾。格式依副檔名或檔頭自動判斷。
///
/// 解壓前會檢查磁碟空間與壓縮比；每個檔案先寫入暫存檔再改名。
/// 單一項目的錯誤（權限、路徑、連結）會記錄在報告中並繼續處理其他項目；
/// 壓縮檔本身的錯誤（損壞、密碼錯誤）則中止並回傳錯誤。
pub fn extract(archive: &Path, dest: &Path, opts: &ExtractOptions, progress: &dyn Progress) -> Result<ExtractReport> {
    // 分割檔（名稱.001）先接回暫存的完整壓縮檔
    let joined = volume::join_if_split(archive, progress)?;
    let archive = joined.as_ref().map_or(archive, |j| j.path.as_path());
    let format = Format::detect(archive)?;
    std::fs::create_dir_all(dest)?;
    let available = fs4::available_space(dest).ok();
    preflight(archive, format, dest, opts, available)?;

    let plan = util::Planner::new(archive, dest, opts, progress, available);
    let pw = opts.password.as_deref();
    let res = match format {
        Format::Zip => zip_fmt::extract(archive, &plan, pw, progress),
        Format::Iso => iso::extract(archive, &plan, progress),
        Format::SevenZ => sevenz::extract(archive, &plan, pw, progress),
        f if f.is_tar() => tar_fmt::extract(archive, &plan, f, progress),
        f => single::extract(archive, &plan, f, progress),
    };
    cancelled_or(res, progress)?;
    Ok(plan.finish())
}

/// 依清單估算解壓後大小（只用於可快速讀取清單的 zip／7z），檢查磁碟空間與壓縮炸彈。
fn preflight(archive: &Path, format: Format, dest: &Path, opts: &ExtractOptions, available: Option<u64>) -> Result<()> {
    if !matches!(format, Format::Zip | Format::SevenZ) {
        return Ok(()); // 其他格式要完整解碼才知道大小，改由解壓時即時檢查
    }
    let entries = match list(archive, opts.password.as_deref()) {
        Ok(e) => e,
        Err(_) => return Ok(()), // 清單讀不到（例如加密檔名）時交給解壓本身處理錯誤
    };
    let plan = util::Planner::new(archive, dest, opts, &NoProgress, None);
    let need: u64 = entries.iter().filter(|e| !e.is_dir && plan.selected(&e.name)).filter_map(|e| e.size).fold(0u64, u64::saturating_add);
    check_size(need, std::fs::metadata(archive)?.len(), available, opts.allow_high_ratio)
}

/// 解壓後大小 `need`、壓縮檔大小 `packed`：檢查磁碟空間與壓縮比。
fn check_size(need: u64, packed: u64, available: Option<u64>, allow_high_ratio: bool) -> Result<()> {
    if let Some(available) = available
        && need > available
    {
        return Err(Error::NotEnoughSpace { need, available });
    }
    if !allow_high_ratio && need >= BOMB_MIN_SIZE && need / packed.max(1) >= BOMB_RATIO {
        return Err(Error::PossibleBomb { size: need, packed });
    }
    Ok(())
}

/// 列出 `archive` 的內容。
pub fn list(archive: &Path, password: Option<&str>) -> Result<Vec<Entry>> {
    let joined = volume::join_if_split(archive, &NoProgress)?;
    let archive = joined.as_ref().map_or(archive, |j| j.path.as_path());
    let mut entries = match Format::detect(archive)? {
        Format::Zip => zip_fmt::list(archive)?,
        Format::Iso => iso::list(archive)?,
        Format::SevenZ => sevenz::list(archive, password)?,
        f if f.is_tar() => tar_fmt::list(archive, f)?,
        f => single::list(archive, f)?,
    };
    for e in &mut entries {
        e.name = util::normalize_name(&e.name);
    }
    entries.retain(|e| !e.name.is_empty());
    Ok(entries)
}

/// 測試壓縮檔完整性：完整解碼每個項目並驗證 CRC，但不寫出任何檔案。
pub fn test(archive: &Path, password: Option<&str>, progress: &dyn Progress) -> Result<TestReport> {
    let joined = volume::join_if_split(archive, progress)?;
    let archive = joined.as_ref().map_or(archive, |j| j.path.as_path());
    let res = match Format::detect(archive)? {
        Format::Zip => zip_fmt::test(archive, password, progress),
        Format::Iso => iso::test(archive, progress),
        Format::SevenZ => sevenz::test(archive, password, progress),
        f if f.is_tar() => tar_fmt::test(archive, f, progress),
        f => single::test(archive, f, progress),
    };
    cancelled_or(res, progress)
}

/// 使用者已取消時，把錯誤統一換成 `Cancelled`。
fn cancelled_or<T>(res: Result<T>, progress: &dyn Progress) -> Result<T> {
    match res {
        Err(_) if progress.is_cancelled() => Err(Error::Cancelled),
        r => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;

    #[test]
    fn size_checks() {
        // 一般檔案
        assert!(check_size(GB, GB / 3, Some(10 * GB), false).is_ok());
        // 空間不足
        assert!(matches!(check_size(5 * GB, GB, Some(4 * GB), false), Err(Error::NotEnoughSpace { .. })));
        // 1 MB 展開成 1 GB：可能是壓縮炸彈；使用者確認後放行
        assert!(matches!(check_size(GB, 1 << 20, None, false), Err(Error::PossibleBomb { .. })));
        assert!(check_size(GB, 1 << 20, None, true).is_ok());
        // 壓縮比高但總量小（例如一堆空白文字檔）不打擾使用者
        assert!(check_size(100 << 20, 1 << 10, None, false).is_ok());
        // 錯誤訊息是中文且帶有大小
        let msg = Error::NotEnoughSpace { need: 5 * GB, available: GB }.to_string();
        assert!(msg.contains("5.0 GB") && msg.contains("1.0 GB"), "{msg}");
    }
}
