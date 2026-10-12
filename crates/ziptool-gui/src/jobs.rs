//! 背景工作：壓縮、解壓縮、測試、開啟壓縮檔內的檔案。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use ziptool_core::{CompressOptions, Error, ExtractOptions, Format, Issue, OverwriteAnswer, Progress};

use crate::human;

#[derive(Clone)]
pub enum Task {
    Compress { inputs: Vec<PathBuf>, output: PathBuf, format: Format, opts: CompressOptions },
    /// 多個壓縮檔時，每個解到 `dest/<名稱>`；單一壓縮檔直接解到 `dest`。
    /// `smart`：壓縮檔內只有一個最上層資料夾時，不再多包一層同名資料夾。
    Extract { archives: Vec<PathBuf>, dest: PathBuf, opts: ExtractOptions, smart: bool },
    Test { archives: Vec<PathBuf>, password: Option<String> },
    /// 更新既有壓縮檔：加入檔案到 `into` 資料夾、刪除 `remove` 中的項目。
    Update { archive: PathBuf, add: Vec<PathBuf>, into: String, remove: Vec<String>, password: Option<String> },
    /// 把壓縮檔內的單一檔案解到暫存資料夾後，用系統預設程式開啟。
    OpenInside { archive: PathBuf, entry: String, strip: String, password: Option<String> },
    /// 計算檔案（資料夾則為其中每個檔案）的雜湊值
    Hash { inputs: Vec<PathBuf> },
    /// 計算壓縮檔內項目的雜湊值：先解到暫存資料夾
    HashInside { archive: PathBuf, entries: Vec<String>, strip: String, password: Option<String> },
}

impl Task {
    pub fn title(&self) -> &'static str {
        match self {
            Task::Compress { .. } => "正在壓縮",
            Task::Extract { .. } => "正在解壓縮",
            Task::Test { .. } => "正在測試",
            Task::OpenInside { .. } => "正在開啟",
            Task::Update { .. } => "正在更新壓縮檔",
            Task::Hash { .. } | Task::HashInside { .. } => "正在計算雜湊值",
        }
    }

    pub fn set_password(&mut self, pw: String) {
        match self {
            Task::Extract { opts, .. } => opts.password = Some(pw),
            Task::Test { password, .. }
            | Task::OpenInside { password, .. }
            | Task::Update { password, .. }
            | Task::HashInside { password, .. } => *password = Some(pw),
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
        let r = self.handle.join().unwrap_or_else(|_| Err(Error::Unsupported("背景工作異常中止".into())));
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
    *lock(&s.stage) = if n > 1 { format!("第 {} / {n} 個：{}", i + 1, file_name(ar)) } else { file_name(ar) };
}

fn run(task: Task, s: &Shared) -> Result<Outcome, Error> {
    let started = Instant::now();
    match task {
        Task::Compress { inputs, output, format, opts } => {
            ziptool_core::compress(&inputs, &output, format, &opts, s)?;
            let size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
            let total: u64 = s.total.load(Ordering::Relaxed);
            let ratio = if total > 0 { size as f64 * 100.0 / total as f64 } else { 0.0 };
            Ok(Outcome {
                title: "壓縮完成".into(),
                message: format!(
                    "{}\n\n原始大小：{}\n壓縮後：{}（{ratio:.0}%）\n耗時：{:.1} 秒",
                    output.display(),
                    human(total),
                    human(size),
                    started.elapsed().as_secs_f32()
                ),
                open_dir: output.parent().map(Path::to_path_buf),
                refresh: true,
                issues: Vec::new(),
                ..Default::default()
            })
        }
        Task::Extract { archives, dest, opts, smart } => {
            let n = archives.len();
            let (mut files, mut bytes, mut issues) = (0, 0, Vec::new());
            let mut shown = dest.clone();
            for (i, ar) in archives.iter().enumerate() {
                stage(s, i, n, ar);
                let mut out = if n > 1 { dest.join(archive_stem(ar)) } else { dest.clone() };
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
            }
            let title = if issues.is_empty() { "解壓縮完成" } else { "解壓縮完成，但有項目被略過" };
            Ok(Outcome {
                title: title.into(),
                message: format!(
                    "已解壓縮到：\n{}\n\n檔案：{files} 個（{}）\n耗時：{:.1} 秒",
                    shown.display(),
                    human(bytes),
                    started.elapsed().as_secs_f32()
                ),
                open_dir: Some(shown),
                refresh: true,
                issues,
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
                title: "測試完成".into(),
                message: format!("沒有發現錯誤。\n\n壓縮檔：{n} 個\n檔案：{files} 個\n大小：{}", human(bytes)),
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
                (a, 0) => format!("已加入 {a} 個項目"),
                (0, r) => format!("已刪除 {r} 個項目"),
                (a, r) => format!("已加入 {a} 個、刪除 {r} 個項目"),
            };
            Ok(Outcome {
                title: "已更新壓縮檔".into(),
                message: format!("{}\n\n{what}\n耗時：{:.1} 秒", archive.display(), started.elapsed().as_secs_f32()),
                open_dir: None,
                refresh: true,
                issues: Vec::new(),
                ..Default::default()
            })
        }
        Task::Hash { inputs } => Ok(Outcome { hashes: ziptool_core::hash_files(&inputs, s)?, ..Default::default() }),
        Task::HashInside { archive, entries, strip, password } => {
            let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let dir = std::env::temp_dir().join("Zipo").join(format!("{}-hash-{stamp}", std::process::id()));
            let r = (|| {
                let opts = ExtractOptions { password, selection: Some(entries), strip_prefix: strip, ..Default::default() };
                let r = ziptool_core::extract(&archive, &dir, &opts, s)?;
                if let Some(i) = r.issues.first() {
                    return Err(Error::Unsupported(format!("無法讀取 {}：{}", i.name, i.reason)));
                }
                let mut top: Vec<PathBuf> = std::fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
                top.sort();
                ziptool_core::hash_files(&top, s)
            })();
            let _ = std::fs::remove_dir_all(&dir);
            Ok(Outcome { hashes: r?, ..Default::default() })
        }
        Task::OpenInside { archive, entry, strip, password } => {
            let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let dir = std::env::temp_dir().join("Zipo").join(format!("{}-{stamp}", std::process::id()));
            let opts = ExtractOptions { password, selection: Some(vec![entry.clone()]), strip_prefix: strip.clone(), ..Default::default() };
            let r = ziptool_core::extract(&archive, &dir, &opts, s)?;
            if let Some(i) = r.issues.first() {
                return Err(Error::Unsupported(format!("無法開啟 {}：{}", i.name, i.reason)));
            }
            let file = extracted_file(&dir).ok_or_else(|| Error::Unsupported(format!("無法開啟 {entry}")))?;
            crate::open_path(&file);
            Ok(Outcome::default())
        }
    }
}

/// 開啟壓縮檔內的檔案時，暫存資料夾是新建的且只解出那一個檔案：直接找出它實際的位置。
/// 不可用項目名稱組路徑：名稱在解壓時會被清理（例如 `C:x` → `C_x`），
/// 而 `dir.join("C:x")` 會丟掉 `dir`，變成指向資料夾之外的另一個檔案。
fn extracted_file(dir: &Path) -> Option<PathBuf> {
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
