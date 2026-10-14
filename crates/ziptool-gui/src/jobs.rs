//! 背景工作：壓縮、解壓縮、測試、開啟壓縮檔內的檔案。

use ziptool_core::{F, L};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use ziptool_core::{CompressOptions, Error, ExtractOptions, Format, Issue, OverwriteAnswer, Progress};

use crate::human;

/// 壓縮或解壓縮完成後的動作
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Then {
    /// 壓縮：原始檔移到資源回收筒（先測試壓縮檔無誤）；解壓縮：壓縮檔移到資源回收筒（全部項目都解出時）
    pub recycle: bool,
    /// 完成後開啟所在資料夾
    pub open_folder: bool,
}

#[derive(Clone)]
pub enum Task {
    Compress { inputs: Vec<PathBuf>, output: PathBuf, format: Format, opts: CompressOptions, then: Then },
    /// 多個壓縮檔時，每個解到 `dest/<名稱>`；單一壓縮檔直接解到 `dest`。
    /// `smart`：壓縮檔內只有一個最上層資料夾時，不再多包一層同名資料夾。
    /// `here`：每個壓縮檔直接解到它所在的資料夾（右鍵選單「解壓縮到這裡」，`dest` 不使用）；
    /// 否則多個壓縮檔各自解到 `dest` 底下的同名資料夾
    Extract { archives: Vec<PathBuf>, dest: PathBuf, opts: ExtractOptions, smart: bool, here: bool, then: Then },
    Test { archives: Vec<PathBuf>, password: Option<String> },
    /// 更新既有壓縮檔：加入檔案到 `into` 資料夾、刪除 `remove` 中的項目。
    Update { archive: PathBuf, add: Vec<PathBuf>, into: String, remove: Vec<String>, password: Option<String> },
    /// 壓縮檔內重新命名：`from`、`to` 為壓縮檔內的完整名稱（資料夾連同其中的內容）
    Rename { archive: PathBuf, from: String, to: String, password: Option<String> },
    /// 在壓縮檔內新增空的資料夾；`path` 為壓縮檔內的完整名稱
    NewFolder { archive: PathBuf, path: String, password: Option<String> },
    /// 把壓縮檔內的單一檔案解到暫存資料夾後，用系統預設程式開啟。
    OpenInside { archive: PathBuf, entry: String, strip: String, password: Option<String> },
    /// 拖曳大型項目前，先在背景解到暫存資料夾（顯示進度），完成後再拖曳一次即可
    PrepareDrag { archive: PathBuf, entries: Vec<String>, strip: String, password: Option<String> },
    /// 把壓縮檔內的壓縮檔解到暫存資料夾，以便直接瀏覽
    OpenNested { archive: PathBuf, entry: String, strip: String, password: Option<String> },
    /// 計算檔案（資料夾則為其中每個檔案）的雜湊值
    Hash { inputs: Vec<PathBuf> },
    /// 計算壓縮檔內項目的雜湊值：先解到暫存資料夾
    HashInside { archive: PathBuf, entries: Vec<String>, strip: String, password: Option<String> },
}

impl Task {
    pub fn title(&self) -> &'static str {
        match self {
            Task::Compress { .. } => L!("正在壓縮", "Compressing"),
            Task::Extract { .. } => L!("正在解壓縮", "Extracting"),
            Task::Test { .. } => L!("正在測試", "Testing"),
            Task::OpenInside { .. } | Task::OpenNested { .. } => L!("正在開啟", "Opening"),
            Task::PrepareDrag { .. } => L!("正在準備拖曳", "Preparing to drag"),
            Task::Update { .. } => L!("正在更新壓縮檔", "Updating archive"),
            Task::Rename { .. } => L!("正在重新命名", "Renaming"),
            Task::NewFolder { .. } => L!("正在新增資料夾", "Creating folder"),
            Task::Hash { .. } | Task::HashInside { .. } => L!("正在計算雜湊值", "Calculating hashes"),
        }
    }

    pub fn set_password(&mut self, pw: String) {
        match self {
            Task::Extract { opts, .. } => opts.password = Some(pw),
            Task::Test { password, .. }
            | Task::OpenInside { password, .. }
            | Task::Update { password, .. }
            | Task::Rename { password, .. }
            | Task::NewFolder { password, .. }
            | Task::HashInside { password, .. }
            | Task::OpenNested { password, .. }
            | Task::PrepareDrag { password, .. } => *password = Some(pw),
            Task::Compress { .. } | Task::Hash { .. } => {}
        }
    }

    /// 使用者確認高壓縮比的檔案不是壓縮炸彈後，允許繼續。
    pub fn allow_high_ratio(&mut self) {
        if let Task::Extract { opts, .. } = self {
            opts.allow_high_ratio = true;
        }
    }
}

#[derive(Default)]
pub struct Outcome {
    pub title: String,
    pub message: String,
    pub open_dir: Option<PathBuf>,
    /// 完成後需要重新整理目前的清單
    pub refresh: bool,
    /// 被略過的項目（有的話以警告樣式顯示）
    pub issues: Vec<Issue>,
    /// 雜湊值計算的結果（有的話顯示雜湊值對話框）
    pub hashes: Vec<(String, ziptool_core::Hashes)>,
    /// 完成後自動開啟 `open_dir`
    pub auto_open: bool,
    /// 開啟壓縮檔內的檔案時，實際解出並開啟的檔案（用來偵測修改後存回）
    pub opened: Option<PathBuf>,
    /// 準備拖曳時解出的最上層項目
    pub prepared: Vec<PathBuf>,
}

/// 取得鎖；即使另一個執行緒在持有鎖時 panic，也繼續使用資料而不連鎖 panic。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 背景工作與 UI 共享的進度狀態。
pub struct Shared {
    total: AtomicU64,
    done: AtomicU64,
    cancel: AtomicBool,
    current: Mutex<String>,
    stage: Mutex<String>,
    started: Mutex<Instant>,
    /// 等待使用者回答的同名檔問題
    question: Mutex<Option<PathBuf>>,
    answer: Mutex<Option<OverwriteAnswer>>,
    answered: Condvar,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            total: 0.into(),
            done: 0.into(),
            cancel: false.into(),
            current: Mutex::default(),
            stage: Mutex::default(),
            started: Mutex::new(Instant::now()),
            question: Mutex::default(),
            answer: Mutex::default(),
            answered: Condvar::new(),
        }
    }
}

impl Progress for Shared {
    fn start(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
        self.done.store(0, Ordering::Relaxed);
        *lock(&self.started) = Instant::now();
    }
    fn advance(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }
    fn entry(&self, name: &str) {
        *lock(&self.current) = name.to_string();
    }
    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    /// 在背景執行緒中等待 UI 回答；使用者按取消時也會結束等待。
    fn ask_overwrite(&self, path: &Path) -> OverwriteAnswer {
        *lock(&self.answer) = None;
        *lock(&self.question) = Some(path.to_path_buf());
        let mut ans = lock(&self.answer);
        let result = loop {
            if let Some(a) = ans.take() {
                break a;
            }
            if self.is_cancelled() {
                break OverwriteAnswer::Cancel;
            }
            ans = self.answered.wait_timeout(ans, Duration::from_millis(100)).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
        };
        drop(ans);
        *lock(&self.question) = None;
        result
    }
}

/// 進度視窗顯示用的快照。
pub struct Snapshot {
    pub total: u64,
    pub done: u64,
    pub current: String,
    pub stage: String,
    pub elapsed: Duration,
    pub speed: f64,
    pub remaining: Option<Duration>,
    pub cancelling: bool,
    pub question: Option<PathBuf>,
}

pub struct Job {
    pub task: Task,
    shared: Arc<Shared>,
    handle: JoinHandle<Result<Outcome, Error>>,
    began: Instant,
}

impl Job {
    pub fn spawn(task: Task) -> Self {
        let shared = Arc::new(Shared::default());
        let (t, s) = (task.clone(), shared.clone());
        let handle = std::thread::spawn(move || run(t, &s));
        Job { task, shared, handle, began: Instant::now() }
    }

    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
        self.shared.answered.notify_all();
    }

    pub fn answer(&self, a: OverwriteAnswer) {
        *lock(&self.shared.answer) = Some(a);
        self.shared.answered.notify_all();
    }

    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    pub fn join(self) -> (Task, Result<Outcome, Error>) {
        let r = self.handle.join().unwrap_or_else(|_| Err(Error::Unsupported(L!("背景工作異常中止，請再試一次", "The background task stopped unexpectedly; please try again").into())));
        (self.task, r)
    }

    pub fn snapshot(&self) -> Snapshot {
        let s = &self.shared;
        let total = s.total.load(Ordering::Relaxed);
        let done = s.done.load(Ordering::Relaxed).min(total.max(1));
        let since = lock(&s.started).elapsed().as_secs_f64();
        let speed = if since > 0.2 { done as f64 / since } else { 0.0 };
        let remaining = (speed > 0.0 && total > done).then(|| Duration::from_secs_f64((total - done) as f64 / speed));
        Snapshot {
            total,
            done,
            current: lock(&s.current).clone(),
            stage: lock(&s.stage).clone(),
            elapsed: self.began.elapsed(),
            speed,
            remaining,
            cancelling: s.is_cancelled(),
            question: lock(&s.question).clone(),
        }
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

fn archive_stem(ar: &Path) -> String {
    let name = file_name(ar);
    Format::split_extension(&name).map(|(s, _)| s.to_string()).filter(|s| !s.is_empty()).unwrap_or(name)
}

/// 智慧解壓：要解到「壓縮檔同名資料夾」，而壓縮檔內剛好只有一個最上層資料夾時，
/// 直接解到上一層，避免出現 `專案\專案\...` 這種重複的資料夾。
/// 只對能快速讀取清單的 zip／7z 判斷；其他格式要完整解碼才知道內容，維持原樣。
pub fn smart_dest(ar: &Path, out: PathBuf, opts: &ExtractOptions) -> PathBuf {
    if opts.flatten || opts.selection.is_some() || out.file_name().map(|n| n.to_string_lossy().into_owned()) != Some(archive_stem(ar)) {
        return out;
    }
    if !matches!(Format::detect(ar), Ok(Format::Zip | Format::SevenZ)) {
        return out;
    }
    let Ok(entries) = ziptool_core::list(ar, opts.password.as_deref()) else { return out };
    let mut tops = entries.iter().map(|e| e.name.split('/').next().unwrap_or(""));
    let Some(first) = tops.next() else { return out };
    let single = tops.all(|t| t == first);
    let is_folder = entries.iter().any(|e| e.name.starts_with(&format!("{first}/")) || (e.name == first && e.is_dir));
    match out.parent() {
        Some(parent) if single && is_folder => parent.to_path_buf(),
        _ => out,
    }
}

fn stage(s: &Shared, i: usize, n: usize, ar: &Path) {
    *lock(&s.stage) = if n > 1 { F!("第 {} / {n} 個：{}", "{} of {n}: {}", i + 1, file_name(ar)) } else { file_name(ar) };
}

fn run(task: Task, s: &Shared) -> Result<Outcome, Error> {
    let started = Instant::now();
    match task {
        Task::Compress { inputs, output, format, opts, then } => {
            ziptool_core::compress(&inputs, &output, format, &opts, s)?;
            // 分割時原檔已切成 名稱.001、名稱.002…
            let parts: Vec<PathBuf> = if opts.volume_size.is_some() {
                (1..).map(|n| PathBuf::from(format!("{}.{n:03}", output.display()))).take_while(|p| p.is_file()).collect()
            } else {
                vec![output.clone()]
            };
            let size: u64 = parts.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
            // 分割時進度改以壓縮檔大小計算，原始大小要另外算
            let total: u64 = if opts.volume_size.is_some() { crate::dialogs::disk_size(&inputs) } else { s.total.load(Ordering::Relaxed) };
            let ratio = if total > 0 { size as f64 * 100.0 / total as f64 } else { 0.0 };
            let mut message = F!(
                "{}\n\n原始大小：{}\n壓縮後：{}（{ratio:.0}%）\n耗時：{:.1} 秒", "{}\n\nOriginal size: {}\nCompressed: {} ({ratio:.0}%)\nTime: {:.1} s",
                parts.first().unwrap_or(&output).display(),
                human(total),
                human(size),
                started.elapsed().as_secs_f32()
            );
            if parts.len() > 1 || opts.volume_size.is_some() {
                message += &F!("\n分割為 {} 個檔案；解壓縮時開啟 .001 即可", "\nSplit into {} files; open the .001 file to extract", parts.len());
            }
            if then.recycle {
                // 先確認壓縮檔完整，才移除原始檔
                *lock(&s.stage) = L!("正在檢查壓縮檔", "Testing the archive").into();
                message += &match ziptool_core::test(parts.first().unwrap_or(&output), opts.password.as_deref(), &ziptool_core::NoProgress) {
                    Ok(_) => match send_to_recycle_bin(&inputs) {
                        Ok(()) => L!("\n\n原始檔已移到資源回收筒。", "\n\nThe original files were moved to the Recycle Bin.").to_string(),
                        Err(e) => F!("\n\n原始檔無法移到資源回收筒：{e}", "\n\nThe original files could not be moved to the Recycle Bin: {e}"),
                    },
                    Err(e) => F!("\n\n壓縮檔檢查失敗（{e}），原始檔保留不動。", "\n\nThe archive test failed ({e}); the original files were kept."),
                };
            }
            Ok(Outcome {
                title: L!("壓縮完成", "Compression complete").into(),
                message,
                open_dir: output.parent().map(Path::to_path_buf),
                refresh: true,
                auto_open: then.open_folder,
                ..Default::default()
            })
        }
        Task::Extract { archives, dest, opts, smart, here, then } => {
            let n = archives.len();
            let (mut files, mut bytes, mut issues, mut skipped) = (0, 0, Vec::new(), 0);
            let mut shown = dest.clone();
            for (i, ar) in archives.iter().enumerate() {
                stage(s, i, n, ar);
                let mut out = match ar.parent() {
                    Some(p) if here => p.to_path_buf(),
                    _ if n > 1 => dest.join(archive_stem(ar)),
                    _ => dest.clone(),
                };
                if smart {
                    out = smart_dest(ar, out, &opts);
                }
                if n == 1 {
                    shown = out.clone();
                }
                let r = ziptool_core::extract(ar, &out, &opts, s)?;
                files += r.files;
                bytes += r.bytes;
                issues.extend(r.issues);
                skipped += r.skipped;
            }
            let title = if issues.is_empty() { L!("解壓縮完成", "Extraction complete") } else { L!("解壓縮完成，但有項目被略過", "Extraction complete, but some items were skipped") };
            let mut message = F!(
                "已解壓縮到：\n{}\n\n檔案：{files} 個（{}）\n耗時：{:.1} 秒", "Extracted to:\n{}\n\nFiles: {files} ({})\nTime: {:.1} s",
                shown.display(),
                human(bytes),
                started.elapsed().as_secs_f32()
            );
            // 只解出部分項目、有項目被略過或保留了目的地原有的檔案時，壓縮檔保留不動
            if then.recycle {
                message += &if !issues.is_empty() || skipped > 0 || opts.selection.is_some() {
                    L!("\n\n有項目未解出，壓縮檔保留不動。", "\n\nSome items were not extracted; the archive was kept.").to_string()
                } else {
                    // 分割壓縮檔：整組分割檔一起移除
                    let all: Vec<PathBuf> = archives
                        .iter()
                        .flat_map(|a| if ziptool_core::is_first_volume(a) { ziptool_core::volume_paths(a).unwrap_or_else(|_| vec![a.clone()]) } else { vec![a.clone()] })
                        .collect();
                    match send_to_recycle_bin(&all) {
                        Ok(()) => L!("\n\n壓縮檔已移到資源回收筒。", "\n\nThe archive was moved to the Recycle Bin.").to_string(),
                        Err(e) => F!("\n\n壓縮檔無法移到資源回收筒：{e}", "\n\nThe archive could not be moved to the Recycle Bin: {e}"),
                    }
                };
            }
            Ok(Outcome {
                title: title.into(),
                message,
                open_dir: Some(shown),
                refresh: true,
                issues,
                auto_open: then.open_folder,
                ..Default::default()
            })
        }
        Task::Test { archives, password } => {
            let n = archives.len();
            let (mut files, mut bytes) = (0, 0);
            for (i, ar) in archives.iter().enumerate() {
                stage(s, i, n, ar);
                let r = ziptool_core::test(ar, password.as_deref(), s)?;
                files += r.files;
                bytes += r.bytes;
            }
            Ok(Outcome {
                title: L!("測試完成", "Test complete").into(),
                message: F!("沒有發現錯誤。\n\n壓縮檔：{n} 個\n檔案：{files} 個\n大小：{}", "No errors found.\n\nArchives: {n}\nFiles: {files}\nSize: {}", human(bytes)),
                open_dir: None,
                refresh: false,
                issues: Vec::new(),
                ..Default::default()
            })
        }
        Task::Update { archive, add, into, remove, password } => {
            *lock(&s.stage) = file_name(&archive);
            let opts = CompressOptions { password, ..Default::default() };
            ziptool_core::update(&archive, &add, &into, &remove, &opts, s)?;
            let what = match (add.len(), remove.len()) {
                (a, 0) => F!("已加入 {a} 個項目", "Added {a} item(s)"),
                (0, r) => F!("已刪除 {r} 個項目", "Deleted {r} item(s)"),
                (a, r) => F!("已加入 {a} 個、刪除 {r} 個項目", "Added {a} and deleted {r} item(s)"),
            };
            Ok(Outcome {
                title: L!("已更新壓縮檔", "Archive updated").into(),
                message: F!("{}\n\n{what}\n耗時：{:.1} 秒", "{}\n\n{what}\nTime: {:.1} s", archive.display(), started.elapsed().as_secs_f32()),
                open_dir: None,
                refresh: true,
                issues: Vec::new(),
                ..Default::default()
            })
        }
        Task::Rename { archive, from, to, password } => {
            *lock(&s.stage) = file_name(&archive);
            let opts = CompressOptions { password, ..Default::default() };
            ziptool_core::rename(&archive, &from, &to, &opts, s)?;
            // 與檔案總管相同：改好名稱就好，不另外通知
            Ok(Outcome { refresh: true, ..Default::default() })
        }
        Task::NewFolder { archive, path, password } => {
            *lock(&s.stage) = file_name(&archive);
            // 先在暫存資料夾建立同名的空資料夾，再加入壓縮檔
            let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let tmp = std::env::temp_dir().join(format!("zipo-newfolder-{stamp}"));
            let (into, leaf) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
            let dir = tmp.join(leaf);
            let res = std::fs::create_dir_all(&dir).map_err(Error::from).and_then(|()| {
                let opts = CompressOptions { password, ..Default::default() };
                ziptool_core::update(&archive, &[dir], into, &[], &opts, s)
            });
            let _ = std::fs::remove_dir_all(&tmp);
            res?;
            Ok(Outcome { refresh: true, ..Default::default() })
        }
        Task::Hash { inputs } => Ok(Outcome { hashes: ziptool_core::hash_files(&inputs, s)?, ..Default::default() }),
        Task::HashInside { archive, entries, strip, password } => {
            let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let dir = std::env::temp_dir().join("Zipo").join(format!("{}-hash-{stamp}", std::process::id()));
            let r = (|| {
                let opts = ExtractOptions { password, selection: Some(entries), strip_prefix: strip, ..Default::default() };
                let r = ziptool_core::extract(&archive, &dir, &opts, s)?;
                if let Some(i) = r.issues.first() {
                    return Err(Error::Unsupported(F!("無法讀取 {}：{}", "Cannot read {}: {}", i.name, i.reason)));
                }
                let mut top: Vec<PathBuf> = std::fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
                top.sort();
                ziptool_core::hash_files(&top, s)
            })();
            let _ = std::fs::remove_dir_all(&dir);
            Ok(Outcome { hashes: r?, ..Default::default() })
        }
        Task::PrepareDrag { archive, entries, strip, password } => {
            let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let dir = std::env::temp_dir().join("Zipo").join(format!("{}-drag-{stamp}", std::process::id()));
            let opts = ExtractOptions { password, selection: Some(entries), strip_prefix: strip, ..Default::default() };
            let r = ziptool_core::extract(&archive, &dir, &opts, s);
            let r = r.and_then(|r| match r.issues.first() {
                Some(i) => Err(Error::Unsupported(F!("無法拖曳 {}：{}", "Cannot drag {}: {}", i.name, i.reason))),
                None => Ok(()),
            });
            if let Err(e) = r {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
            let prepared: Vec<PathBuf> = std::fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
            // 不跳出對話框（會擋住拖曳）：由畫面重新選取這些項目並在狀態列提示
            Ok(Outcome { prepared, ..Default::default() })
        }
        Task::OpenNested { archive, entry, strip, password } => {
            let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let dir = std::env::temp_dir().join("Zipo").join(format!("{}-nested-{stamp}", std::process::id()));
            let opts = ExtractOptions { password, selection: Some(vec![entry.clone()]), strip_prefix: strip, ..Default::default() };
            let r = ziptool_core::extract(&archive, &dir, &opts, s)?;
            if let Some(i) = r.issues.first() {
                return Err(Error::Unsupported(F!("無法開啟 {}：{}", "Cannot open {}: {}", i.name, i.reason)));
            }
            let file = extracted_file(&dir).ok_or_else(|| Error::Unsupported(F!("無法開啟 {entry}", "Cannot open {entry}")))?;
            Ok(Outcome { opened: Some(file), ..Default::default() })
        }
        Task::OpenInside { archive, entry, strip, password } => {
            let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let dir = std::env::temp_dir().join("Zipo").join(format!("{}-{stamp}", std::process::id()));
            let opts = ExtractOptions { password, selection: Some(vec![entry.clone()]), strip_prefix: strip.clone(), ..Default::default() };
            let r = ziptool_core::extract(&archive, &dir, &opts, s)?;
            if let Some(i) = r.issues.first() {
                return Err(Error::Unsupported(F!("無法開啟 {}：{}", "Cannot open {}: {}", i.name, i.reason)));
            }
            let file = extracted_file(&dir).ok_or_else(|| Error::Unsupported(F!("無法開啟 {entry}", "Cannot open {entry}")))?;
            crate::open_path(&file);
            Ok(Outcome { opened: Some(file), ..Default::default() })
        }
    }
}

/// 開啟壓縮檔內的檔案時，暫存資料夾是新建的且只解出那一個檔案：直接找出它實際的位置。
/// 不可用項目名稱組路徑：名稱在解壓時會被清理（例如 `C:x` → `C_x`），
/// 而 `dir.join("C:x")` 會丟掉 `dir`，變成指向資料夾之外的另一個檔案。
pub(crate) fn extracted_file(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let t = e.file_type().ok()?;
            if t.is_dir() {
                stack.push(e.path());
            } else if t.is_file() {
                return Some(e.path());
            }
        }
    }
    None
}

#[cfg(test)]
mod open_inside_tests {
    use super::*;

    #[test]
    fn opens_the_file_actually_extracted() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(extracted_file(tmp.path()), None);
        // 例如項目「docs/C:evil.html」解壓後成為 docs/C_evil.html
        std::fs::create_dir_all(tmp.path().join("docs")).unwrap();
        std::fs::write(tmp.path().join("docs/C_evil.html"), "x").unwrap();
        let f = extracted_file(tmp.path()).unwrap();
        assert_eq!(f, tmp.path().join("docs").join("C_evil.html"));
        // 舊的寫法：以項目名稱組路徑會跳出暫存資料夾（「C:」只在 Windows 有特殊意義）
        #[cfg(windows)]
        assert!(!tmp.path().join("C:evil.html").starts_with(tmp.path()));
    }
}

#[cfg(not(test))]
pub(crate) fn send_to_recycle_bin(paths: &[PathBuf]) -> std::io::Result<()> {
    recycle(paths)
}

/// 測試時不動到真正的資源回收筒：改名為「原名.recycled」表示已移除
#[cfg(test)]
pub(crate) fn send_to_recycle_bin(paths: &[PathBuf]) -> std::io::Result<()> {
    for p in paths {
        let mut name = p.file_name().unwrap_or_default().to_os_string();
        name.push(".recycled");
        std::fs::rename(p, p.with_file_name(name))?;
    }
    Ok(())
}

/// 把檔案或資料夾移到資源回收筒（可以還原）。
/// 無法回收時（網路磁碟、部分隨身碟、檔案大於資源回收筒容量）Windows 會改成永久刪除，
/// 這時一定先跳出警告讓使用者決定；選擇不刪除則視為失敗，檔案保留不動。
#[cfg(windows)]
#[cfg_attr(test, allow(dead_code))]
pub fn recycle(paths: &[PathBuf]) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::{FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING, SHFILEOPSTRUCTW, SHFileOperationW};
    if paths.is_empty() {
        return Ok(());
    }
    // 以 0 分隔、兩個 0 結尾的路徑清單；必須是完整路徑才會移到資源回收筒
    let mut list: Vec<u16> = Vec::new();
    for p in paths {
        let full = std::path::absolute(p)?;
        list.extend(full.as_os_str().encode_wide());
        list.push(0);
    }
    list.push(0);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: windows::core::PCWSTR(list.as_ptr()),
        // FOF_WANTNUKEWARNING：要永久刪除時仍會詢問（蓋過 FOF_NOCONFIRMATION）
        fFlags: (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_SILENT.0 | FOF_NOERRORUI.0 | FOF_WANTNUKEWARNING.0) as u16,
        ..Default::default()
    };
    // SAFETY: 路徑清單在呼叫期間有效，結構依 Windows 的定義建立
    let r = unsafe { SHFileOperationW(&mut op) };
    if r != 0 || op.fAnyOperationsAborted.as_bool() {
        return Err(std::io::Error::other(F!("錯誤代碼 {r:#x}", "error code {r:#x}")));
    }
    Ok(())
}

/// 實機測試（會把檔案移到真正的資源回收筒，平常不執行）：`cargo test real_recycle -- --ignored`
/// 環境變數 ZIPO_RECYCLE_FILE 為要移走的檔案
#[cfg(all(test, windows))]
#[test]
#[ignore]
fn real_recycle_moves_file_to_recycle_bin() {
    let f = PathBuf::from(std::env::var("ZIPO_RECYCLE_FILE").unwrap());
    assert!(f.exists());
    recycle(std::slice::from_ref(&f)).unwrap();
    assert!(!f.exists(), "應已移走");
}

#[cfg(not(windows))]
#[cfg_attr(test, allow(dead_code))]
pub fn recycle(_paths: &[PathBuf]) -> std::io::Result<()> {
    Err(std::io::Error::other(L!("只支援 Windows", "Only supported on Windows")))
}

#[cfg(test)]
mod then_tests {
    use super::*;

    fn finish(task: Task) -> Outcome {
        let (_, res) = Job::spawn(task).join();
        res.unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn compress_then_recycle_sources_and_open_folder() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("data");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("a.txt"), "a").unwrap();
        let out = t.path().join("o.7z");
        let then = Then { recycle: true, open_folder: true };
        let o = finish(Task::Compress { inputs: vec![src.clone()], output: out.clone(), format: Format::SevenZ, opts: Default::default(), then });
        assert!(o.message.contains("原始檔已移到資源回收筒"), "{}", o.message);
        assert!(!src.exists() && t.path().join("data.recycled").exists());
        assert!(o.auto_open && o.open_dir.as_deref() == Some(t.path()));
        // 壓縮檔完整
        assert_eq!(ziptool_core::test(&out, None, &ziptool_core::NoProgress).unwrap().files, 1);
    }

    #[test]
    fn extract_then_recycle_archive_only_when_everything_extracted() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("a.txt");
        std::fs::write(&src, "a").unwrap();
        std::fs::write(t.path().join("b.txt"), "b").unwrap();
        let make = |name: &str| {
            let ar = t.path().join(name);
            ziptool_core::compress(&[src.clone(), t.path().join("b.txt")], &ar, Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();
            ar
        };
        let then = Then { recycle: true, open_folder: false };

        // 全部解出：壓縮檔移除
        let ar = make("all.zip");
        let o = finish(Task::Extract { archives: vec![ar.clone()], dest: t.path().join("x1"), opts: Default::default(), smart: false, here: false, then });
        assert!(o.message.contains("壓縮檔已移到資源回收筒"), "{}", o.message);
        assert!(!ar.exists() && !o.auto_open);

        // 只解出部分項目：壓縮檔保留
        let ar = make("part.zip");
        let opts = ExtractOptions { selection: Some(vec!["a.txt".into()]), ..Default::default() };
        let o = finish(Task::Extract { archives: vec![ar.clone()], dest: t.path().join("x2"), opts, smart: false, here: false, then });
        assert!(o.message.contains("壓縮檔保留不動"), "{}", o.message);
        assert!(ar.exists());

        // 目的地已有同名檔案、選擇略過：解出的不是壓縮檔裡的版本，壓縮檔保留
        let ar = make("skip.zip");
        std::fs::create_dir_all(t.path().join("x4")).unwrap();
        std::fs::write(t.path().join("x4/a.txt"), "舊版").unwrap();
        let opts = ExtractOptions { overwrite: ziptool_core::Overwrite::Skip, ..Default::default() };
        let o = finish(Task::Extract { archives: vec![ar.clone()], dest: t.path().join("x4"), opts, smart: false, here: false, then });
        assert!(o.message.contains("壓縮檔保留不動"), "{}", o.message);
        assert!(ar.exists());
        assert_eq!(std::fs::read_to_string(t.path().join("x4/a.txt")).unwrap(), "舊版");

        // 分割壓縮檔：整組分割檔都移除，不只是 .001
        let whole = t.path().join("vol.zip");
        let opts = ziptool_core::CompressOptions { level: Some(0), volume_size: Some(64), ..Default::default() };
        ziptool_core::compress(&[src.clone(), t.path().join("b.txt")], &whole, Format::Zip, &opts, &ziptool_core::NoProgress).unwrap();
        let first = t.path().join("vol.zip.001");
        assert!(t.path().join("vol.zip.002").exists());
        let o = finish(Task::Extract { archives: vec![first], dest: t.path().join("x5"), opts: Default::default(), smart: false, here: false, then });
        assert!(o.message.contains("壓縮檔已移到資源回收筒"), "{}", o.message);
        let left: Vec<_> = std::fs::read_dir(t.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("vol.zip.0") && !n.ends_with(".recycled")).collect();
        assert!(left.is_empty(), "分割檔應全部移除：{left:?}");

        // 分割壓縮：完成訊息的「原始大小」是原始檔案的大小（不是壓縮檔的大小），並列出分割檔數量
        let big = t.path().join("大檔.bin");
        std::fs::write(&big, vec![7u8; 300_000]).unwrap();
        let opts = ziptool_core::CompressOptions { level: Some(0), volume_size: Some(100_000), ..Default::default() };
        let o = finish(Task::Compress { inputs: vec![big], output: t.path().join("v2.zip"), format: Format::Zip, opts, then: Then::default() });
        assert!(o.message.contains(&format!("原始大小：{}", crate::human(300_000))), "{}", o.message);
        assert!(o.message.contains("分割為 4 個檔案"), "{}", o.message);

        // 沒有勾選：不動
        let ar = make("keep.zip");
        finish(Task::Extract { archives: vec![ar.clone()], dest: t.path().join("x3"), opts: Default::default(), smart: false, here: false, then: Then::default() });
        assert!(ar.exists());
    }
}
