// release 版不跳出主控台視窗
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Zipo 檔案管理員：版面與操作方式參考 7-Zip File Manager。

mod browser;
#[cfg(debug_assertions)]
mod devshot;
mod dialogs;
mod jobs;
mod localize;
// 效能診斷只在開發版提供：正式版不應依環境變數寫檔、自動捲動或自行關閉
#[cfg(debug_assertions)]
mod perf;
mod shell;
mod theme;
mod update;
mod widgets;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use eframe::egui::{self, Align, Key, Layout, Modifiers, RichText, Sense, vec2};
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as icon;
use ziptool_core::{Entry, Error, Format, Issue};

use browser::{Location, Row, SortKey};
use dialogs::{AddDialog, ExtractDialog, ExtractSource, Outcome, PasswordDialog, PasswordPurpose};
use jobs::{Job, Task, Then};
use theme::pal;
use widgets::*;

/// release 版沒有主控台：發生 panic 時寫入記錄檔並跳出訊息，而不是無聲消失。
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let log = std::env::temp_dir().join("Zipo").join("crash.log");
        let bt = std::backtrace::Backtrace::force_capture();
        let text = format!("Zipo {} 發生錯誤
{info}

{bt}
", env!("CARGO_PKG_VERSION"));
        if let Some(dir) = log.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&log, &text);
        default(info);
        // 只在主執行緒提示；背景工作的 panic 會由 Job::join 轉成一般錯誤訊息。
        // 第一個畫面出現前的失敗會先自動改用其他繪圖方式重試，重試也失敗時才提示。
        let will_retry = !FIRST_FRAME.load(std::sync::atomic::Ordering::Relaxed) && std::env::var_os(RENDER_RETRY).is_none();
        if std::thread::current().name() == Some("main") && !will_retry {
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("Zipo 發生錯誤")
                .set_description(format!("{info}

詳細資訊已寫入：
{}", log.display()))
                .show();
        }
    }));
}

/// 第一個畫面已經畫出來（之後的 panic 不再是繪圖初始化問題）
static FIRST_FRAME: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 已經改用備援繪圖方式重新啟動過
const RENDER_RETRY: &str = "ZIPTOOL_RENDER_RETRY";

fn main() -> eframe::Result {
    install_panic_hook();
    match std::panic::catch_unwind(run_app) {
        Ok(r) => r,
        // 顯示卡驅動與繪圖後端不相容時，第一個畫面出現前就會失敗：
        // 改用 Vulkan／OpenGL 重新啟動一次，而不是直接結束。
        Err(_) if !FIRST_FRAME.load(std::sync::atomic::Ordering::Relaxed) && std::env::var_os(RENDER_RETRY).is_none() => {
            let exe = std::env::current_exe().map_err(|e| eframe::Error::AppCreation(Box::new(e)))?;
            std::process::Command::new(exe)
                .args(std::env::args_os().skip(1))
                .env("WGPU_BACKEND", "vulkan,gl")
                .env(RENDER_RETRY, "1")
                .spawn()
                .map_err(|e| eframe::Error::AppCreation(Box::new(e)))?;
            Ok(())
        }
        Err(p) => std::panic::resume_unwind(p),
    }
}

fn run_app() -> eframe::Result {
    #[allow(unused_mut)]
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Zipo")
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/ziptool-256.png")).unwrap_or_default())
            .with_inner_size([1000.0, 660.0])
            .with_min_inner_size([640.0, 420.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    // Windows 上優先使用 DirectX 12：實測 Intel 內顯的 Vulkan 驅動捲動時會固定掉幀（每幀間隔到 35 ms 以上，
    // 偶爾 50–100 ms），DirectX 12 則穩定在 16–22 ms。設定了 WGPU_BACKEND 時尊重使用者的選擇。
    // 32 位元程式中 wgpu 編譯 DX12 著色器會失敗（實測啟動即當掉），所以只在 64 位元版啟用。
    #[cfg(all(windows, target_pointer_width = "64"))]
    if std::env::var_os("WGPU_BACKEND").is_none()
        && let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup
    {
        setup.native_adapter_selector = Some(std::sync::Arc::new(|adapters, surface| {
            let usable = |a: &&eframe::wgpu::Adapter| surface.is_none_or(|s| a.is_surface_supported(s));
            adapters
                .iter()
                .filter(usable)
                .find(|a| a.get_info().backend == eframe::wgpu::Backend::Dx12)
                .or_else(|| adapters.iter().find(usable))
                .cloned()
                .ok_or_else(|| "找不到可用的顯示卡".to_string())
        }));
    }
    let start = parse_args(std::env::args_os().skip(1).collect());
    eframe::run_native(
        // 設定儲存名稱維持舊名，保留使用者原有的設定
        "ZipTool",
        options,
        Box::new(move |cc| {
            theme::setup(&cc.egui_ctx);
            let open = match &start {
                StartAction::Open(p) => Some(p.clone()),
                _ => None,
            };
            let mut app = App::with_storage(open, cc.storage);
            app.apply_start(start);
            if let Ok(exe) = std::env::current_exe() {
                update::cleanup_old(&exe);
            }
            app.auto_update_check();
            #[cfg(debug_assertions)]
            if let Some((shot, setup)) = devshot::DevShot::from_env(&cc.egui_ctx) {
                if let Some(p) = setup.open {
                    app.open_path_arg(p);
                }
                app.devshot = Some((shot, setup.select, setup.dialog));
            }
            Ok(Box::new(app))
        }),
    )
}

/// 從命令列（例如檔案總管右鍵選單）啟動時要做的事。
#[derive(Debug, PartialEq)]
enum StartAction {
    None,
    Open(PathBuf),
    Add(Vec<PathBuf>),
    ExtractHere(PathBuf),
    ExtractFolder(PathBuf),
}

fn parse_args(args: Vec<std::ffi::OsString>) -> StartAction {
    let mut it = args.into_iter();
    let Some(first) = it.next() else { return StartAction::None };
    let rest: Vec<PathBuf> = it.map(PathBuf::from).collect();
    match first.to_str() {
        Some("--add") if !rest.is_empty() => StartAction::Add(rest),
        Some("--extract-here") if rest.len() == 1 => StartAction::ExtractHere(rest[0].clone()),
        Some("--extract-folder") if rest.len() == 1 => StartAction::ExtractFolder(rest[0].clone()),
        Some(s) if s.starts_with("--") => StartAction::None,
        _ => StartAction::Open(PathBuf::from(first)),
    }
}

// ---------- 共用小工具 ----------

pub fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

pub fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

fn format_time(t: Option<SystemTime>) -> String {
    let Some(d) = t.and_then(ziptool_core::local_time) else { return String::new() };
    format!("{}/{}/{} {:02}:{:02}", d.year(), u8::from(d.month()), d.day(), d.hour(), d.minute())
}

fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// 用系統預設程式開啟檔案或資料夾。
pub fn open_path(path: &Path) {
    // 直接呼叫 ShellExecuteW，不經過 explorer.exe 的命令列：explorer 會把逗號當成參數分隔，
    // 例如「a,b.txt」會開成「文件」資料夾而不是這個檔案。在背景執行緒呼叫，避免部分程式卡住畫面。
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "shell32")]
        unsafe extern "system" {
            fn ShellExecuteW(
                hwnd: *mut std::ffi::c_void,
                op: *const u16,
                file: *const u16,
                params: *const u16,
                dir: *const u16,
                show: i32,
            ) -> *mut std::ffi::c_void;
        }
        let file: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        std::thread::spawn(move || {
            let op: Vec<u16> = "open\0".encode_utf16().collect();
            const SW_SHOWNORMAL: i32 = 1;
            // SAFETY: 兩個字串都以 0 結尾且在呼叫期間有效
            unsafe { ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
        });
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

pub fn pick_save(current: &str, format: Format) -> Option<PathBuf> {
    let cur = PathBuf::from(current);
    let mut dlg = rfd::FileDialog::new().add_filter(format.name(), &[format.extension()]);
    if let Some(dir) = cur.parent().filter(|d| d.is_dir()) {
        dlg = dlg.set_directory(dir);
    }
    dlg.set_file_name(file_name(&cur)).save_file()
}

pub fn pick_folder(current: &str) -> Option<PathBuf> {
    let mut dlg = rfd::FileDialog::new();
    let cur = PathBuf::from(current);
    if let Some(dir) = cur.ancestors().find(|d| d.is_dir()) {
        dlg = dlg.set_directory(dir);
    }
    dlg.pick_folder()
}

/// 檔案類型圖示的顏色：資料夾琥珀、壓縮檔紫、圖片綠、PDF 紅，其餘中性灰
fn row_icon_color(r: &Row, p: &theme::Palette) -> egui::Color32 {
    if r.is_drive {
        return p.text2;
    }
    if r.is_dir {
        return p.folder;
    }
    match row_icon(r) {
        icon::FILE_ZIP => p.archive,
        icon::FILE_IMAGE => p.image,
        icon::FILE_PDF => p.doc_red,
        _ => p.text2,
    }
}

fn row_icon(r: &Row) -> &'static str {
    if r.is_drive {
        return icon::HARD_DRIVES;
    }
    if r.is_dir {
        return icon::FOLDER;
    }
    if Format::from_file_name(&r.name).is_some() {
        return icon::FILE_ZIP;
    }
    let ext = r.name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "heic" | "ico" => icon::FILE_IMAGE,
        "txt" | "md" | "log" | "csv" | "json" | "xml" | "ini" | "toml" | "yaml" | "yml" => icon::FILE_TEXT,
        "pdf" => icon::FILE_PDF,
        "doc" | "docx" => icon::FILE_DOC,
        "xls" | "xlsx" => icon::FILE_XLS,
        "ppt" | "pptx" => icon::FILE_PPT,
        "mp3" | "wav" | "flac" | "m4a" | "ogg" => icon::FILE_AUDIO,
        "mp4" | "mkv" | "mov" | "avi" | "webm" => icon::FILE_VIDEO,
        "rs" | "js" | "ts" | "py" | "c" | "cpp" | "h" | "cs" | "java" | "go" | "html" | "css" => icon::FILE_CODE,
        "exe" | "msi" | "dll" | "bat" | "cmd" | "ps1" => icon::GEAR_SIX,
        _ => icon::FILE,
    }
}

// ---------- 應用程式狀態 ----------

struct ArchiveCache {
    path: PathBuf,
    entries: Vec<Entry>,
    password: Option<String>,
    /// 讀取清單時壓縮檔的大小與修改時間；不同就代表被其他程式改過，需要重新讀取
    stamp: (u64, Option<SystemTime>),
    format: &'static str,
}

impl ArchiveCache {
    fn is_fresh(&self) -> bool {
        file_stamp(&self.path) == Some(self.stamp)
    }
}

impl Drop for ArchiveCache {
    fn drop(&mut self) {
        if let Some(pw) = &mut self.password {
            zeroize::Zeroize::zeroize(pw);
        }
    }
}

fn file_stamp(p: &Path) -> Option<(u64, Option<SystemTime>)> {
    std::fs::metadata(p).ok().map(|m| (m.len(), m.modified().ok()))
}

struct Loading {
    path: PathBuf,
    inner: String,
    password: Option<String>,
    handle: JoinHandle<Result<Vec<Entry>, Error>>,
    /// 由上一頁／下一頁觸發的讀取，完成時不要再記入瀏覽紀錄
    from_history: bool,
}

enum Dialog {
    Add(AddDialog),
    Extract(ExtractDialog),
    Password(PasswordDialog),
    Info { title: String, lines: Vec<(String, String)> },
    Notice { title: String, message: String, ok: bool, open_dir: Option<PathBuf>, issues: Vec<Issue> },
    /// 壓縮比異常高，確認後才解壓
    ConfirmBomb { task: Task, message: String },
    /// 執行壓縮檔內的程式前確認
    ConfirmRun { task: Task, name: String },
    /// 更新壓縮檔（加入或刪除項目）前確認
    ConfirmUpdate { task: Task, title: String, message: String, danger: bool },
    About,
    /// 雜湊值結果；`expected` 為使用者貼上要比對的值
    Hashes { rows: Vec<(String, ziptool_core::Hashes)>, expected: String },
    /// 有新版本可以更新
    UpdateAvailable(update::Release),
    /// 正在下載並安裝更新
    Updating,
}

/// 線上更新的背景工作
enum UpdateWork {
    Check { manual: bool, handle: std::thread::JoinHandle<Result<update::Release, String>> },
    Install(std::thread::JoinHandle<Result<PathBuf, String>>),
}

/// 工具列每個按鈕是否可用
#[derive(Debug, PartialEq)]
struct ToolbarState {
    in_archive: bool,
    compress: bool,
    extract: bool,
    test: bool,
    info: bool,
    delete: bool,
}

#[derive(Clone, Copy)]
enum Action {
    Open,
    Add,
    Extract,
    Test,
    Info,
    Delete,
    Hash,
}

struct App {
    loc: Location,
    rows: Vec<Row>,
    sel: BTreeSet<usize>,
    anchor: Option<usize>,
    sort_key: SortKey,
    sort_asc: bool,
    addr: String,
    status_error: Option<String>,
    cache: Option<ArchiveCache>,
    loading: Option<Loading>,
    dialog: Option<Dialog>,
    job: Option<Job>,
    scroll_to: Option<usize>,
    /// 工作進行中按下關閉：先取消工作，結束後再關閉視窗
    close_after_job: bool,
    /// 目前位置的完整清單（`rows` 是套用篩選、排序後的結果）
    all_rows: Vec<Row>,
    filter: String,
    back: Vec<Location>,
    forward: Vec<Location>,
    /// 正在執行上一頁／下一頁，這次移動不記入紀錄
    history_nav: bool,
    /// 網址列切換成可直接輸入路徑
    addr_editing: bool,
    /// 直接打字跳到檔案
    typeahead: String,
    typeahead_at: std::time::Instant,
    /// 加入壓縮檔對話框上次選的格式與等級
    add_format: Format,
    add_level: usize,
    /// 顯示「隱藏＋系統」屬性的項目（預設與檔案總管相同，不顯示）
    show_system: bool,
    /// 啟動時自動檢查更新（每天最多一次）
    auto_update: bool,
    /// 上次檢查更新的時間（Unix 秒）
    last_update_check: u64,
    update_work: Option<UpdateWork>,
    /// 自動檢查找到的新版本，等目前沒有其他對話框或工作時再顯示
    pending_release: Option<update::Release>,
    /// 要拖曳到程式外的壓縮檔內項目（完整名稱）；畫面處理完後才開始拖曳
    drag_out: Option<Vec<String>>,
    /// 上次在壓縮／解壓縮對話框勾選的「完成後開啟資料夾」
    open_after: bool,
    /// 效能診斷（設定 ZIPTOOL_PERF 時啟用）
    #[cfg(debug_assertions)]
    perf: Option<perf::Perf>,

    #[cfg(debug_assertions)]
    devshot: Option<(devshot::DevShot, usize, Option<String>)>,
}

impl App {
    #[cfg(test)]
    fn new(start: Option<PathBuf>) -> Self {
        Self::with_storage(start, None)
    }

    /// `storage`：上次結束時儲存的設定（最後的資料夾、排序、加入壓縮檔的格式與等級、ZIP 檔名編碼）。
    fn with_storage(start: Option<PathBuf>, storage: Option<&dyn eframe::Storage>) -> Self {
        let mut app = App {
            loc: Location::Computer,
            rows: Vec::new(),
            sel: BTreeSet::new(),
            anchor: None,
            sort_key: SortKey::Name,
            sort_asc: true,
            addr: String::new(),
            status_error: None,
            cache: None,
            loading: None,
            dialog: None,
            job: None,
            scroll_to: None,
            close_after_job: false,
            all_rows: Vec::new(),
            filter: String::new(),
            back: Vec::new(),
            forward: Vec::new(),
            history_nav: false,
            addr_editing: false,
            typeahead: String::new(),
            typeahead_at: std::time::Instant::now(),
            add_format: Format::SevenZ,
            add_level: 3,
            show_system: false,
            auto_update: true,
            last_update_check: 0,
            update_work: None,
            pending_release: None,
            drag_out: None,
            open_after: false,
            #[cfg(debug_assertions)]
            perf: perf::Perf::from_env(),
            #[cfg(debug_assertions)]
            devshot: None,
        };
        let get = |k: &str| storage.and_then(|s| s.get_string(k));
        app.auto_update = get("auto_update").as_deref() != Some("0");
        app.open_after = get("open_after").as_deref() == Some("1");
        app.last_update_check = get("last_update_check").and_then(|v| v.parse().ok()).unwrap_or(0);
        if let Some(k) = get("sort_key") {
            app.sort_key = match k.as_str() {
                "size" => SortKey::Size,
                "packed" => SortKey::Packed,
                "modified" => SortKey::Modified,
                "type" => SortKey::Type,
                _ => SortKey::Name,
            };
        }
        app.sort_asc = get("sort_asc").is_none_or(|v| v != "0");
        if let Some(f) = get("add_format").and_then(|f| Format::from_name(&f)) {
            app.add_format = f;
        }
        if let Some(l) = get("add_level").and_then(|l| l.parse().ok()).filter(|l| *l < dialogs::LEVELS.len()) {
            app.add_level = l;
        }
        ziptool_core::set_zip_codepage(get("zip_codepage").and_then(|c| c.parse().ok()));
        app.show_system = get("show_system").is_some_and(|v| v == "1");

        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
        let last = get("last_dir").map(PathBuf::from).filter(|p| p.is_dir());
        match start {
            Some(p) => app.open_path_arg(p),
            None => app.navigate(last.or(home).map(Location::Dir).unwrap_or(Location::Computer)),
        }
        app.back.clear();
        cleanup_temp(false);
        app
    }

    fn apply_start(&mut self, action: StartAction) {
        let parent = |p: &Path| p.parent().map(Path::to_path_buf).unwrap_or_default();
        match action {
            StartAction::None | StartAction::Open(_) => {}
            StartAction::Add(paths) => {
                self.navigate(Location::Dir(parent(&paths[0])));
                self.dialog = Some(Dialog::Add(self.with_open_after_add(AddDialog::new(paths, self.add_format, self.add_level))));
            }
            StartAction::ExtractHere(ar) => {
                let dir = parent(&ar);
                self.navigate(Location::Dir(dir.clone()));
                let opts = ziptool_core::ExtractOptions { overwrite: ziptool_core::Overwrite::Ask, ..Default::default() };
                self.spawn(Task::Extract { archives: vec![ar], dest: dir, opts, smart: false, then: Then::default() });
            }
            StartAction::ExtractFolder(ar) => {
                self.navigate(Location::Dir(parent(&ar)));
                self.dialog = Some(Dialog::Extract(self.with_open_after_extract(ExtractDialog::new(ExtractSource::Files(vec![ar]), None))));
            }
        }
    }

    fn save_settings(&self, storage: &mut dyn eframe::Storage) {
        if let Location::Dir(d) = &self.loc {
            storage.set_string("last_dir", d.display().to_string());
        } else if let Location::Archive { path, .. } = &self.loc
            && let Some(d) = path.parent()
        {
            storage.set_string("last_dir", d.display().to_string());
        }
        let key = match self.sort_key {
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Packed => "packed",
            SortKey::Modified => "modified",
            SortKey::Type => "type",
        };
        storage.set_string("sort_key", key.into());
        storage.set_string("sort_asc", if self.sort_asc { "1" } else { "0" }.into());
        storage.set_string("add_format", self.add_format.name().into());
        storage.set_string("add_level", self.add_level.to_string());
        storage.set_string("show_system", if self.show_system { "1" } else { "0" }.into());
        storage.set_string("auto_update", if self.auto_update { "1" } else { "0" }.into());
        storage.set_string("open_after", if self.open_after { "1" } else { "0" }.into());
        storage.set_string("last_update_check", self.last_update_check.to_string());
        storage.set_string("zip_codepage", ziptool_core::zip_codepage().map(|c| c.to_string()).unwrap_or_default());
    }

    /// 從命令列或拖放開啟：資料夾直接進入，壓縮檔則打開瀏覽。
    fn open_path_arg(&mut self, p: PathBuf) {
        let p = std::fs::canonicalize(&p).map(strip_verbatim).unwrap_or(p);
        if p.is_dir() {
            self.navigate(Location::Dir(p));
        } else if p.is_file() {
            if let Some(dir) = p.parent() {
                self.navigate(Location::Dir(dir.to_path_buf()));
            }
            self.navigate(Location::Archive { path: p, inner: String::new() });
        } else {
            self.status_error = Some(format!("找不到：{}", p.display()));
        }
    }

    fn busy(&self) -> bool {
        self.job.is_some() || self.dialog.is_some()
    }

    // ---------- 瀏覽 ----------

    fn navigate(&mut self, loc: Location) {
        self.status_error = None;
        // 使用者已前往別處：放棄進行中的讀取，讀完也不會再跳回那個壓縮檔
        self.loading = None;
        let rows = match &loc {
            Location::Computer => browser::list_drives(),
            Location::Dir(p) => match browser::list_dir(p, self.show_system) {
                Ok(r) => r,
                Err(e) => {
                    let why = if e.kind() == std::io::ErrorKind::PermissionDenied {
                        "沒有讀取這個資料夾的權限".to_string()
                    } else {
                        ziptool_core::describe(&e)
                    };
                    self.status_error = Some(format!("無法開啟 {}：{why}", p.display()));
                    self.addr = self.loc.display();
                    return;
                }
            },
            Location::Archive { path, inner } => match &self.cache {
                Some(c) if c.path == *path && c.is_fresh() => browser::archive_rows(&c.entries, inner),
                _ => {
                    self.start_loading(path.clone(), inner.clone(), None);
                    return;
                }
            },
        };
        let prev = std::mem::replace(&mut self.loc, loc);
        if prev != self.loc {
            self.filter.clear();
            if !self.history_nav {
                self.back.push(prev.clone());
                if self.back.len() > 100 {
                    self.back.remove(0);
                }
                self.forward.clear();
            }
        }
        // 舊的選取索引屬於上一個清單，必須在排序前清掉
        self.sel.clear();
        self.anchor = None;
        self.all_rows = rows;
        self.resort();
        // 回到上一層時，選取剛才所在的資料夾
        if let Some(name) = child_name(&self.loc, &prev)
            && let Some(i) = self.rows.iter().position(|r| r.name == name)
        {
            self.sel.insert(i);
            self.anchor = Some(i);
            self.scroll_to = Some(i);
        }
        self.addr = self.loc.display();
    }

    fn start_loading(&mut self, path: PathBuf, inner: String, password: Option<String>) {
        let (p, pw) = (path.clone(), password.clone());
        let handle = std::thread::spawn(move || ziptool_core::list(&p, pw.as_deref()));
        self.loading = Some(Loading { path, inner, password, handle, from_history: self.history_nav });
    }

    fn refresh(&mut self) {
        if let Location::Archive { path, .. } = &self.loc
            && let Some(c) = &self.cache
            && c.path == *path
        {
            let (p, pw) = (path.clone(), c.password.clone());
            self.cache = None;
            let inner = match &self.loc {
                Location::Archive { inner, .. } => inner.clone(),
                _ => String::new(),
            };
            self.start_loading(p, inner, pw);
            return;
        }
        let selected: Vec<String> = self.sel.iter().map(|&i| self.rows[i].name.clone()).collect();
        self.navigate(self.loc.clone());
        self.sel = self.rows.iter().enumerate().filter(|(_, r)| selected.contains(&r.name)).map(|(i, _)| i).collect();
    }

    /// 依篩選文字與排序方式重建 `rows`，並依名稱保留選取。
    fn resort(&mut self) {
        let selected: Vec<String> = self.sel.iter().filter_map(|&i| self.rows.get(i)).map(|r| r.name.clone()).collect();
        let f = self.filter.to_lowercase();
        self.rows = self.all_rows.iter().filter(|r| f.is_empty() || r.display.to_lowercase().contains(&f) || r.name.to_lowercase().contains(&f)).cloned().collect();
        browser::sort(&mut self.rows, self.sort_key, self.sort_asc);
        self.sel = self.rows.iter().enumerate().filter(|(_, r)| selected.contains(&r.name)).map(|(i, _)| i).collect();
        self.anchor = self.anchor.filter(|&a| a < self.rows.len());
    }

    fn go_back(&mut self) {
        if let Some(loc) = self.back.pop() {
            self.forward.push(self.loc.clone());
            self.history_nav = true;
            self.navigate(loc);
            self.history_nav = false;
        }
    }

    fn go_forward(&mut self) {
        if let Some(loc) = self.forward.pop() {
            self.back.push(self.loc.clone());
            self.history_nav = true;
            self.navigate(loc);
            self.history_nav = false;
        }
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.loc.parent() {
            self.navigate(parent);
        }
    }

    fn go_address(&mut self) {
        let text = self.addr.trim().trim_matches('"').to_string();
        if text.is_empty() || text == "電腦" {
            return self.navigate(Location::Computer);
        }
        // 也接受「壓縮檔路徑\內部資料夾\」
        let path = PathBuf::from(&text);
        if path.is_dir() {
            return self.navigate(Location::Dir(path));
        }
        if path.is_file() {
            return self.navigate(Location::Archive { path, inner: String::new() });
        }
        for anc in path.ancestors().skip(1) {
            if anc.is_file() {
                let inner = path.strip_prefix(anc).unwrap_or(Path::new("")).to_string_lossy().replace('\\', "/");
                return self.navigate(Location::Archive { path: anc.to_path_buf(), inner: inner.trim_matches('/').to_string() });
            }
        }
        self.status_error = Some(format!("找不到路徑：{text}"));
    }

    fn row_path(&self, r: &Row) -> Option<PathBuf> {
        match &self.loc {
            Location::Computer => Some(PathBuf::from(&r.name)),
            Location::Dir(d) => Some(d.join(&r.name)),
            Location::Archive { .. } => None,
        }
    }

    fn inner_name(&self, r: &Row) -> String {
        match &self.loc {
            Location::Archive { inner, .. } if !inner.is_empty() => format!("{inner}/{}", r.name),
            _ => r.name.clone(),
        }
    }

    fn selected_rows(&self) -> Vec<&Row> {
        self.sel.iter().map(|&i| &self.rows[i]).collect()
    }

    fn open_row(&mut self, i: usize) {
        let r = self.rows[i].clone();
        match self.loc.clone() {
            Location::Computer | Location::Dir(_) => {
                let path = self.row_path(&r).unwrap();
                if r.is_dir {
                    self.navigate(Location::Dir(path));
                } else if r.is_archive || Format::detect(&path).is_ok() {
                    self.navigate(Location::Archive { path, inner: String::new() });
                } else if path.is_file() {
                    open_path(&path);
                } else {
                    // 不是可開啟的檔案（例如讀不到的連結）：不交給外部程式，避免跳出系統錯誤視窗
                    self.status_error = Some(format!("無法開啟 {}", r.name));
                }
            }
            Location::Archive { path, inner } => {
                if r.is_dir {
                    let inner = if inner.is_empty() { r.name.clone() } else { format!("{inner}/{}", r.name) };
                    self.navigate(Location::Archive { path, inner });
                } else {
                    let password = self.cache.as_ref().and_then(|c| c.password.clone());
                    let task = Task::OpenInside { archive: path, entry: self.inner_name(&r), strip: inner, password };
                    if is_executable(&r.name) {
                        self.dialog = Some(Dialog::ConfirmRun { task, name: r.name.clone() });
                    } else {
                        self.spawn(task);
                    }
                }
            }
        }
    }

    // ---------- 指令 ----------

    /// 把壓縮檔內的項目拖曳到檔案總管等其他程式：先解到暫存資料夾，再交給 Windows 的拖放（與 7-Zip 相同）。
    /// 放下後由對方複製檔案；暫存資料夾在程式結束時清除。
    fn start_drag_out(&mut self, entries: Vec<String>, frame: &eframe::Frame) {
        if let Some(items) = self.prepare_drag_out(entries) {
            drag_files(frame, items);
        }
    }

    /// 解出要拖曳的項目，回傳暫存資料夾中最上層的檔案與資料夾；無法拖曳時顯示原因並回傳 `None`
    fn prepare_drag_out(&mut self, entries: Vec<String>) -> Option<Vec<PathBuf>> {
        let Location::Archive { path, inner } = self.loc.clone() else { return None };
        // 拖曳前要先解完，太大時畫面會停住太久
        let size: u64 = self.selected_rows().iter().filter_map(|r| r.size).sum();
        if size > DRAG_OUT_MAX {
            self.notice(
                "項目太大，無法拖曳",
                format!("拖曳前需要先解壓縮（約 {}）。超過 {} 時請改用「解壓縮」。", ziptool_core::format_size(size), ziptool_core::format_size(DRAG_OUT_MAX)),
            );
            return None;
        }
        let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let dir = std::env::temp_dir().join("Zipo").join(format!("{}-drag-{stamp}", std::process::id()));
        let password = self.cache.as_ref().and_then(|c| c.password.clone());
        let opts = ziptool_core::ExtractOptions { password, selection: Some(entries), strip_prefix: inner.clone(), ..Default::default() };
        match ziptool_core::extract(&path, &dir, &opts, &ziptool_core::NoProgress) {
            Ok(r) if !r.issues.is_empty() => {
                let i = &r.issues[0];
                self.notice("無法拖曳", format!("{}：{}", i.name, i.reason));
                return None;
            }
            Ok(_) => {}
            Err(e @ (Error::PasswordRequired | Error::WrongPassword)) => {
                // 輸入密碼後重新讀取壓縮檔，之後再拖曳一次即可
                self.dialog = Some(Dialog::Password(PasswordDialog::new(
                    PasswordPurpose::Open { path: path.clone(), inner },
                    file_name(&path),
                    matches!(e, Error::WrongPassword),
                )));
                return None;
            }
            Err(e) => {
                self.notice("無法拖曳", e.to_string());
                return None;
            }
        }
        let items: Vec<PathBuf> = std::fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        (!items.is_empty()).then_some(items)
    }

    /// 啟動時的自動檢查：開啟此功能且距離上次檢查超過一天才連線
    fn auto_update_check(&mut self) {
        let now = unix_now();
        if self.auto_update && now.saturating_sub(self.last_update_check) >= update::CHECK_INTERVAL_SECS {
            self.start_update_check(false);
        }
    }

    fn start_update_check(&mut self, manual: bool) {
        if self.update_work.is_some() {
            return;
        }
        self.last_update_check = unix_now();
        self.update_work = Some(UpdateWork::Check { manual, handle: std::thread::spawn(update::check) });
    }

    fn start_update_install(&mut self, release: update::Release) {
        let Ok(exe) = std::env::current_exe() else {
            return self.notice("無法更新", "找不到目前執行的程式。");
        };
        self.update_work = Some(UpdateWork::Install(std::thread::spawn(move || update::download_and_install(&release, &exe))));
        self.dialog = Some(Dialog::Updating);
    }

    /// 處理線上更新的背景工作結果
    fn poll_update(&mut self, ctx: &egui::Context) {
        let finished = match &self.update_work {
            Some(UpdateWork::Check { handle, .. }) => handle.is_finished(),
            Some(UpdateWork::Install(handle)) => handle.is_finished(),
            None => false,
        };
        if self.update_work.is_some() && !finished {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        if finished {
            match self.update_work.take() {
                Some(UpdateWork::Check { manual, handle }) => match handle.join().unwrap_or_else(|_| Err("檢查更新時發生異常".into())) {
                    Ok(r) if update::is_newer(&r.version, env!("CARGO_PKG_VERSION")) => self.pending_release = Some(r),
                    Ok(_) if manual => self.notice("已是最新版本", format!("目前使用的 Zipo {} 已是最新版本。", env!("CARGO_PKG_VERSION"))),
                    Err(e) if manual => self.notice("無法檢查更新", e),
                    // 自動檢查失敗（例如沒有網路）不打擾使用者
                    _ => {}
                },
                Some(UpdateWork::Install(handle)) => match handle.join().unwrap_or_else(|_| Err("更新時發生異常".into())) {
                    Ok(exe) => {
                        if let Err(e) = std::process::Command::new(&exe).spawn() {
                            self.notice("更新完成", format!("新版本已安裝，但無法自動重新啟動（{e}）。請手動開啟 Zipo。"));
                        } else {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                    Err(e) => {
                        self.dialog = None;
                        self.notice("更新失敗", format!("{e}\n\n原本的程式沒有變動。"));
                    }
                },
                None => {}
            }
        }
        if self.dialog.is_none() && self.job.is_none() && let Some(r) = self.pending_release.take() {
            self.dialog = Some(Dialog::UpdateAvailable(r));
        }
    }

    fn notice(&mut self, title: &str, message: impl Into<String>) {
        self.dialog = Some(Dialog::Notice { title: title.into(), message: message.into(), ok: false, open_dir: None, issues: Vec::new() });
    }

    fn cmd_add(&mut self) {
        if matches!(self.loc, Location::Archive { .. }) {
            if let Some(files) = rfd::FileDialog::new().set_title("選擇要加入壓縮檔的檔案").pick_files() {
                self.confirm_add_into_archive(files);
            }
            return;
        }
        let paths: Vec<PathBuf> = self.selected_rows().iter().filter(|r| !r.is_drive).filter_map(|r| self.row_path(r)).collect();
        if paths.is_empty() {
            return self.notice("加入", "請先選取要加入壓縮檔的檔案或資料夾。");
        }
        self.dialog = Some(Dialog::Add(self.with_open_after_add(AddDialog::new(paths, self.add_format, self.add_level))));
    }

    /// 在壓縮檔內加入檔案（加到目前瀏覽的資料夾）。
    fn confirm_add_into_archive(&mut self, files: Vec<PathBuf>) {
        let Location::Archive { path, inner } = self.loc.clone() else { return };
        if Format::detect(&path).is_ok_and(|f| f.is_single()) {
            return self.notice("加入", "這種格式只能包含單一檔案，無法再加入其他檔案。");
        }
        let names: Vec<String> = files.iter().take(3).map(|p| file_name(p)).collect();
        let more = if files.len() > 3 { format!(" 等 {} 個項目", files.len()) } else { String::new() };
        let place = if inner.is_empty() { "根目錄".to_string() } else { format!("「{inner}」資料夾") };
        let message = format!("要把 {}{more} 加入 {} 的{place}嗎？\n壓縮檔中同名的項目會被取代。", names.join("、"), file_name(&path));
        let password = self.cache.as_ref().and_then(|c| c.password.clone());
        let task = Task::Update { archive: path, add: files, into: inner, remove: Vec::new(), password };
        self.dialog = Some(Dialog::ConfirmUpdate { task, title: "加入到壓縮檔".into(), message, danger: false });
    }

    fn cmd_delete(&mut self) {
        let Location::Archive { path, .. } = self.loc.clone() else {
            return self.notice("刪除", "刪除只能用在壓縮檔內的項目。Zipo 不會刪除你電腦上的檔案。");
        };
        let remove: Vec<String> = self.selected_rows().iter().map(|r| self.inner_name(r)).collect();
        if remove.is_empty() {
            return self.notice("刪除", "請先選取要從壓縮檔刪除的項目。");
        }
        if Format::detect(&path).is_ok_and(|f| f.is_single()) {
            return self.notice("刪除", "這種格式只能包含單一檔案，無法刪除其中的項目。");
        }
        let message = format!(
            "要從 {} 刪除選取的 {} 個項目嗎？\n資料夾會連同其中的內容一起刪除，這個動作無法復原。",
            file_name(&path),
            remove.len()
        );
        let password = self.cache.as_ref().and_then(|c| c.password.clone());
        let task = Task::Update { archive: path, add: Vec::new(), into: String::new(), remove, password };
        self.dialog = Some(Dialog::ConfirmUpdate { task, title: "從壓縮檔刪除".into(), message, danger: true });
    }

    fn cmd_extract(&mut self) {
        let source = match &self.loc {
            Location::Archive { path, inner } => {
                let selection: Vec<String> = self.selected_rows().iter().map(|r| self.inner_name(r)).collect();
                let selection = if selection.is_empty() && inner.is_empty() {
                    None
                } else if selection.is_empty() {
                    Some(vec![inner.clone()])
                } else {
                    Some(selection)
                };
                ExtractSource::Inside { archive: path.clone(), selection, strip: inner.clone() }
            }
            _ => {
                let files: Vec<PathBuf> =
                    self.selected_rows().iter().filter(|r| r.is_archive).filter_map(|r| self.row_path(r)).collect();
                if files.is_empty() {
                    return self.notice("解壓縮", "請先選取要解壓縮的壓縮檔。");
                }
                ExtractSource::Files(files)
            }
        };
        let password = self.cache.as_ref().and_then(|c| c.password.clone());
        self.dialog = Some(Dialog::Extract(self.with_open_after_extract(ExtractDialog::new(source, password))));
    }

    fn cmd_test(&mut self) {
        match &self.loc {
            Location::Archive { path, .. } => {
                let password = self.cache.as_ref().and_then(|c| c.password.clone());
                self.spawn(Task::Test { archives: vec![path.clone()], password });
            }
            _ => {
                let files: Vec<PathBuf> =
                    self.selected_rows().iter().filter(|r| r.is_archive).filter_map(|r| self.row_path(r)).collect();
                if files.is_empty() {
                    return self.notice("測試", "請先選取要測試的壓縮檔。");
                }
                self.spawn(Task::Test { archives: files, password: None });
            }
        }
    }

    fn cmd_info(&mut self) {
        let mut lines = Vec::new();
        let rows = self.selected_rows();
        if rows.is_empty() {
            if let (Location::Archive { path, .. }, Some(c)) = (&self.loc, &self.cache) {
                let files = c.entries.iter().filter(|e| !e.is_dir).count();
                let dirs = c.entries.iter().filter(|e| e.is_dir).count();
                let size: u64 = c.entries.iter().filter_map(|e| e.size).sum();
                let packed = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                lines.push(("路徑".into(), path.display().to_string()));
                lines.push(("格式".into(), Format::detect(path).map(|f| f.name().to_string()).unwrap_or_default()));
                lines.push(("檔案".into(), files.to_string()));
                lines.push(("資料夾".into(), dirs.to_string()));
                lines.push(("原始大小".into(), human(size)));
                lines.push(("壓縮檔大小".into(), human(packed)));
                if size > 0 {
                    lines.push(("壓縮率".into(), format!("{:.1}%", packed as f64 * 100.0 / size as f64)));
                }
                lines.push(("加密".into(), if c.entries.iter().any(|e| e.encrypted) || c.password.is_some() { "是" } else { "否" }.into()));
                let title = file_name(path);
                self.dialog = Some(Dialog::Info { title, lines });
            } else {
                self.notice("內容", "請先選取項目。");
            }
            return;
        }
        let size: u64 = rows.iter().filter_map(|r| r.size).sum();
        if rows.len() == 1 {
            let r = rows[0];
            lines.push(("名稱".into(), r.display.clone()));
            if r.display != r.name {
                lines.push(("實際名稱".into(), r.name.clone()));
            }
            lines.push(("類型".into(), r.type_label()));
            if let Some(p) = self.row_path(r) {
                lines.push(("位置".into(), p.parent().map(|d| d.display().to_string()).unwrap_or_default()));
            } else {
                lines.push(("位置".into(), self.loc.display()));
            }
        } else {
            lines.push(("項目".into(), format!("{} 個", rows.len())));
        }
        lines.push(("大小".into(), format!("{}（{} 位元組）", human(size), size)));
        if rows.iter().any(|r| r.packed.is_some()) {
            let packed: u64 = rows.iter().filter_map(|r| r.packed).sum();
            lines.push(("壓縮後".into(), human(packed)));
        }
        if rows.len() == 1 && rows[0].modified.is_some() {
            lines.push(("修改日期".into(), format_time(rows[0].modified)));
        }
        if rows.iter().any(|r| r.encrypted) {
            lines.push(("加密".into(), "是".into()));
        }
        let title = if rows.len() == 1 { rows[0].display.clone() } else { "內容".into() };
        self.dialog = Some(Dialog::Info { title, lines });
    }

    fn run_action(&mut self, a: Action) {
        match a {
            Action::Open => {
                if let Some(&i) = self.sel.first() {
                    self.open_row(i);
                }
            }
            Action::Add => self.cmd_add(),
            Action::Extract => self.cmd_extract(),
            Action::Test => self.cmd_test(),
            Action::Info => self.cmd_info(),
            Action::Delete => self.cmd_delete(),
            Action::Hash => self.cmd_hash(),
        }
    }

    /// 對話框預設勾選上次的「完成後開啟資料夾」
    fn with_open_after_add(&self, mut d: AddDialog) -> AddDialog {
        d.open_folder = self.open_after;
        d
    }

    fn with_open_after_extract(&self, mut d: ExtractDialog) -> ExtractDialog {
        d.open_folder = self.open_after;
        d
    }

    /// 計算選取項目的雜湊值；壓縮檔內的項目先解到暫存資料夾
    fn cmd_hash(&mut self) {
        let rows: Vec<Row> = self.selected_rows().into_iter().filter(|r| !r.is_drive).cloned().collect();
        if rows.is_empty() {
            return self.notice("計算雜湊值", "請先選取檔案或資料夾。");
        }
        let task = match &self.loc {
            Location::Archive { path, inner } => Task::HashInside {
                archive: path.clone(),
                entries: rows.iter().map(|r| self.inner_name(r)).collect(),
                strip: inner.clone(),
                password: self.cache.as_ref().and_then(|c| c.password.clone()),
            },
            Location::Dir(d) => Task::Hash { inputs: rows.iter().map(|r| d.join(&r.name)).collect() },
            Location::Computer => return,
        };
        self.spawn(task);
    }

    fn spawn(&mut self, task: Task) {
        self.job = Some(Job::spawn(task));
    }

    // ---------- 背景工作 ----------

    fn poll(&mut self, ctx: &egui::Context) {
        if self.loading.as_ref().is_some_and(|l| l.handle.is_finished()) {
            let l = self.loading.take().unwrap();
            match l.handle.join().unwrap_or_else(|_| Err(Error::Unsupported("讀取時發生異常".into()))) {
                Ok(entries) => {
                    let stamp = file_stamp(&l.path).unwrap_or((0, None));
                    let format = Format::detect(&l.path).map(|f| f.name()).unwrap_or("?");
                    self.cache = Some(ArchiveCache { path: l.path.clone(), entries, password: l.password.clone(), stamp, format });
                    self.history_nav = l.from_history;
                    self.navigate(Location::Archive { path: l.path.clone(), inner: l.inner.clone() });
                    self.history_nav = false;
                }
                Err(e @ (Error::PasswordRequired | Error::WrongPassword)) => {
                    let wrong = matches!(e, Error::WrongPassword) || l.password.is_some();
                    let name = file_name(&l.path);
                    self.dialog = Some(Dialog::Password(PasswordDialog::new(
                        PasswordPurpose::Open { path: l.path, inner: l.inner },
                        name,
                        wrong,
                    )));
                }
                Err(e) => {
                    self.status_error = Some(format!("無法開啟 {}：{e}", file_name(&l.path)));
                    self.addr = self.loc.display();
                }
            }
        }

        if self.job.as_ref().is_some_and(|j| j.is_finished()) {
            let (task, res) = self.job.take().unwrap().join();
            if ctx.input(|i| i.viewport().focused) == Some(false) {
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
            }
            if self.close_after_job {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            match res {
                Ok(out) => {
                    if out.refresh {
                        self.refresh();
                    }
                    if out.auto_open
                        && let Some(d) = &out.open_dir
                    {
                        open_path(d);
                    }
                    if !out.hashes.is_empty() {
                        self.dialog = Some(Dialog::Hashes { rows: out.hashes, expected: String::new() });
                    } else if !out.title.is_empty() {
                        self.dialog = Some(Dialog::Notice {
                            title: out.title,
                            message: out.message,
                            ok: out.issues.is_empty(),
                            open_dir: out.open_dir,
                            issues: out.issues,
                        });
                    }
                }
                Err(e @ (Error::PasswordRequired | Error::WrongPassword)) => {
                    let name = match &task {
                        Task::Extract { archives, .. } | Task::Test { archives, .. } => file_name(&archives[0]),
                        Task::OpenInside { archive, .. } => file_name(archive),
                        Task::Compress { output, .. } => file_name(output),
                        Task::Update { archive, .. } | Task::HashInside { archive, .. } => file_name(archive),
                        Task::Hash { inputs } => inputs.first().map(|p| file_name(p)).unwrap_or_default(),
                    };
                    self.dialog = Some(Dialog::Password(PasswordDialog::new(
                        PasswordPurpose::Retry(task),
                        name,
                        matches!(e, Error::WrongPassword),
                    )));
                }
                Err(e @ Error::PossibleBomb { .. }) => {
                    self.dialog = Some(Dialog::ConfirmBomb { task, message: e.to_string() });
                }
                Err(Error::Cancelled) => self.status_error = Some("已取消".into()),
                Err(e) => {
                    self.dialog = Some(Dialog::Notice {
                        title: "發生錯誤".into(),
                        message: e.to_string(),
                        ok: false,
                        open_dir: None,
                        issues: Vec::new(),
                    })
                }
            }
        }
    }

    // ---------- 畫面 ----------

    /// 「檢視」選單：重新整理、排序、顯示系統項目、ZIP 檔名編碼、深淺色
    fn view_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(220.0);
        if ui.button(format!("{}  重新整理\tF5", icon::ARROWS_CLOCKWISE)).clicked() {
            self.refresh();
        }
        ui.separator();
        ui.label(RichText::new("排序方式").size(12.0).color(pal(ui).text2));
        for (key, label) in [(SortKey::Name, "名稱"), (SortKey::Modified, "修改日期"), (SortKey::Type, "類型"), (SortKey::Size, "大小")] {
            if ui.radio(self.sort_key == key, label).clicked() {
                self.sort_key = key;
                self.resort();
            }
        }
        ui.separator();
        if ui
            .checkbox(&mut self.show_system, "顯示受保護的系統項目")
            .on_hover_text("例如家目錄中的 My Documents、Application Data 等相容性連結；檔案總管預設也不顯示")
            .changed()
        {
            self.refresh();
        }
        ui.menu_button("ZIP 檔名編碼", |ui| {
            let current = ziptool_core::zip_codepage();
            let choices = [
                (None, "自動（依系統）"),
                (Some(950), "繁體中文（Big5）"),
                (Some(936), "簡體中文（GBK）"),
                (Some(932), "日文（Shift_JIS）"),
                (Some(949), "韓文（EUC-KR）"),
                (Some(1252), "西歐語系（Windows-1252）"),
            ];
            for (cp, label) in choices {
                if ui.radio(current == cp, label).clicked() {
                    ziptool_core::set_zip_codepage(cp);
                    self.refresh();
                }
            }
        })
        .response
        .on_hover_text("舊式 zip（例如 Windows 內建壓縮產生的）沒有記錄檔名編碼；中文檔名出現亂碼時可在這裡切換");
        ui.separator();
        let dark = ui.visuals().dark_mode;
        let (glyph, label) = if dark { (icon::SUN, "淺色模式") } else { (icon::MOON, "深色模式") };
        if ui.button(format!("{glyph}  {label}")).clicked() {
            ui.ctx().set_theme(if dark { egui::Theme::Light } else { egui::Theme::Dark });
        }
    }

    /// 「⋯」選單：開啟壓縮檔、選取、檔案總管整合、關於、結束
    fn more_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(220.0);
        if ui.button(format!("{}  開啟壓縮檔…", icon::FOLDER_OPEN)).clicked()
            && let Some(p) = rfd::FileDialog::new().pick_file()
        {
            self.open_path_arg(p);
        }
        ui.separator();
        if ui.button(format!("{}  全選\tCtrl+A", icon::CHECK_SQUARE)).clicked() {
            self.sel = (0..self.rows.len()).collect();
        }
        if ui.button(format!("{}  取消選取", icon::SQUARE)).clicked() {
            self.sel.clear();
        }
        if ui.button(format!("{}  反向選取", icon::SWAP)).clicked() {
            self.sel = (0..self.rows.len()).filter(|i| !self.sel.contains(i)).collect();
        }
        ui.separator();
        let installed = shell::is_installed(shell::ROOT);
        let label = if installed { "移除檔案總管右鍵選單" } else { "加入檔案總管右鍵選單" };
        let r = ui.button(format!("{}  {label}", icon::CURSOR_CLICK)).on_hover_text(
            "在檔案總管的右鍵選單加入「加入壓縮檔」「解壓縮到這裡」等項目（Windows 11 位於「顯示其他選項」）。\n只修改目前使用者的設定，隨時可以移除。",
        );
        if r.clicked() {
            let res = if installed {
                shell::uninstall(shell::ROOT)
            } else {
                std::env::current_exe().and_then(|exe| shell::install(shell::ROOT, &exe))
            };
            let (title, msg) = match (res, installed) {
                (Ok(()), false) => ("已加入右鍵選單", "在檔案總管中對檔案或壓縮檔按右鍵即可使用。Windows 11 請先點選「顯示其他選項」。".to_string()),
                (Ok(()), true) => ("已移除右鍵選單", "檔案總管的右鍵選單已恢復原狀。".to_string()),
                (Err(e), _) => ("無法變更右鍵選單", ziptool_core::describe(&e)),
            };
            self.notice(title, msg);
        }
        let r = ui.button(format!("{}  設為壓縮檔預設程式…", icon::ARCHIVE)).on_hover_text(
            "讓 Zipo 成為 .zip、.7z 等壓縮檔的預設開啟程式。\nWindows 不允許程式自行變更預設程式，接下來會開啟「設定」，請在那裡選取 Zipo。",
        );
        if r.clicked() {
            match std::env::current_exe().and_then(|exe| shell::register_default(shell::SOFTWARE, &exe)) {
                Ok(()) => {
                    shell::open_default_apps_settings();
                    self.notice(
                        "請在「設定」中完成",
                        "已開啟 Windows「設定 › 預設應用程式 › Zipo」。請點選 .zip（以及其他想要的格式），再選擇 Zipo。",
                    );
                }
                Err(e) => self.notice("無法註冊預設程式", ziptool_core::describe(&e)),
            }
        }
        if shell::is_default_registered(shell::SOFTWARE) {
            let r = ui.button(format!("{}  移除預設程式註冊", icon::X_CIRCLE)).on_hover_text("從 Windows 的預設程式清單移除 Zipo。");
            if r.clicked() {
                match shell::unregister_default(shell::SOFTWARE) {
                    Ok(()) => self.notice("已移除預設程式註冊", "壓縮檔會改由 Windows 原本的程式開啟。"),
                    Err(e) => self.notice("無法移除預設程式註冊", ziptool_core::describe(&e)),
                }
            }
        }
        ui.separator();
        let checking = matches!(self.update_work, Some(UpdateWork::Check { .. }));
        if ui.add_enabled(!checking, egui::Button::new(format!("{}  檢查更新…", icon::ARROWS_CLOCKWISE))).clicked() {
            self.start_update_check(true);
        }
        ui.checkbox(&mut self.auto_update, "啟動時自動檢查更新");
        if ui.button(format!("{}  關於 Zipo", icon::INFO)).clicked() {
            self.dialog = Some(Dialog::About);
        }
        if ui.button(format!("{}  結束", icon::SIGN_OUT)).clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// 工具列狀態：依目前位置與選取的項目決定每個按鈕能不能按。
    fn toolbar_state(&self) -> ToolbarState {
        let in_archive = matches!(self.loc, Location::Archive { .. });
        let rows = self.selected_rows();
        let has_sel = !rows.is_empty();
        let has_archive = rows.iter().any(|r| r.is_archive);
        let selectable = rows.iter().any(|r| !r.is_drive);
        ToolbarState {
            in_archive,
            compress: in_archive || selectable,
            extract: in_archive || has_archive,
            test: in_archive || has_archive,
            info: has_sel || in_archive,
            delete: in_archive && has_sel,
        }
    }

    /// 工具列：左側為主要指令（20 px 圖示＋文字），右側為「檢視」與「⋯」選單。
    fn command_bar(&mut self, ui: &mut egui::Ui) {
        let mut action = None;
        let st = self.toolbar_state();
        let w = ui.available_width();
        ui.allocate_ui_with_layout(vec2(w, CONTROL_H), Layout::left_to_right(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let mut button = |ui: &mut egui::Ui, enabled: bool, glyph: &str, label: &str, tip: &str, why: &str, a: Action| {
                let r = ui.add_enabled_ui(enabled, |ui| command_button(ui, glyph, Some(label))).inner;
                if r.on_hover_text(tip).on_disabled_hover_text(why).clicked() {
                    action = Some(a);
                }
            };
            if st.in_archive {
                button(ui, true, icon::PLUS_CIRCLE, "加入", "選擇檔案加入目前開啟的壓縮檔", "", Action::Add);
            } else {
                button(ui, st.compress, icon::FILE_ZIP, "壓縮", "把選取的檔案或資料夾壓縮成壓縮檔", "先選取要壓縮的檔案或資料夾", Action::Add);
            }
            let extract_tip = if st.in_archive { "解壓縮選取的項目（未選取則全部）" } else { "解壓縮選取的壓縮檔" };
            button(ui, st.extract, icon::ARROW_SQUARE_OUT, "解壓縮", extract_tip, "選取的項目中沒有壓縮檔", Action::Extract);
            button(ui, st.test, icon::SEAL_CHECK, "測試", "測試壓縮檔是否完整、沒有損壞", "選取的項目中沒有壓縮檔", Action::Test);
            separator_v(ui);
            button(ui, st.info, icon::INFO, "內容", "顯示選取項目的詳細資訊", "先選取項目", Action::Info);
            // 刪除只用在壓縮檔內的項目；在一般資料夾不顯示，避免誤以為會刪除電腦上的檔案
            if st.in_archive {
                button(ui, st.delete, icon::TRASH, "刪除", "從壓縮檔刪除選取的項目（Delete）", "先選取要刪除的項目", Action::Delete);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let more = command_button(ui, icon::DOTS_THREE, None).on_hover_text("更多");
                egui::Popup::menu(&more).show(|ui| self.more_menu(ui));
                let view = command_button(ui, icon::SLIDERS_HORIZONTAL, Some("檢視")).on_hover_text("排序、顯示方式與外觀");
                egui::Popup::menu(&view).show(|ui| self.view_menu(ui));
            });
        });
        if let Some(a) = action {
            self.run_action(a);
        }
    }

    /// 網址列上分段顯示的路徑：每一段都可以點選前往。
    fn breadcrumbs(&self) -> Vec<(String, Location)> {
        fn dir_parts(dir: &Path) -> Vec<(String, Location)> {
            let mut out = vec![("本機".to_string(), Location::Computer)];
            let mut acc = PathBuf::new();
            for c in dir.components() {
                match c {
                    std::path::Component::Prefix(pre) => {
                        acc.push(pre.as_os_str());
                    }
                    std::path::Component::RootDir => {
                        acc.push(std::path::MAIN_SEPARATOR_STR);
                        let label = if cfg!(windows) { localize::drive_display_name(&acc) } else { "/".into() };
                        out.push((label, Location::Dir(acc.clone())));
                    }
                    std::path::Component::Normal(n) => {
                        acc.push(n);
                        let label = localize::path_display_name(&acc).unwrap_or_else(|| n.to_string_lossy().into_owned());
                        out.push((label, Location::Dir(acc.clone())));
                    }
                    _ => {}
                }
            }
            out
        }
        match &self.loc {
            Location::Computer => vec![("本機".into(), Location::Computer)],
            Location::Dir(d) => dir_parts(d),
            Location::Archive { path, inner } => {
                let mut out = path.parent().map(dir_parts).unwrap_or_default();
                out.push((file_name(path), Location::Archive { path: path.clone(), inner: String::new() }));
                let mut acc = String::new();
                for part in inner.split('/').filter(|p| !p.is_empty()) {
                    acc = if acc.is_empty() { part.to_string() } else { format!("{acc}/{part}") };
                    out.push((part.to_string(), Location::Archive { path: path.clone(), inner: acc.clone() }));
                }
                out
            }
        }
    }

    /// 網址列：上一頁／下一頁／上一層（32×32），分段式路徑（點空白處可直接輸入），右側搜尋框。
    fn address_bar(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        let crumbs = self.breadcrumbs();
        let here = crumbs.last().map(|c| c.0.clone()).unwrap_or_default();
        let w = ui.available_width();
        let mut go_to: Option<Location> = None;
        let mut go_typed = false;
        let mut filter_changed = false;
        ui.allocate_ui_with_layout(vec2(w, CONTROL_H), Layout::left_to_right(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if ui.add_enabled_ui(!self.back.is_empty(), |ui| icon_button(ui, icon::ARROW_LEFT)).inner.on_hover_text("上一頁（Alt+←）").clicked() {
                self.go_back();
            }
            if ui.add_enabled_ui(!self.forward.is_empty(), |ui| icon_button(ui, icon::ARROW_RIGHT)).inner.on_hover_text("下一頁（Alt+→）").clicked() {
                self.go_forward();
            }
            if ui.add_enabled_ui(self.loc.parent().is_some(), |ui| icon_button(ui, icon::ARROW_UP)).inner.on_hover_text("上一層（Backspace）").clicked() {
                self.go_up();
            }
            ui.add_space(4.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.allocate_ui_with_layout(vec2(240.0, CONTROL_H), Layout::left_to_right(Align::Center), |ui| {
                    input_frame(ui, icon::MAGNIFYING_GLASS, |ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.filter)
                                .frame(egui::Frame::NONE)
                                .hint_text(format!("搜尋 {here}"))
                                .desired_width(ui.available_width()),
                        );
                        filter_changed = r.changed();
                    });
                });
                ui.add_space(4.0);
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let lead = match self.loc {
                        Location::Computer => icon::DESKTOP,
                        Location::Dir(_) => icon::FOLDER,
                        Location::Archive { .. } => icon::FILE_ZIP,
                    };
                    if self.addr_editing {
                        input_frame(ui, lead, |ui| {
                            let r = ui.add(egui::TextEdit::singleline(&mut self.addr).frame(egui::Frame::NONE).desired_width(ui.available_width()));
                            if !r.has_focus() && !r.lost_focus() {
                                r.request_focus();
                            }
                            if r.lost_focus() {
                                self.addr_editing = false;
                                if ui.input(|i| i.key_pressed(Key::Enter)) {
                                    go_typed = true;
                                } else {
                                    self.addr = self.loc.display();
                                }
                            }
                        });
                    } else {
                        let frame = input_frame(ui, lead, |ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            ui.spacing_mut().button_padding = vec2(4.0, 2.0);
                            // 放不下時，前段收進「…」選單（與檔案總管相同），不在單字中間截斷
                            let widths: Vec<f32> = crumbs
                                .iter()
                                .map(|(l, _)| ui.painter().layout_no_wrap(l.clone(), egui::FontId::proportional(14.0), p.text).size().x + CRUMB_EXTRA)
                                .collect();
                            let first = first_visible_crumb(&widths, ui.available_width(), CRUMB_ELLIPSIS);
                            if first > 0 {
                                let more = ui.add(egui::Button::new(RichText::new("…").color(p.text2)).frame(false)).on_hover_text("顯示上層資料夾");
                                egui::Popup::menu(&more).show(|ui| {
                                    for (label, loc) in crumbs[..first].iter().rev() {
                                        if ui.button(format!("{}  {label}", icon::FOLDER)).clicked() {
                                            go_to = Some(loc.clone());
                                        }
                                    }
                                });
                                ui.label(RichText::new(icon::CARET_RIGHT).size(12.0).color(p.muted));
                            }
                            let n = crumbs.len();
                            for (k, (label, loc)) in crumbs.iter().enumerate().skip(first) {
                                let color = if k + 1 == n { p.text } else { p.text2 };
                                let b = egui::Button::new(RichText::new(label).color(color)).frame(false).truncate();
                                if ui.add(b).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                    go_to = Some(loc.clone());
                                }
                                if k + 1 < n {
                                    ui.label(RichText::new(icon::CARET_RIGHT).size(12.0).color(p.muted));
                                }
                            }
                        });
                        // 點路徑旁的空白處：切換成可直接輸入
                        if go_to.is_none() && frame.interact(Sense::click()).on_hover_cursor(egui::CursorIcon::Text).clicked() {
                            self.addr = self.loc.display();
                            self.addr_editing = true;
                        }
                    }
                });
            });
        });
        if filter_changed {
            self.sel.clear();
            self.anchor = None;
            self.resort();
        }
        if let Some(loc) = go_to {
            self.navigate(loc);
        }
        if go_typed {
            self.go_address();
        }
    }

    fn file_table(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        if let Some(l) = &self.loading {
            let mut stop = false;
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() / 2.0 - 40.0).max(0.0));
                ui.spinner();
                ui.label(RichText::new(format!("正在讀取 {}…", file_name(&l.path))).color(p.text2));
                stop = ui.add(secondary_button(ui, "取消")).clicked();
            });
            if stop {
                // 放棄這次讀取；背景執行緒讀完後結果會被丟棄
                self.loading = None;
                self.addr = self.loc.display();
            }
            return;
        }
        let in_archive = matches!(self.loc, Location::Archive { .. });
        let mut click: Option<(usize, Modifiers)> = None;
        let mut double: Option<usize> = None;
        let mut ctx_action: Option<Action> = None;
        let mut right_click: Option<usize> = None;
        let mut drag_row: Option<usize> = None;
        let mut sort_click: Option<SortKey> = None;
        let modifiers = ui.input(|i| i.modifiers);

        // 欄位順序與檔案總管相同：名稱、修改日期、類型（壓縮檔內為壓縮後大小）、大小
        let cols: Vec<(SortKey, &str, bool)> = if in_archive {
            vec![(SortKey::Name, "名稱", false), (SortKey::Modified, "修改日期", false), (SortKey::Packed, "壓縮後", true), (SortKey::Size, "大小", true)]
        } else {
            vec![(SortKey::Name, "名稱", false), (SortKey::Modified, "修改日期", false), (SortKey::Type, "類型", false), (SortKey::Size, "大小", true)]
        };

        let mut table = TableBuilder::new(ui)
            .id_salt(("files", in_archive))
            .striped(false)
            .resizable(true)
            .sense(Sense::click_and_drag())
            .auto_shrink(false)
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::remainder().at_least(240.0).clip(true))
            .column(Column::initial(150.0).at_least(110.0))
            .column(Column::initial(if in_archive { 100.0 } else { 130.0 }).at_least(70.0))
            .column(Column::initial(100.0).at_least(70.0));
        if let Some(i) = self.scroll_to.take() {
            table = table.scroll_to_row(i, None);
        }

        let (sort_key, sort_asc) = (self.sort_key, self.sort_asc);
        // 第一欄內縮 16 px：加上清單外框的 4 px，圖示與標題恰好落在 CONTENT_X；最後一欄右側留 8 px
        table
            .header(CONTROL_H, |mut header| {
                for (k, (key, label, right)) in cols.iter().enumerate() {
                    header.col(|ui| {
                        let layout = if *right { Layout::right_to_left(Align::Center) } else { Layout::left_to_right(Align::Center) };
                        ui.with_layout(layout, |ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            if k == 0 {
                                ui.add_space(CONTENT_X - 4.0);
                            } else if k + 1 == cols.len() {
                                ui.add_space(8.0);
                            }
                            let arrow = if *key == sort_key { if sort_asc { icon::CARET_UP } else { icon::CARET_DOWN } } else { "" };
                            let text = if *right { format!("{arrow} {label}") } else { format!("{label} {arrow}") };
                            let b = egui::Button::new(RichText::new(text.trim()).size(12.0).color(p.text2)).frame(false);
                            if ui.add(b).clicked() {
                                sort_click = Some(*key);
                            }
                        });
                    });
                }
            })
            .body(|body| {
                body.rows(CONTROL_H, self.rows.len(), |mut row| {
                    let i = row.index();
                    let r = &self.rows[i];
                    row.set_selected(self.sel.contains(&i));
                    row.col(|ui| {
                        ui.add_space(CONTENT_X - 4.0);
                        ui.label(RichText::new(row_icon(r)).size(16.0).color(row_icon_color(r, p)));
                        let name = ui.add(egui::Label::new(RichText::new(&r.display).color(p.text)).truncate().selectable(false));
                        if r.display != r.name {
                            name.on_hover_text(format!("實際名稱：{}", r.name));
                        }
                        if r.encrypted {
                            ui.label(RichText::new(icon::LOCK_SIMPLE).size(14.0).color(p.text2)).on_hover_text("已加密");
                        }
                    });
                    let num = |v: Option<u64>| v.map(human).unwrap_or_default();
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(format_time(r.modified)).color(p.text2)).truncate().selectable(false));
                    });
                    row.col(|ui| {
                        if in_archive {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new(num(r.packed)).color(p.text2));
                            });
                        } else {
                            ui.add(egui::Label::new(RichText::new(r.type_label()).color(p.text2)).truncate().selectable(false));
                        }
                    });
                    row.col(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.add_space(8.0);
                            ui.label(RichText::new(num(r.size)).color(p.text2));
                        });
                    });

                    let resp = row.response();
                    if resp.double_clicked() {
                        double = Some(i);
                    } else if resp.clicked() {
                        click = Some((i, modifiers));
                    }
                    if resp.secondary_clicked() {
                        right_click = Some(i);
                    }
                    if in_archive && resp.drag_started() {
                        drag_row = Some(i);
                    }
                    resp.context_menu(|ui| {
                        ui.set_min_width(170.0);
                        let is_archive_file = r.is_archive;
                        if ui.button(format!("{}  開啟", icon::FOLDER_OPEN)).clicked() {
                            ctx_action = Some(Action::Open);
                        }
                        ui.separator();
                        if !in_archive && !r.is_drive && ui.button(format!("{}  壓縮…", icon::FILE_ARCHIVE)).clicked() {
                            ctx_action = Some(Action::Add);
                        }
                        if (in_archive || is_archive_file) && ui.button(format!("{}  解壓縮…", icon::ARROW_SQUARE_OUT)).clicked() {
                            ctx_action = Some(Action::Extract);
                        }
                        if (in_archive || is_archive_file) && ui.button(format!("{}  測試", icon::SEAL_CHECK)).clicked() {
                            ctx_action = Some(Action::Test);
                        }
                        if in_archive && ui.button(format!("{}  從壓縮檔刪除", icon::TRASH)).clicked() {
                            ctx_action = Some(Action::Delete);
                        }
                        if !r.is_drive && ui.button(format!("{}  計算雜湊值", icon::FINGERPRINT)).clicked() {
                            ctx_action = Some(Action::Hash);
                        }
                        ui.separator();
                        if ui.button(format!("{}  內容", icon::INFO)).clicked() {
                            ctx_action = Some(Action::Info);
                        }
                    });
                });
            });

        if let Some(key) = sort_click {
            if self.sort_key == key {
                self.sort_asc = !self.sort_asc;
            } else {
                self.sort_key = key;
                self.sort_asc = true;
            }
            self.resort();
        }
        if let Some(i) = right_click
            && !self.sel.contains(&i)
        {
            self.sel = [i].into();
            self.anchor = Some(i);
        }
        if let Some((i, m)) = click {
            self.select_click(i, m);
        }
        // 從壓縮檔內拖曳出去：拖曳未選取的項目時只拖曳該項目，否則拖曳全部選取的項目
        if let Some(i) = drag_row {
            if !self.sel.contains(&i) {
                self.sel = [i].into();
                self.anchor = Some(i);
            }
            self.drag_out = Some(self.selected_rows().iter().map(|r| self.inner_name(r)).collect());
        }
        if let Some(i) = double {
            self.sel = [i].into();
            self.open_row(i);
        } else if let Some(a) = ctx_action {
            self.run_action(a);
        }
    }

    fn select_click(&mut self, i: usize, m: Modifiers) {
        if m.shift
            && let Some(a) = self.anchor
        {
            let (lo, hi) = if a <= i { (a, i) } else { (i, a) };
            if !m.command {
                self.sel.clear();
            }
            self.sel.extend(lo..=hi);
            return;
        }
        if m.command {
            if !self.sel.remove(&i) {
                self.sel.insert(i);
            }
        } else {
            self.sel = [i].into();
        }
        self.anchor = Some(i);
    }

    /// 狀態列：12 px 說明文字（與檔案總管相同的寫法）
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        let w = ui.available_width();
        ui.allocate_ui_with_layout(vec2(w, 20.0), Layout::left_to_right(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            if let Some(e) = &self.status_error {
                ui.label(RichText::new(format!("{}  {e}", icon::WARNING_CIRCLE)).size(12.0).color(p.danger));
                return;
            }
            ui.label(RichText::new(format!("{} 個項目", self.rows.len())).size(12.0).color(p.text2));
            if !self.sel.is_empty() {
                let size: u64 = self.sel.iter().filter_map(|&i| self.rows[i].size).sum();
                let has_size = self.sel.iter().any(|&i| self.rows[i].size.is_some_and(|s| s > 0));
                let size = if has_size { format!("，{}", human(size)) } else { String::new() };
                ui.label(RichText::new(format!("已選取 {} 個項目{size}", self.sel.len())).size(12.0).color(p.text2));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let (Location::Archive { .. }, Some(c)) = (&self.loc, &self.cache) {
                    let (fmt, size) = (c.format, c.stamp.0);
                    let enc = if c.entries.iter().any(|e| e.encrypted) || c.password.is_some() { format!("  ·  {} 已加密", icon::LOCK_SIMPLE) } else { String::new() };
                    ui.label(RichText::new(format!("{fmt} 壓縮檔  ·  {}{enc}", human(size))).size(12.0).color(p.text2));
                }
            });
        });
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if self.busy() || ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        let (alt_left, alt_right) = ctx.input(|i| (i.modifiers.alt && i.key_pressed(Key::ArrowLeft), i.modifiers.alt && i.key_pressed(Key::ArrowRight)));
        if ctx.input(|i| i.key_pressed(Key::Delete)) && matches!(self.loc, Location::Archive { .. }) && !self.sel.is_empty() {
            return self.cmd_delete();
        }
        if alt_left {
            return self.go_back();
        }
        if alt_right {
            return self.go_forward();
        }
        // 直接打字：跳到名稱以此開頭的項目（一秒內連續輸入會累加）
        let typed: String = ctx.input(|i| {
            if i.modifiers.command || i.modifiers.alt {
                return String::new();
            }
            i.events.iter().filter_map(|e| if let egui::Event::Text(t) = e { Some(t.as_str()) } else { None }).collect()
        });
        if !typed.is_empty() {
            if self.typeahead_at.elapsed() > Duration::from_secs(1) {
                self.typeahead.clear();
            }
            self.typeahead.push_str(&typed.to_lowercase());
            self.typeahead_at = std::time::Instant::now();
            if let Some(i) = self.rows.iter().position(|r| r.display.to_lowercase().starts_with(&self.typeahead) || r.name.to_lowercase().starts_with(&self.typeahead)) {
                self.sel = [i].into();
                self.anchor = Some(i);
                self.scroll_to = Some(i);
            }
            return;
        }
        let (enter, back, all, f5, up, down, home, end) = ctx.input(|i| {
            (
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::Backspace),
                i.modifiers.command && i.key_pressed(Key::A),
                i.key_pressed(Key::F5),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::Home),
                i.key_pressed(Key::End),
            )
        });
        if enter {
            self.run_action(Action::Open);
        } else if back {
            self.go_up();
        } else if all {
            self.sel = (0..self.rows.len()).collect();
        } else if f5 {
            self.refresh();
        } else if !self.rows.is_empty() && (up || down || home || end) {
            let last = self.rows.len() - 1;
            let cur = self.anchor.unwrap_or(0);
            let next = if home { 0 } else if end { last } else if up { cur.saturating_sub(1) } else { (cur + 1).min(last) };
            let next = if self.sel.is_empty() && (up || down) { 0 } else { next };
            self.sel = [next].into();
            self.anchor = Some(next);
            self.scroll_to = Some(next);
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        // 從 Zipo 拖曳出去的暫存檔又被放回視窗時忽略，避免加入壓縮檔
        let own = std::env::temp_dir().join("Zipo");
        let dropped: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.starts_with(&own)).collect());
        if dropped.is_empty() || self.busy() {
            return;
        }
        if matches!(self.loc, Location::Archive { .. }) {
            return self.confirm_add_into_archive(dropped);
        }
        if dropped.len() == 1 && dropped[0].is_file() && Format::from_file_name(&file_name(&dropped[0])).is_some() {
            self.open_path_arg(dropped[0].clone());
        } else {
            self.dialog = Some(Dialog::Add(self.with_open_after_add(AddDialog::new(dropped, self.add_format, self.add_level))));
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.dialog.take() else { return };
        let mut keep = true;
        let modal = egui::Modal::new(egui::Id::new("dialog")).frame(dialog_frame(ctx));
        let resp = modal.show(ctx, |ui| match &mut dialog {
            Dialog::Add(d) => match d.ui(ui) {
                Outcome::Pending => {}
                Outcome::Cancel => keep = false,
                Outcome::Ok(task) => {
                    keep = false;
                    (self.add_format, self.add_level) = d.choice();
                    self.open_after = d.open_folder;
                    self.spawn(task);
                }
            },
            Dialog::Extract(d) => match d.ui(ui) {
                Outcome::Pending => {}
                Outcome::Cancel => keep = false,
                Outcome::Ok(task) => {
                    keep = false;
                    self.open_after = d.open_folder;
                    self.spawn(task);
                }
            },
            Dialog::Password(d) => match d.ui(ui) {
                Outcome::Pending => {}
                Outcome::Cancel => {
                    keep = false;
                    self.addr = self.loc.display();
                }
                Outcome::Ok(pw) => {
                    keep = false;
                    let purpose = std::mem::replace(&mut d.purpose, PasswordPurpose::Open { path: PathBuf::new(), inner: String::new() });
                    match purpose {
                        PasswordPurpose::Open { path, inner } => self.start_loading(path, inner, Some(pw)),
                        PasswordPurpose::Retry(mut task) => {
                            task.set_password(pw);
                            self.spawn(task);
                        }
                    }
                }
            },
            Dialog::Info { title, lines } => {
                let p = pal(ui);
                ui.set_width(440.0);
                dialog_title(ui, icon::INFO, title);
                egui::Grid::new("info").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                    for (k, v) in lines.iter() {
                        ui.label(RichText::new(k).color(p.text2));
                        ui.add(egui::Label::new(RichText::new(v).color(p.text)).wrap());
                        ui.end_row();
                    }
                });
                let (ok, cancel) = dialog_buttons_single(ui);
                keep = !(ok || cancel);
            }
            Dialog::Notice { title, message, ok, open_dir, issues } => {
                let p = pal(ui);
                ui.set_width(460.0);
                let (glyph, color) = if *ok { (icon::CHECK_CIRCLE, p.success) } else { (icon::INFO, p.accent) };
                let (glyph, color) =
                    if title == "發生錯誤" || !issues.is_empty() { (icon::WARNING_CIRCLE, p.danger) } else { (glyph, color) };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(glyph).size(22.0).color(color));
                    ui.label(RichText::new(title.as_str()).size(16.0).strong().color(p.text));
                });
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(message.as_str()).color(p.text)).wrap());
                if !issues.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new(format!("略過 {} 個項目：", issues.len())).strong().color(p.text));
                    egui::Frame::new().fill(p.sunken).corner_radius(6).inner_margin(8).show(ui, |ui| {
                        egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            for i in issues.iter() {
                                ui.add(egui::Label::new(RichText::new(&i.name).size(12.5).color(p.text)).wrap());
                                ui.label(RichText::new(format!("    {}", i.reason)).size(12.0).color(p.text2));
                            }
                        });
                    });
                }
                ui.add_space(6.0);
                let mut close = false;
                ui.allocate_ui_with_layout(vec2(ui.available_width(), 32.0), Layout::right_to_left(Align::Center), |ui| {
                    close = ui.add(primary_button(ui, "確定")).clicked();
                    if let Some(d) = open_dir
                        && ui.add(secondary_button(ui, "開啟資料夾")).clicked()
                    {
                        open_path(d);
                        close = true;
                    }
                });
                close |= ui.input(|i| i.key_pressed(Key::Enter) || i.key_pressed(Key::Escape));
                keep = !close;
            }
            Dialog::ConfirmBomb { task, message } => {
                let p = pal(ui);
                ui.set_width(440.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::WARNING).size(22.0).color(p.danger));
                    ui.label(RichText::new("這個壓縮檔的壓縮比異常高").size(16.0).strong().color(p.text));
                });
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(message.as_str()).color(p.text)).wrap());
                ui.label(RichText::new("惡意的壓縮檔可能解出遠超過預期的資料、塞滿磁碟。只有在確定來源可信時才繼續。").color(p.text2));
                ui.add_space(6.0);
                let (mut go, mut stop) = (false, false);
                ui.allocate_ui_with_layout(vec2(ui.available_width(), 32.0), Layout::right_to_left(Align::Center), |ui| {
                    stop = ui.add(primary_button(ui, "取消")).clicked();
                    go = ui.add(secondary_button(ui, "仍要解壓縮")).clicked();
                });
                stop |= ui.input(|i| i.key_pressed(Key::Enter) || i.key_pressed(Key::Escape));
                if go {
                    let mut t = task.clone();
                    t.allow_high_ratio();
                    self.spawn(t);
                }
                keep = !(go || stop);
            }
            Dialog::ConfirmUpdate { task, title, message, danger } => {
                let p = pal(ui);
                ui.set_width(440.0);
                let (glyph, color) = if *danger { (icon::TRASH, p.danger) } else { (icon::PLUS_CIRCLE, p.accent) };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(glyph).size(22.0).color(color));
                    ui.label(RichText::new(title.as_str()).size(16.0).strong().color(p.text));
                });
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(message.as_str()).color(p.text)).wrap());
                ui.add_space(6.0);
                let (mut go, mut stop) = (false, false);
                ui.allocate_ui_with_layout(vec2(ui.available_width(), 32.0), Layout::right_to_left(Align::Center), |ui| {
                    stop = ui.add(secondary_button(ui, "取消")).clicked();
                    go = ui.add(primary_button(ui, if *danger { "刪除" } else { "加入" })).clicked();
                });
                // 刪除不接受 Enter，避免誤按
                go |= !*danger && ui.input(|i| i.key_pressed(Key::Enter));
                stop |= ui.input(|i| i.key_pressed(Key::Escape));
                if go {
                    self.spawn(task.clone());
                }
                keep = !(go || stop);
            }
            Dialog::ConfirmRun { task, name } => {
                let p = pal(ui);
                ui.set_width(440.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::WARNING).size(22.0).color(p.danger));
                    ui.label(RichText::new("要執行壓縮檔中的程式嗎？").size(16.0).strong().color(p.text));
                });
                ui.add_space(8.0);
                ui.label(RichText::new(name.as_str()).strong().color(p.text));
                ui.label(RichText::new("這是可執行的檔案。來源不明的壓縮檔可能夾帶惡意程式，只有在確定來源可信時才執行。").color(p.text2));
                ui.add_space(6.0);
                let (mut go, mut stop) = (false, false);
                ui.allocate_ui_with_layout(vec2(ui.available_width(), 32.0), Layout::right_to_left(Align::Center), |ui| {
                    stop = ui.add(primary_button(ui, "取消")).clicked();
                    go = ui.add(secondary_button(ui, "執行")).clicked();
                });
                stop |= ui.input(|i| i.key_pressed(Key::Enter) || i.key_pressed(Key::Escape));
                if go {
                    self.spawn(task.clone());
                }
                keep = !(go || stop);
            }
            Dialog::About => {
                let p = pal(ui);
                ui.set_width(380.0);
                dialog_title(ui, icon::FILE_ZIP, &format!("Zipo {}", env!("CARGO_PKG_VERSION")));
                ui.label(RichText::new("以 7-Zip 為藍本、使用 Rust 撰寫的壓縮工具。").color(p.text));
                ui.label(RichText::new("支援 7z、zip、tar、tar.gz、tar.bz2、tar.xz、tar.zst、gz、bz2、xz、zst。").color(p.text2));
                let (ok, cancel) = dialog_buttons_single(ui);
                keep = !(ok || cancel);
            }
            Dialog::UpdateAvailable(release) => {
                let p = pal(ui);
                ui.set_width(440.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::ARROW_CIRCLE_UP).size(22.0).color(p.accent));
                    ui.label(RichText::new(format!("有新版本：Zipo {}", release.version)).size(16.0).strong().color(p.text));
                });
                ui.add_space(8.0);
                let detail = match &release.asset {
                    Some(a) => format!(
                        "目前使用的是 {}。
更新檔約 {}，驗證完整後會取代程式並自動重新啟動。",
                        env!("CARGO_PKG_VERSION"),
                        ziptool_core::format_size(a.size)
                    ),
                    None => format!("目前使用的是 {}。
這個版本需要到下載頁面手動更新。", env!("CARGO_PKG_VERSION")),
                };
                ui.add(egui::Label::new(RichText::new(detail).color(p.text)).wrap());
                if link(ui, "查看更新內容").clicked() {
                    open_path(Path::new(&release.page));
                }
                ui.add_space(6.0);
                let (mut go, mut later) = (false, false);
                ui.allocate_ui_with_layout(vec2(ui.available_width(), 32.0), Layout::right_to_left(Align::Center), |ui| {
                    later = ui.add(secondary_button(ui, "稍後")).clicked();
                    go = ui.add(primary_button(ui, if release.asset.is_some() { "立即更新" } else { "開啟下載頁面" })).clicked();
                });
                later |= ui.input(|i| i.key_pressed(Key::Escape));
                if go {
                    if release.asset.is_some() {
                        let r = release.clone();
                        self.start_update_install(r);
                    } else {
                        open_path(Path::new(&release.page));
                    }
                }
                keep = !(go || later);
            }
            Dialog::Hashes { rows, expected } => {
                let p = pal(ui);
                ui.set_width(640.0);
                dialog_title(ui, icon::FINGERPRINT, "雜湊值");
                egui::ScrollArea::vertical().max_height(360.0).auto_shrink([false, true]).show(ui, |ui| {
                    for (name, h) in rows.iter() {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(name.as_str()).strong().color(p.text));
                            ui.label(RichText::new(human(h.size)).color(p.text2));
                        });
                        egui::Grid::new(("hash", name.as_str())).num_columns(3).spacing([12.0, 4.0]).show(ui, |ui| {
                            for (algo, value) in h.all() {
                                ui.label(RichText::new(algo).color(p.text2));
                                ui.add(egui::Label::new(RichText::new(value).monospace().color(p.text)).selectable(true));
                                if ui.small_button(icon::COPY).on_hover_text("複製").clicked() {
                                    ui.ctx().copy_text(value.to_string());
                                }
                                ui.end_row();
                            }
                        });
                        ui.add_space(10.0);
                    }
                });
                ui.add_space(4.0);
                ui.label(RichText::new("比對：貼上網站提供的雜湊值").color(p.text2));
                ui.add(egui::TextEdit::singleline(expected).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace));
                if !expected.trim().is_empty() {
                    let hit = rows.iter().find_map(|(name, h)| h.matches(expected).map(|algo| (name, algo)));
                    match hit {
                        Some((name, algo)) => ui.label(RichText::new(format!("{}  符合「{name}」的 {algo}", icon::CHECK_CIRCLE)).color(p.success)),
                        None => ui.label(RichText::new(format!("{}  不符合任何一個檔案的雜湊值", icon::X_CIRCLE)).color(p.danger)),
                    };
                }
                let (ok, cancel) = dialog_buttons_single(ui);
                keep = !(ok || cancel);
            }
            Dialog::Updating => {
                let p = pal(ui);
                ui.set_width(400.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(20.0).color(p.accent));
                    ui.label(RichText::new("正在更新 Zipo…").size(16.0).strong().color(p.text));
                });
                ui.add_space(8.0);
                ui.label(RichText::new("正在下載並驗證更新檔，完成後會自動重新啟動。").color(p.text2));
                // 更新完成或失敗時由 poll_update 換成其他對話框
                keep = true;
            }
        });
        // 進行中的對話框不因點擊背景而關閉，避免誤觸
        let _ = resp.should_close();
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        }
    }

    fn progress_window(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        let s = job.snapshot();
        let title = job.task.title();
        let mut cancel = false;
        let mut answer = None;
        egui::Modal::new(egui::Id::new("progress")).frame(dialog_frame(ctx)).show(ctx, |ui| {
            let p = pal(ui);
            ui.set_width(460.0);
            dialog_title(ui, icon::HOURGLASS_MEDIUM, title);
            if !s.stage.is_empty() {
                ui.label(RichText::new(&s.stage).color(p.text));
            }
            ui.add(egui::Label::new(RichText::new(&s.current).size(12.5).color(p.text2)).truncate());
            ui.add_space(4.0);
            let frac = if s.total > 0 { s.done as f32 / s.total as f32 } else { 0.0 };
            progress_bar(ui, frac);
            ui.add_space(6.0);
            if let Some(q) = &s.question {
                // 與 7-Zip 的「確認取代檔案」相同的選項
                egui::Frame::new().fill(p.sunken).corner_radius(6).inner_margin(10).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("目的資料夾已經有同名的檔案：").strong().color(p.text));
                    ui.add(egui::Label::new(RichText::new(q.display().to_string()).size(12.5).color(p.text2)).wrap());
                    if let Ok(m) = std::fs::metadata(q) {
                        let info = format!("現有檔案：{}，{}", human(m.len()), format_time(m.modified().ok()));
                        ui.label(RichText::new(info).size(12.5).color(p.text2));
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new("要用壓縮檔中的檔案取代嗎？").color(p.text));
                    ui.horizontal_wrapped(|ui| {
                        use ziptool_core::OverwriteAnswer as A;
                        let choices = [("取代", A::Yes), ("全部取代", A::YesAll), ("略過", A::No), ("全部略過", A::NoAll), ("全部自動重新命名", A::RenameAll)];
                        for (label, a) in choices {
                            if ui.add(secondary_button(ui, label)).clicked() {
                                answer = Some(a);
                            }
                        }
                    });
                });
                ui.add_space(6.0);
            }
            egui::Grid::new("prog").num_columns(4).spacing([14.0, 4.0]).show(ui, |ui| {
                let k = |ui: &mut egui::Ui, t: &str| ui.label(RichText::new(t).size(12.5).color(p.text2));
                let v = |ui: &mut egui::Ui, t: String| ui.label(RichText::new(t).size(12.5).color(p.text));
                k(ui, "已經過");
                v(ui, format_duration(s.elapsed));
                k(ui, "進度");
                v(ui, format!("{:.0}%", frac * 100.0));
                ui.end_row();
                k(ui, "剩餘時間");
                v(ui, s.remaining.map(format_duration).unwrap_or_else(|| "--:--:--".into()));
                k(ui, "已處理");
                v(ui, format!("{} / {}", human(s.done), human(s.total)));
                ui.end_row();
                k(ui, "速度");
                v(ui, format!("{}/s", human(s.speed as u64)));
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 32.0), Layout::right_to_left(Align::Center), |ui| {
                let label = if s.cancelling { "正在取消…" } else { "取消" };
                cancel = ui.add_enabled(!s.cancelling, secondary_button(ui, label)).clicked();
            });
        });
        if let Some(a) = answer {
            job.answer(a);
        }
        if cancel {
            job.cancel();
        }
    }
}

/// 網址列每一段除了文字以外的寬度（按鈕內距 8＋分隔箭頭 12＋間距 4）
const CRUMB_EXTRA: f32 = 24.0;
/// 「…」按鈕加上其後分隔箭頭的寬度
const CRUMB_ELLIPSIS: f32 = 36.0;

/// 從右往左放入路徑分段，回傳第一個能完整顯示的分段索引；最後一段（目前位置）一定顯示。
fn first_visible_crumb(widths: &[f32], avail: f32, ellipsis: f32) -> usize {
    let total: f32 = widths.iter().sum();
    if total <= avail || widths.len() <= 1 {
        return 0;
    }
    let mut used = ellipsis;
    let mut first = widths.len();
    for (i, w) in widths.iter().enumerate().rev() {
        if first < widths.len() && used + w > avail {
            break;
        }
        used += w;
        first = i;
    }
    first
}

/// 視窗內所有區塊第一個看得見的圖示或文字的 x 座標
const CONTENT_X: f32 = 20.0;

/// 對話框外框：Windows 11 ContentDialog 樣式（8 px 圓角、24 px 內距）
fn dialog_frame(ctx: &egui::Context) -> egui::Frame {
    egui::Frame::window(&ctx.style_of(ctx.theme())).corner_radius(8).inner_margin(24)
}

/// 只有一個「確定」按鈕的對話框底部。
fn dialog_buttons_single(ui: &mut egui::Ui) -> (bool, bool) {
    let mut ok = false;
    ui.add_space(6.0);
    ui.allocate_ui_with_layout(vec2(ui.available_width(), 32.0), Layout::right_to_left(Align::Center), |ui| {
        ok = ui.add(primary_button(ui, "確定")).clicked();
    });
    let keys = ui.input(|i| i.key_pressed(Key::Enter) || i.key_pressed(Key::Escape));
    (ok, keys)
}

/// 會直接執行程式碼的檔案類型；從壓縮檔中開啟前要先確認。
fn is_executable(name: &str) -> bool {
    const EXT: [&str; 55] = [
        "exe", "com", "bat", "cmd", "ps1", "psm1", "psd1", "ps1xml", "psc1", "vbs", "vbe", "vb", "js", "jse", "wsf", "wsh", "ws",
        "wsc", "sct", "msi", "msp", "mst", "scr", "lnk", "hta", "cpl", "reg", "jar", "pif", "msc", "chm", "url", "website", "scf",
        "inf", "xll", "gadget", "diagcab", "mht", "mhtml", "library-ms", "search-ms", "searchconnector-ms", "settingcontent-ms",
        "appinstaller", "msix", "msixbundle", "appx", "appxbundle", "application", "appref-ms", "iso", "img", "vhd", "vhdx",
    ];
    name.rsplit_once('.').is_some_and(|(_, e)| EXT.contains(&e.to_ascii_lowercase().as_str()))
}

/// 拖曳到程式外時先解壓縮的大小上限（解壓縮期間畫面會停住）
const DRAG_OUT_MAX: u64 = 256 << 20;

/// 交給系統的拖放；只複製，不移動
#[cfg(windows)]
fn drag_files(frame: &eframe::Frame, items: Vec<PathBuf>) {
    let options = drag::Options { skip_animatation_on_cancel_or_failure: false, mode: drag::DragMode::Copy };
    let image = drag::Image::Raw(drag_image());
    let _ = drag::start_drag(frame, drag::DragItem::Files(items), image, |_, _| {}, options);
}

#[cfg(not(windows))]
fn drag_files(_frame: &eframe::Frame, _items: Vec<PathBuf>) {}

/// 拖曳時跟著游標的圖示（64 px 的程式圖示）
#[cfg(windows)]
fn drag_image() -> Vec<u8> {
    static PNG: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    PNG.get_or_init(|| {
        let Ok(img) = image::load_from_memory(include_bytes!("../assets/ziptool-256.png")) else { return Vec::new() };
        let small = img.resize(64, 64, image::imageops::FilterType::Triangle);
        let mut out = std::io::Cursor::new(Vec::new());
        small.write_to(&mut out, image::ImageFormat::Png).map(|_| out.into_inner()).unwrap_or_default()
    })
    .clone()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// 清除開啟壓縮檔內檔案時留下的暫存資料夾：
/// `only_mine` 為真時刪除本程序建立的；否則刪除超過一天的舊資料夾（上次異常結束留下的）。
/// 檔案仍被其他程式開著時刪除會失敗，直接略過。
fn cleanup_temp(only_mine: bool) {
    let base = std::env::temp_dir().join("Zipo");
    let Ok(dir) = std::fs::read_dir(&base) else { return };
    let prefix = format!("{}-", std::process::id());
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d > Duration::from_secs(86_400));
        if (only_mine && name.starts_with(&prefix)) || (!only_mine && old) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}


/// `prev` 是 `cur` 底下的直接子項目時，回傳其名稱。
fn child_name(cur: &Location, prev: &Location) -> Option<String> {
    match (cur, prev) {
        (Location::Computer, Location::Dir(p)) => p.components().next().map(|c| {
            let mut s = c.as_os_str().to_string_lossy().into_owned();
            if !s.ends_with('\\') && !s.ends_with('/') {
                s.push(std::path::MAIN_SEPARATOR);
            }
            s
        }),
        (Location::Dir(d), Location::Dir(p)) if p.parent() == Some(d.as_path()) => Some(file_name(p)),
        (Location::Dir(d), Location::Archive { path, inner }) if inner.is_empty() && path.parent() == Some(d.as_path()) => {
            Some(file_name(path))
        }
        (Location::Archive { path: a, inner: ci }, Location::Archive { path: b, inner: pi }) if a == b => {
            let rest = if ci.is_empty() { pi.as_str() } else { pi.strip_prefix(&format!("{ci}/"))? };
            (!rest.is_empty() && !rest.contains('/')).then(|| rest.to_string())
        }
        _ => None,
    }
}

/// 測試用：程式內部的路徑一律是實際路徑（例如 macOS 的 /var 實際上是 /private/var），比對時也要用實際路徑
#[cfg(test)]
fn real_path(p: &Path) -> PathBuf {
    strip_verbatim(std::fs::canonicalize(p).unwrap())
}

/// Windows 的 canonicalize 會加上 `\\?\` 前綴，顯示時去掉。
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => p,
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.save_settings(storage);
    }

    /// 程式結束時清除本程式建立的暫存資料夾（開啟壓縮檔內的檔案、拖曳到程式外）
    fn on_exit(&mut self) {
        cleanup_temp(true);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        FIRST_FRAME.store(true, std::sync::atomic::Ordering::Relaxed);
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested())
            && let Some(job) = &self.job
        {
            job.cancel();
            self.close_after_job = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        self.poll(&ctx);
        self.poll_update(&ctx);
        self.handle_drops(&ctx);
        self.keyboard(&ctx);
        #[cfg(not(debug_assertions))]
        let _ = frame; // 只有效能診斷用到
        #[cfg(debug_assertions)]
        if let Some(p) = &mut self.perf
            && let Some(row) = p.tick(&ctx, frame, self.rows.len())
        {
            self.scroll_to = Some(row);
            self.sel = [row].into();
        }
        if self.job.is_some() || self.loading.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        let p = pal(ui);
        // 對齊：每一區第一個看得見的圖示或文字都在 x = CONTENT_X（20 px）。
        // 工具列按鈕內距 10、導覽按鈕圖示內縮 8、清單第一欄內縮 16，所以外框邊界各自不同。
        let chrome = egui::Frame::new().fill(p.chrome);
        let x = CONTENT_X as i8;
        // 再扣掉字形本身的左側留白（實測：20 px 圖示約 2 px、16 px 圖示約 1 px、文字約 1 px），讓看得見的筆畫對齊
        egui::Panel::top("command").frame(chrome.inner_margin(egui::Margin { left: x - 12, right: 12, top: 8, bottom: 4 })).show(ui, |ui| self.command_bar(ui));
        egui::Panel::top("address").frame(chrome.inner_margin(egui::Margin { left: x - 9, right: 12, top: 4, bottom: 8 })).show(ui, |ui| self.address_bar(ui));
        egui::Panel::bottom("status").frame(chrome.inner_margin(egui::Margin { left: x + 1, right: 12, top: 4, bottom: 4 })).show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin { left: 4, right: 4, top: 0, bottom: 0 }))
            .show(ui, |ui| {
                ui.add_enabled_ui(self.job.is_none(), |ui| self.file_table(ui));
            });

        self.progress_window(&ctx);
        self.dialogs(&ctx);

        if let Some(entries) = self.drag_out.take() {
            self.start_drag_out(entries, frame);
        }

        #[cfg(debug_assertions)]
        self.devshot_tick(&ctx);
    }
}

#[cfg(debug_assertions)]
impl App {
    /// 截圖開關：選取列、開啟對話框，或直接執行工作以驗證整個流程。
    fn devshot_tick(&mut self, ctx: &egui::Context) {
        let Some((shot, select, dialog)) = &mut self.devshot else { return };
        let progress_mode = shot.progress;
        if self.loading.is_none() {
            if *select > 0 {
                self.sel = (0..(*select).min(self.rows.len())).collect();
                self.anchor = self.sel.first().copied();
                *select = 0;
            }
            if let Some(d) = dialog.take() {
                let selected: Vec<PathBuf> = self.selected_rows().iter().filter_map(|r| self.row_path(r)).collect();
                let out_dir = self.loc.parent().and_then(|_| match &self.loc {
                    Location::Dir(d) => Some(d.clone()),
                    _ => None,
                });
                let compress = |level| Task::Compress {
                    inputs: selected.clone(),
                    output: out_dir.clone().unwrap_or_default().join("devshot.7z"),
                    format: Format::SevenZ,
                    opts: ziptool_core::CompressOptions { level: Some(level), ..Default::default() },
                    then: Then::default(),
                };
                match d.as_str() {
                    "add" => self.cmd_add(),
                    "extract" => self.cmd_extract(),
                    "info" => self.cmd_info(),
                    "delete" => self.cmd_delete(),
                    "about" => self.dialog = Some(Dialog::About),
                    "update" => {
                        let asset = update::Asset { name: "zipo.zip".into(), url: String::new(), sha256: "0".repeat(64), size: 9_200_000 };
                        let page = "https://github.com/omni624562/zipo/releases".into();
                        self.dialog = Some(Dialog::UpdateAvailable(update::Release { version: "0.3.0".into(), page, asset: Some(asset) }));
                    }
                    "updating" => self.dialog = Some(Dialog::Updating),
                    "hash" => {
                        let h = ziptool_core::hash_reader(&mut &b"abc"[..], &ziptool_core::NoProgress).unwrap();
                        let rows = vec![("zipo-0.3.1-windows-x64.zip".into(), h.clone()), ("說明文件.pdf".into(), h)];
                        self.dialog = Some(Dialog::Hashes { rows, expected: "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD".into() });
                    }
                    "test" => self.cmd_test(),
                    "compress-run" => self.spawn(compress(5)),
                    "progress-compress" => self.spawn(compress(9)),
                    "extract-run" | "progress-ask" => {
                        if let Location::Archive { path, .. } = &self.loc {
                            let dest = dialogs::default_dest(path);
                            let overwrite = if d == "progress-ask" { ziptool_core::Overwrite::Ask } else { Default::default() };
                            let opts = ziptool_core::ExtractOptions { overwrite, ..Default::default() };
                            self.spawn(Task::Extract { archives: vec![path.clone()], dest, opts, smart: true, then: Then::default() });
                        }
                    }
                    _ => {}
                }
            }
        }
        let ready = self.loading.is_none() && if progress_mode { self.job.is_some() } else { self.job.is_none() };
        if let Some((shot, _, _)) = &mut self.devshot {
            shot.tick(ctx, ready);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 根目錄 12 個檔案 + 一個只有 1 個檔案的子資料夾
    fn tree() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        for i in 0..12 {
            std::fs::write(t.path().join(format!("f{i:02}.txt")), "x").unwrap();
        }
        std::fs::create_dir(t.path().join("sub")).unwrap();
        std::fs::write(t.path().join("sub").join("only.txt"), "y").unwrap();
        t
    }

    #[test]
    fn navigate_with_stale_selection_does_not_panic() {
        let t = tree();
        let mut app = App::new(Some(t.path().to_path_buf()));
        assert_eq!(app.rows.len(), 13);
        // 選取最後一列後進入只有 1 個項目的資料夾
        app.sel = [12].into();
        app.anchor = Some(12);
        app.navigate(Location::Dir(t.path().join("sub")));
        assert_eq!(app.rows.len(), 1);
        assert!(app.sel.is_empty());

        // 在子資料夾選取後回上一層，應選取剛才的資料夾
        app.sel = [0].into();
        app.go_up();
        let sel: Vec<_> = app.sel.iter().map(|&i| app.rows[i].name.clone()).collect();
        assert_eq!(sel, ["sub"]);
    }

    #[test]
    fn refresh_after_files_removed_does_not_panic() {
        let t = tree();
        let mut app = App::new(Some(t.path().to_path_buf()));
        app.sel = (0..app.rows.len()).collect();
        for i in 0..12 {
            std::fs::remove_file(t.path().join(format!("f{i:02}.txt"))).unwrap();
        }
        app.refresh();
        assert_eq!(app.rows.len(), 1);
        assert!(app.sel.iter().all(|&i| i < app.rows.len()));
    }

    /// 隨機（固定種子）執行大量瀏覽操作，每一步都檢查選取狀態不會指向不存在的列。
    #[test]
    fn random_browsing_keeps_selection_valid() {
        let t = tree();
        std::fs::create_dir_all(t.path().join("sub").join("deep")).unwrap();
        let zip = t.path().join("pack.zip");
        ziptool_core::compress(&[t.path().join("sub")], &zip, Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();

        let ctx = egui::Context::default();
        let mut app = App::new(Some(t.path().to_path_buf()));
        let mut seed: u64 = 0x2545F4914F6CDD1D;
        let mut rnd = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n.max(1) as u64) as usize
        };
        let keys = [SortKey::Name, SortKey::Size, SortKey::Packed, SortKey::Modified, SortKey::Type];
        let mut created = 0;
        for step in 0..3000 {
            // 等背景讀取完成
            for _ in 0..500 {
                app.poll(&ctx);
                if app.loading.is_none() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            app.dialog = None;
            let n = app.rows.len();
            match rnd(9) {
                0 if n > 0 => app.select_click(rnd(n), Modifiers::NONE),
                1 if n > 0 => app.select_click(rnd(n), Modifiers::SHIFT),
                2 if n > 0 => app.select_click(rnd(n), Modifiers::COMMAND),
                3 => {
                    app.sort_key = keys[rnd(keys.len())];
                    app.sort_asc = rnd(2) == 0;
                    app.resort();
                }
                // 只開資料夾與壓縮檔，避免測試時啟動外部程式
                4 if n > 0 => {
                    let i = rnd(n);
                    let r = &app.rows[i];
                    if r.is_dir || r.is_archive {
                        app.open_row(i);
                    }
                }
                5 => app.go_up(),
                6 => app.refresh(),
                7 => {
                    // 模擬其他程式在背景新增或刪除檔案
                    if rnd(2) == 0 {
                        created += 1;
                        let _ = std::fs::write(t.path().join(format!("new{created}.txt")), "n");
                    } else if let Some(e) = std::fs::read_dir(t.path()).unwrap().flatten().find(|e| e.path().extension().is_some_and(|x| x == "txt")) {
                        let _ = std::fs::remove_file(e.path());
                    }
                    app.refresh();
                }
                _ => {
                    // 回到起點，避免一路走到磁碟根目錄
                    if step % 50 == 0 {
                        app.navigate(Location::Dir(t.path().to_path_buf()));
                    }
                }
            }
            let len = app.rows.len();
            assert!(app.sel.iter().all(|&i| i < len), "第 {step} 步：選取 {:?} 超出 {len} 列", app.sel);
            assert!(app.anchor.is_none_or(|a| a < len), "第 {step} 步：anchor {:?} 超出 {len} 列", app.anchor);
            let _ = app.selected_rows();
        }
    }
}

#[cfg(test)]
mod job_tests {
    use super::*;
    use ziptool_core::{ExtractOptions, Overwrite, OverwriteAnswer};

    fn zip_of(dir: &Path) -> PathBuf {
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "新內容").unwrap();
        let ar = dir.join("x.zip");
        ziptool_core::compress(&[src], &ar, Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();
        ar
    }

    fn wait_question(job: &Job) -> PathBuf {
        for _ in 0..500 {
            if let Some(q) = job.snapshot().question {
                return q;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("等不到同名檔問題");
    }

    fn wait_done(job: Job) -> (Task, Result<jobs::Outcome, Error>) {
        for _ in 0..500 {
            if job.is_finished() {
                return job.join();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("工作沒有結束");
    }

    fn extract_task(ar: &Path, dest: &Path) -> Task {
        Task::Extract {
            archives: vec![ar.to_path_buf()],
            dest: dest.to_path_buf(),
            opts: ExtractOptions { overwrite: Overwrite::Ask, ..Default::default() },
            smart: false,
            then: Then::default(),
        }
    }

    #[test]
    fn overwrite_question_is_answered_from_ui() {
        let t = tempfile::tempdir().unwrap();
        let ar = zip_of(t.path());
        let dest = t.path().join("out");
        std::fs::create_dir_all(dest.join("src")).unwrap();
        std::fs::write(dest.join("src/a.txt"), "舊內容").unwrap();

        let job = Job::spawn(extract_task(&ar, &dest));
        let q = wait_question(&job);
        assert!(q.ends_with("a.txt"));
        job.answer(OverwriteAnswer::Yes);
        let (_, r) = wait_done(job);
        assert!(r.is_ok());
        assert_eq!(std::fs::read_to_string(dest.join("src/a.txt")).unwrap(), "新內容");
    }

    #[test]
    fn cancel_while_waiting_for_answer() {
        let t = tempfile::tempdir().unwrap();
        let ar = zip_of(t.path());
        let dest = t.path().join("out");
        std::fs::create_dir_all(dest.join("src")).unwrap();
        std::fs::write(dest.join("src/a.txt"), "舊內容").unwrap();

        let job = Job::spawn(extract_task(&ar, &dest));
        wait_question(&job);
        job.cancel();
        let (_, r) = wait_done(job);
        assert!(matches!(r, Err(Error::Cancelled)), "{:?}", r.err());
        assert_eq!(std::fs::read_to_string(dest.join("src/a.txt")).unwrap(), "舊內容");
    }

    #[test]
    fn skipped_items_are_reported() {
        let t = tempfile::tempdir().unwrap();
        let ar = zip_of(t.path());
        let dest = t.path().join("out");
        // 讓 src/a.txt 的位置是資料夾，該項目會被略過
        std::fs::create_dir_all(dest.join("src/a.txt")).unwrap();
        let (_, r) = wait_done(Job::spawn(extract_task(&ar, &dest)));
        let out = r.unwrap();
        assert_eq!(out.issues.len(), 1);
        assert!(out.title.contains("略過"));
    }

    /// 讀取壓縮檔清單時離開，讀完後不應把使用者拉回壓縮檔。
    #[test]
    fn leaving_while_loading_is_respected() {
        let t = tempfile::tempdir().unwrap();
        let ar = zip_of(t.path());
        let ctx = egui::Context::default();
        let mut app = App::new(Some(t.path().to_path_buf()));
        app.navigate(Location::Archive { path: ar, inner: String::new() });
        assert!(app.loading.is_some());
        let elsewhere = t.path().join("src");
        app.navigate(Location::Dir(elsewhere.clone()));
        std::thread::sleep(Duration::from_millis(300));
        for _ in 0..10 {
            app.poll(&ctx);
        }
        assert_eq!(app.loc, Location::Dir(elsewhere));
    }
}

#[cfg(test)]
mod smart_tests {
    use super::*;
    use ziptool_core::ExtractOptions;

    fn make(dir: &Path, name: &str, tops: &[&str]) -> PathBuf {
        let src = dir.join(format!("src_{name}"));
        std::fs::create_dir_all(&src).unwrap();
        let mut inputs = Vec::new();
        for t in tops {
            let p = src.join(t);
            if t.ends_with(".txt") {
                std::fs::write(&p, "x").unwrap();
            } else {
                std::fs::create_dir_all(&p).unwrap();
                std::fs::write(p.join("inner.txt"), "y").unwrap();
            }
            inputs.push(p);
        }
        let ar = dir.join(name);
        ziptool_core::compress(&inputs, &ar, Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();
        ar
    }

    #[test]
    fn single_top_folder_is_not_nested_twice() {
        let t = tempfile::tempdir().unwrap();
        let opts = ExtractOptions::default();

        let ar = make(t.path(), "proj.zip", &["proj"]);
        assert_eq!(jobs::smart_dest(&ar, t.path().join("proj"), &opts), t.path());

        // 多個最上層項目：維持「壓縮檔同名資料夾」
        let ar = make(t.path(), "multi.zip", &["a", "b.txt"]);
        assert_eq!(jobs::smart_dest(&ar, t.path().join("multi"), &opts), t.path().join("multi"));

        // 只有一個檔案（不是資料夾）：維持原樣
        let ar = make(t.path(), "one.zip", &["only.txt"]);
        assert_eq!(jobs::smart_dest(&ar, t.path().join("one"), &opts), t.path().join("one"));

        // 使用者自訂的位置（名稱不是壓縮檔名）：不動
        let ar = make(t.path(), "proj2.zip", &["proj2"]);
        assert_eq!(jobs::smart_dest(&ar, t.path().join("我的位置"), &opts), t.path().join("我的位置"));
    }

    #[test]
    fn smart_extract_end_to_end() {
        let t = tempfile::tempdir().unwrap();
        let ar = make(t.path(), "proj.zip", &["proj"]);
        let out = t.path().join("dest");
        std::fs::create_dir_all(&out).unwrap();
        let task = Task::Extract { archives: vec![ar], dest: out.join("proj"), opts: Default::default(), smart: true, then: Then::default() };
        let job = Job::spawn(task);
        while !job.is_finished() {
            std::thread::sleep(Duration::from_millis(10));
        }
        let (_, r) = job.join();
        let outcome = r.unwrap();
        assert!(out.join("proj/inner.txt").exists());
        assert!(!out.join("proj/proj").exists(), "不應出現重複的資料夾");
        assert_eq!(outcome.open_dir.as_deref(), Some(out.as_path()));
    }
}

#[cfg(test)]
mod phase3_tests {
    use super::*;
    use std::collections::HashMap;

    fn tree() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        for n in ["apple.txt", "banana.txt", "cherry.log", "Avocado.md"] {
            std::fs::write(t.path().join(n), n).unwrap();
        }
        std::fs::create_dir(t.path().join("sub")).unwrap();
        std::fs::write(t.path().join("sub/inner.txt"), "x").unwrap();
        t
    }

    fn names(app: &App) -> Vec<String> {
        app.rows.iter().map(|r| r.name.clone()).collect()
    }

    fn selected(app: &App) -> Vec<String> {
        app.sel.iter().map(|&i| app.rows[i].name.clone()).collect()
    }

    #[test]
    fn back_and_forward_history() {
        let t = tree();
        let mut app = App::new(Some(t.path().to_path_buf()));
        let root = app.loc.clone();
        app.navigate(Location::Dir(t.path().join("sub")));
        assert_eq!(app.back, std::slice::from_ref(&root));
        app.go_back();
        assert_eq!(app.loc, root);
        assert_eq!(app.forward.len(), 1);
        app.go_forward();
        assert_eq!(app.loc, Location::Dir(t.path().join("sub")));
        // 前往新位置時清掉「下一頁」
        app.go_back();
        app.navigate(Location::Computer);
        assert!(app.forward.is_empty());
    }

    #[test]
    fn filter_rows_and_keep_selection_valid() {
        let t = tree();
        let mut app = App::new(Some(t.path().to_path_buf()));
        assert_eq!(app.rows.len(), 5);
        app.sel = (0..5).collect();
        app.filter = "A".into(); // 不分大小寫
        app.resort();
        assert_eq!(names(&app), ["apple.txt", "Avocado.md", "banana.txt"]);
        assert!(app.sel.iter().all(|&i| i < app.rows.len()));
        // 換資料夾時清掉篩選
        app.navigate(Location::Dir(t.path().join("sub")));
        assert!(app.filter.is_empty());
        assert_eq!(names(&app), ["inner.txt"]);
    }

    fn type_text(app: &mut App, ctx: &egui::Context, text: &str) {
        let raw = egui::RawInput { events: vec![egui::Event::Text(text.into())], ..Default::default() };
        let mut out = ctx.run_ui(raw, |ui| app.keyboard(ui.ctx()));
        out.textures_delta.clear();
    }

    #[test]
    fn type_ahead_jumps_to_matching_row() {
        let t = tree();
        let ctx = egui::Context::default();
        let mut app = App::new(Some(t.path().to_path_buf()));
        type_text(&mut app, &ctx, "c");
        assert_eq!(selected(&app), ["cherry.log"]);
        type_text(&mut app, &ctx, "a");
        assert_eq!(selected(&app), ["cherry.log"], "「ca」沒有符合的項目時保持原選取");
        app.typeahead_at = std::time::Instant::now() - Duration::from_secs(2);
        type_text(&mut app, &ctx, "av");
        assert_eq!(selected(&app), ["Avocado.md"]);
    }

    #[derive(Default)]
    struct MemStorage(HashMap<String, String>);
    impl eframe::Storage for MemStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.into(), value);
        }
        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }
        fn flush(&mut self) {}
    }

    #[test]
    fn settings_are_remembered() {
        let t = tree();
        let root = real_path(t.path());
        let mut store = MemStorage::default();
        {
            let mut app = App::new(Some(root.join("sub")));
            app.sort_key = SortKey::Size;
            app.sort_asc = false;
            app.add_format = Format::Zip;
            app.add_level = 5;
            app.auto_update = false;
            app.last_update_check = 1234;
            app.save_settings(&mut store);
        }
        let app = App::with_storage(None, Some(&store));
        assert_eq!(app.loc, Location::Dir(root.join("sub")));
        assert_eq!((app.sort_key, app.sort_asc), (SortKey::Size, false));
        assert_eq!((app.add_format, app.add_level), (Format::Zip, 5));
        assert_eq!((app.auto_update, app.last_update_check), (false, 1234));

        // 記住的資料夾已被刪除：改回家目錄，不出錯
        let mut gone = MemStorage::default();
        eframe::Storage::set_string(&mut gone, "last_dir", t.path().join("不存在").display().to_string());
        let app = App::with_storage(None, Some(&gone));
        assert_ne!(app.loc, Location::Dir(t.path().join("不存在")));
    }

    #[test]
    fn archive_cache_invalidates_when_file_changes() {
        let t = tree();
        let ar = t.path().join("a.zip");
        let ctx = egui::Context::default();
        ziptool_core::compress(&[t.path().join("apple.txt")], &ar, Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();
        let mut app = App::new(Some(ar.clone()));
        for _ in 0..200 {
            app.poll(&ctx);
            if app.loading.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(names(&app), ["apple.txt"]);

        // 其他程式改寫了壓縮檔
        std::fs::remove_file(&ar).unwrap();
        let inputs = [t.path().join("apple.txt"), t.path().join("banana.txt")];
        ziptool_core::compress(&inputs, &ar, Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();
        app.navigate(Location::Dir(t.path().to_path_buf()));
        app.navigate(Location::Archive { path: ar, inner: String::new() });
        assert!(app.loading.is_some(), "快取過期，應重新讀取");
        for _ in 0..200 {
            app.poll(&ctx);
            if app.loading.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(names(&app), ["apple.txt", "banana.txt"]);
    }

    #[test]
    fn executables_need_confirmation() {
        for n in ["setup.exe", "RUN.BAT", "a.ps1", "x.lnk", "m.msi", "help.chm", "site.URL", "a.pdf.exe", "s.settingcontent-ms", "d.iso", "x.appinstaller"] {
            assert!(is_executable(n), "{n}");
        }
        for n in ["readme.txt", "photo.jpg", "exe", "doc.pdf"] {
            assert!(!is_executable(n), "{n}");
        }
    }
}

#[cfg(test)]
mod update_tests {
    use super::*;

    fn load(app: &mut App, ctx: &egui::Context) {
        for _ in 0..400 {
            app.poll(ctx);
            if app.loading.is_none() && app.job.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("讀取或工作逾時");
    }

    fn names(app: &App) -> Vec<String> {
        app.rows.iter().map(|r| r.name.clone()).collect()
    }

    fn run_confirmed(app: &mut App, ctx: &egui::Context) {
        let Some(Dialog::ConfirmUpdate { task, .. }) = app.dialog.take() else { panic!("應出現確認對話框") };
        app.spawn(task);
        load(app, ctx);
        app.dialog = None;
    }

    #[test]
    fn delete_and_add_inside_archive() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        let src = root.join("proj");
        std::fs::create_dir_all(src.join("docs")).unwrap();
        std::fs::write(src.join("docs/a.txt"), "a").unwrap();
        std::fs::write(src.join("docs/b.txt"), "b").unwrap();
        let ar = root.join("p.zip");
        ziptool_core::compress(&[src], &ar, Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();

        let ctx = egui::Context::default();
        let mut app = App::new(Some(ar.clone()));
        load(&mut app, &ctx);
        app.navigate(Location::Archive { path: ar.clone(), inner: "proj/docs".into() });
        assert_eq!(names(&app), ["a.txt", "b.txt"]);

        // 刪除 a.txt：送出的是壓縮檔內的完整路徑
        app.sel = [0].into();
        app.cmd_delete();
        match &app.dialog {
            Some(Dialog::ConfirmUpdate { task: Task::Update { remove, .. }, danger, .. }) => {
                assert_eq!(remove, &["proj/docs/a.txt"]);
                assert!(*danger);
            }
            _ => panic!("應出現刪除確認"),
        }
        run_confirmed(&mut app, &ctx);
        load(&mut app, &ctx);
        assert_eq!(names(&app), ["b.txt"]);

        // 加入檔案：放進目前所在的資料夾
        let extra = root.join("new.txt");
        std::fs::write(&extra, "n").unwrap();
        app.confirm_add_into_archive(vec![extra]);
        run_confirmed(&mut app, &ctx);
        load(&mut app, &ctx);
        assert_eq!(names(&app), ["b.txt", "new.txt"]);
        let all: Vec<String> = ziptool_core::list(&ar, None).unwrap().into_iter().map(|e| e.name).collect();
        assert!(all.contains(&"proj/docs/new.txt".to_string()), "{all:?}");
    }

    /// 加入檔案到加密的 zip：先要求密碼，輸入後新檔案也加密
    #[test]
    fn add_into_encrypted_zip_asks_for_password() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        std::fs::write(root.join("old.txt"), "old").unwrap();
        let ar = root.join("e.zip");
        let enc = ziptool_core::CompressOptions { password: Some("pw".into()), ..Default::default() };
        ziptool_core::compress(&[root.join("old.txt")], &ar, Format::Zip, &enc, &ziptool_core::NoProgress).unwrap();

        let ctx = egui::Context::default();
        let mut app = App::new(Some(ar.clone()));
        load(&mut app, &ctx);
        let extra = root.join("new.txt");
        std::fs::write(&extra, "n").unwrap();
        app.confirm_add_into_archive(vec![extra]);
        let Some(Dialog::ConfirmUpdate { task, .. }) = app.dialog.take() else { panic!("應出現確認對話框") };
        app.spawn(task);
        load(&mut app, &ctx);
        let Some(Dialog::Password(mut d)) = app.dialog.take() else { panic!("應要求輸入密碼") };
        let purpose = std::mem::replace(&mut d.purpose, PasswordPurpose::Open { path: PathBuf::new(), inner: String::new() });
        let PasswordPurpose::Retry(mut task) = purpose else { panic!("應為重試") };
        task.set_password("pw".into());
        app.spawn(task);
        load(&mut app, &ctx);
        let entries = ziptool_core::list(&ar, None).unwrap();
        assert!(entries.iter().any(|e| e.name == "new.txt"), "{entries:?}");
        assert!(entries.iter().all(|e| e.encrypted), "{entries:?}");
    }

    #[test]
    fn delete_outside_archive_never_touches_disk() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("keep.txt"), "x").unwrap();
        let mut app = App::new(Some(t.path().to_path_buf()));
        app.sel = [0].into();
        app.cmd_delete();
        assert!(matches!(app.dialog, Some(Dialog::Notice { .. })));
        assert!(t.path().join("keep.txt").exists());
    }
}

#[cfg(test)]
mod start_tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<std::ffi::OsString> {
        a.iter().map(|s| (*s).into()).collect()
    }

    /// 視窗圖示讀取失敗時會默默變成空白，所以要確認內嵌的 PNG 能正確解碼
    #[test]
    fn window_icon_decodes() {
        let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/ziptool-256.png")).unwrap();
        assert_eq!((icon.width, icon.height), (256, 256));
        assert!(icon.rgba.chunks(4).any(|p| p[3] == 255), "圖示不應是全透明");
    }

    #[test]
    fn command_line_actions() {
        assert_eq!(parse_args(args(&[])), StartAction::None);
        assert_eq!(parse_args(args(&["D:\\a.7z"])), StartAction::Open("D:\\a.7z".into()));
        assert_eq!(parse_args(args(&["--add", "a", "b"])), StartAction::Add(vec!["a".into(), "b".into()]));
        assert_eq!(parse_args(args(&["--extract-here", "x.zip"])), StartAction::ExtractHere("x.zip".into()));
        assert_eq!(parse_args(args(&["--extract-folder", "x.zip"])), StartAction::ExtractFolder("x.zip".into()));
        assert_eq!(parse_args(args(&["--add"])), StartAction::None, "缺少參數時不做任何事");
        assert_eq!(parse_args(args(&["--unknown", "x"])), StartAction::None);
    }

    #[test]
    fn extract_here_runs_and_reports() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("doc.txt");
        std::fs::write(&src, "內容").unwrap();
        let ar = t.path().join("a.zip");
        ziptool_core::compress(std::slice::from_ref(&src), &ar, Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();
        std::fs::remove_file(&src).unwrap();

        let ctx = egui::Context::default();
        let mut app = App::new(None);
        app.apply_start(StartAction::ExtractHere(ar));
        for _ in 0..400 {
            app.poll(&ctx);
            if app.job.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(std::fs::read_to_string(&src).unwrap(), "內容");
        assert!(matches!(app.dialog, Some(Dialog::Notice { ok: true, .. })));
    }
}

#[cfg(all(test, windows))]
mod junction_tests {
    use super::*;

    fn cmd(args: &[&str]) -> bool {
        std::process::Command::new("cmd").arg("/C").args(args).output().is_ok_and(|o| o.status.success())
    }

    fn junction(link: &Path, target: &Path) {
        assert!(cmd(&["mklink", "/J", &link.display().to_string(), &target.display().to_string()]), "無法建立接合點");
    }

    /// 目錄接合點（例如家目錄中的 My Documents）是資料夾，雙擊應進入，而不是交給檔案總管當檔案開啟。
    #[test]
    fn junction_is_listed_as_folder() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        std::fs::create_dir(root.join("real")).unwrap();
        std::fs::write(root.join("real/inside.txt"), "x").unwrap();
        junction(&root.join("link"), &root.join("real"));

        let rows = browser::list_dir(&root, false).unwrap();
        let link = rows.iter().find(|r| r.name == "link").unwrap();
        assert!(link.is_dir, "接合點應視為資料夾");
        assert!(!link.is_archive);

        let mut app = App::new(Some(root.clone()));
        let i = app.rows.iter().position(|r| r.name == "link").unwrap();
        app.open_row(i);
        assert_eq!(app.loc, Location::Dir(root.join("link")));
        assert_eq!(app.rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["inside.txt"]);
    }

    /// 無法進入的資料夾：在狀態列顯示中文說明，不呼叫外部程式。
    #[test]
    fn inaccessible_folder_shows_chinese_error() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        std::fs::create_dir(root.join("gone")).unwrap();
        junction(&root.join("broken"), &root.join("gone"));
        std::fs::remove_dir(root.join("gone")).unwrap(); // 接合點指向不存在的位置

        let mut app = App::new(Some(root.clone()));
        let i = app.rows.iter().position(|r| r.name == "broken").unwrap();
        assert!(app.rows[i].is_dir);
        app.open_row(i);
        assert_eq!(app.loc, Location::Dir(root.clone()), "進不去時停留在原處");
        let err = app.status_error.clone().unwrap_or_default();
        assert!(err.contains("無法開啟") && !err.contains("os error"), "{err}");
    }

    /// 隱藏＋系統屬性的項目（Windows 的相容性接合點等）預設不顯示，與檔案總管相同。
    #[test]
    fn protected_system_items_are_hidden_by_default() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir(t.path().join("sys")).unwrap();
        std::fs::create_dir(t.path().join("plain_hidden")).unwrap();
        std::fs::write(t.path().join("normal.txt"), "x").unwrap();
        assert!(cmd(&["attrib", "+h", "+s", &t.path().join("sys").display().to_string()]));
        assert!(cmd(&["attrib", "+h", &t.path().join("plain_hidden").display().to_string()]));

        let names = |show: bool| {
            let mut v: Vec<String> = browser::list_dir(t.path(), show).unwrap().into_iter().map(|r| r.name).collect();
            v.sort();
            v
        };
        assert_eq!(names(false), ["normal.txt", "plain_hidden"], "只隱藏「隱藏＋系統」的項目");
        assert_eq!(names(true), ["normal.txt", "plain_hidden", "sys"]);
        let _ = cmd(&["attrib", "-h", "-s", &t.path().join("sys").display().to_string()]);
    }
}

#[cfg(test)]
mod toolbar_tests {
    use super::*;

    #[test]
    fn toolbar_follows_selection() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("doc.pdf"), "x").unwrap();
        let ar = t.path().join("pack.zip");
        ziptool_core::compress(&[t.path().join("doc.pdf")], &ar, Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();
        let mut app = App::new(Some(t.path().to_path_buf()));
        let idx = |app: &App, n: &str| app.rows.iter().position(|r| r.name == n).unwrap();

        // 沒有選取：只能看，不能做任何事
        let st = app.toolbar_state();
        assert!(!st.compress && !st.extract && !st.test && !st.info && !st.in_archive);

        // 選了一般檔案：可以壓縮，不能解壓縮或測試
        app.sel = [idx(&app, "doc.pdf")].into();
        let st = app.toolbar_state();
        assert!(st.compress && !st.extract && !st.test && st.info);

        // 選了壓縮檔：都可以
        app.sel = [idx(&app, "pack.zip")].into();
        let st = app.toolbar_state();
        assert!(st.compress && st.extract && st.test);

        // 在壓縮檔內：加入、解壓縮、測試一定可以；刪除要有選取
        let ctx = egui::Context::default();
        app.navigate(Location::Archive { path: ar, inner: String::new() });
        for _ in 0..400 {
            app.poll(&ctx);
            if app.loading.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let st = app.toolbar_state();
        assert!(st.in_archive && st.compress && st.extract && st.test && !st.delete);
        app.sel = [0].into();
        assert!(app.toolbar_state().delete);
    }
}

#[cfg(test)]
mod crumb_tests {
    use super::*;

    #[test]
    fn breadcrumb_overflow() {
        // 全部放得下
        assert_eq!(first_visible_crumb(&[50.0, 50.0, 50.0], 200.0, 30.0), 0);
        // 放不下：從右邊開始保留，前段收起
        assert_eq!(first_visible_crumb(&[80.0, 80.0, 80.0, 80.0], 200.0, 30.0), 2);
        // 最後一段一定顯示，即使本身就超過寬度
        assert_eq!(first_visible_crumb(&[80.0, 500.0], 200.0, 30.0), 1);
    }

    #[test]
    fn breadcrumbs_for_locations() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        let mut app = App::new(Some(root.clone()));
        let crumbs = app.breadcrumbs();
        assert_eq!(crumbs[0], ("本機".to_string(), Location::Computer));
        assert_eq!(crumbs.last().unwrap().1, Location::Dir(root.clone()));
        // 每一段都指向它自己的位置，路徑逐層變長
        for w in crumbs.windows(2).skip(1) {
            if let (Location::Dir(a), Location::Dir(b)) = (&w[0].1, &w[1].1) {
                assert!(b.starts_with(a) && b != a);
            }
        }

        let ar = t.path().join("a.zip");
        app.loc = Location::Archive { path: ar.clone(), inner: "x/y".into() };
        let crumbs = app.breadcrumbs();
        let tail: Vec<_> = crumbs.iter().rev().take(3).map(|(l, _)| l.as_str()).collect();
        assert_eq!(tail, ["y", "x", "a.zip"]);
        assert_eq!(crumbs.last().unwrap().1, Location::Archive { path: ar, inner: "x/y".into() });
    }
}

#[cfg(test)]
mod update_ui_tests {
    use super::*;

    fn release(v: &str) -> update::Release {
        update::Release { version: v.into(), page: "https://github.com/omni624562/zipo/releases".into(), asset: None }
    }

    /// 讓背景檢查直接回傳指定結果，再處理到完成
    fn finish_check(app: &mut App, ctx: &egui::Context, manual: bool, result: Result<update::Release, String>) {
        let handle = std::thread::spawn(move || result);
        while !handle.is_finished() {
            std::thread::yield_now();
        }
        app.update_work = Some(UpdateWork::Check { manual, handle });
        app.poll_update(ctx);
    }

    #[test]
    fn update_check_results() {
        let t = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();

        // 有新版本：顯示更新對話框
        let mut app = App::new(Some(t.path().to_path_buf()));
        finish_check(&mut app, &ctx, false, Ok(release("999.0.0")));
        assert!(matches!(&app.dialog, Some(Dialog::UpdateAvailable(r)) if r.version == "999.0.0"));

        // 手動檢查、已是最新：告知使用者；自動檢查則不打擾
        let mut app = App::new(Some(t.path().to_path_buf()));
        finish_check(&mut app, &ctx, true, Ok(release(env!("CARGO_PKG_VERSION"))));
        assert!(matches!(&app.dialog, Some(Dialog::Notice { title, .. }) if title == "已是最新版本"));
        let mut app = App::new(Some(t.path().to_path_buf()));
        finish_check(&mut app, &ctx, false, Ok(release(env!("CARGO_PKG_VERSION"))));
        assert!(app.dialog.is_none());

        // 自動檢查失敗（例如沒有網路）不打擾；手動檢查則顯示原因
        let mut app = App::new(Some(t.path().to_path_buf()));
        finish_check(&mut app, &ctx, false, Err("無法連線".into()));
        assert!(app.dialog.is_none());
        let mut app = App::new(Some(t.path().to_path_buf()));
        finish_check(&mut app, &ctx, true, Err("無法連線".into()));
        assert!(matches!(&app.dialog, Some(Dialog::Notice { title, .. }) if title == "無法檢查更新"));

        // 已有其他對話框時先等待，關閉後才顯示
        let mut app = App::new(Some(t.path().to_path_buf()));
        app.dialog = Some(Dialog::About);
        finish_check(&mut app, &ctx, false, Ok(release("999.0.0")));
        assert!(matches!(app.dialog, Some(Dialog::About)));
        app.dialog = None;
        app.poll_update(&ctx);
        assert!(matches!(app.dialog, Some(Dialog::UpdateAvailable(_))));
    }

    /// 自動檢查每天最多一次，關閉後不檢查
    #[test]
    fn auto_check_is_rate_limited() {
        let t = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(t.path().to_path_buf()));
        app.last_update_check = unix_now();
        app.auto_update_check();
        assert!(app.update_work.is_none(), "一天內不應再檢查");
        app.auto_update = false;
        app.last_update_check = 0;
        app.auto_update_check();
        assert!(app.update_work.is_none(), "關閉自動檢查時不應連線");
    }
}

#[cfg(test)]
mod drag_out_tests {
    use super::*;

    fn load(app: &mut App, ctx: &egui::Context) {
        for _ in 0..400 {
            app.poll(ctx);
            if app.loading.is_none() && app.job.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("讀取逾時");
    }

    /// 建立 docs/a.txt、docs/sub/b.txt、top.txt 的 zip，並進入 docs 資料夾
    fn archive_in_docs(root: &Path, password: Option<&str>) -> (App, egui::Context) {
        let src = root.join("src");
        std::fs::create_dir_all(src.join("docs/sub")).unwrap();
        std::fs::write(src.join("docs/a.txt"), "A").unwrap();
        std::fs::write(src.join("docs/sub/b.txt"), "B").unwrap();
        std::fs::write(src.join("top.txt"), "T").unwrap();
        let ar = root.join("d.zip");
        let opts = ziptool_core::CompressOptions { password: password.map(String::from), ..Default::default() };
        ziptool_core::compress(&[src.join("docs"), src.join("top.txt")], &ar, Format::Zip, &opts, &ziptool_core::NoProgress).unwrap();
        let ctx = egui::Context::default();
        let mut app = App::new(Some(ar.clone()));
        load(&mut app, &ctx);
        app.navigate(Location::Archive { path: ar, inner: "docs".into() });
        (app, ctx)
    }

    #[test]
    fn prepares_selected_items_for_dragging() {
        let t = tempfile::tempdir().unwrap();
        let (mut app, _ctx) = archive_in_docs(&real_path(t.path()), None);
        let names: Vec<String> = app.rows.iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, ["sub", "a.txt"]);
        app.sel = [0, 1].into();
        let entries: Vec<String> = app.selected_rows().iter().map(|r| app.inner_name(r)).collect();
        let mut items = app.prepare_drag_out(entries).unwrap();
        items.sort();
        // 最上層就是拖曳的兩個項目（去掉所在的 docs 資料夾），資料夾保留內容
        let got: Vec<String> = items.iter().map(|p| file_name(p)).collect();
        assert_eq!(got, ["a.txt", "sub"]);
        assert_eq!(std::fs::read_to_string(&items[0]).unwrap(), "A");
        assert_eq!(std::fs::read_to_string(items[1].join("b.txt")).unwrap(), "B");
        assert!(items[0].starts_with(std::env::temp_dir().join("Zipo")));
        assert!(app.dialog.is_none());
    }

    #[test]
    fn encrypted_archive_asks_for_password_first() {
        let t = tempfile::tempdir().unwrap();
        let (mut app, _ctx) = archive_in_docs(&real_path(t.path()), Some("pw"));
        assert!(app.prepare_drag_out(vec!["docs/a.txt".into()]).is_none());
        assert!(matches!(app.dialog, Some(Dialog::Password(_))));
    }

    #[derive(Debug)]
    struct TestFile(PathBuf);

    impl egui::DroppedFile for TestFile {
        fn path(&self) -> &Path {
            &self.0
        }
        fn bytes(&self) -> Result<Vec<u8>, String> {
            std::fs::read(&self.0).map_err(|e| e.to_string())
        }
    }

    /// 從 Zipo 拖出去的暫存檔被放回視窗時忽略；一般檔案照常加入
    #[test]
    fn own_temp_files_dropped_back_are_ignored() {
        let t = tempfile::tempdir().unwrap();
        let (mut app, ctx) = archive_in_docs(&real_path(t.path()), None);
        let drop = |app: &mut App, p: PathBuf| {
            let file: egui::DroppedFileHandle = std::sync::Arc::new(TestFile(p));
            let raw = egui::RawInput { dropped_files: vec![file], ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| app.handle_drops(ui.ctx()));
            out.textures_delta.clear();
        };
        drop(&mut app, std::env::temp_dir().join("Zipo").join("1-drag-2").join("a.txt"));
        assert!(app.dialog.is_none(), "不應出現加入確認");
        let other = t.path().join("other.txt");
        std::fs::write(&other, "x").unwrap();
        drop(&mut app, other);
        assert!(matches!(app.dialog, Some(Dialog::ConfirmUpdate { .. })));
    }

    /// 在列上按住並拖動：拖曳該列（未選取時只拖曳該列）
    #[test]
    fn dragging_a_row_requests_drag_out() {
        let t = tempfile::tempdir().unwrap();
        let (mut app, ctx) = archive_in_docs(&real_path(t.path()), None);
        let frame = |app: &mut App, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(800.0, 600.0))),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| app.file_table(ui));
            out.textures_delta.clear();
        };
        frame(&mut app, vec![]);
        // 第二列（a.txt）：表頭之下第二列的位置
        let y = 32.0 + CONTROL_H * 1.5;
        let at = egui::pos2(120.0, y);
        let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        frame(&mut app, vec![egui::Event::PointerMoved(at), press(true)]);
        frame(&mut app, vec![egui::Event::PointerMoved(at + vec2(30.0, 30.0))]);
        frame(&mut app, vec![egui::Event::PointerMoved(at + vec2(60.0, 60.0))]);
        assert_eq!(app.drag_out.as_deref(), Some(&["docs/a.txt".to_string()][..]), "選取：{:?}", app.sel);
    }
}

#[cfg(test)]
mod hash_tests {
    use super::*;

    fn wait(app: &mut App, ctx: &egui::Context) {
        for _ in 0..800 {
            app.poll(ctx);
            if app.loading.is_none() && app.job.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("逾時");
    }

    fn hashes_dialog(app: &App) -> Vec<(String, String)> {
        match &app.dialog {
            Some(Dialog::Hashes { rows, .. }) => rows.iter().map(|(n, h)| (n.clone(), h.sha256.clone())).collect(),
            _ => panic!("應出現雜湊值對話框"),
        }
    }

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn hashes_files_on_disk() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        std::fs::write(root.join("abc.txt"), "abc").unwrap();
        let ctx = egui::Context::default();
        let mut app = App::new(Some(root.clone()));
        app.sel = [app.rows.iter().position(|r| r.name == "abc.txt").unwrap()].into();
        app.cmd_hash();
        wait(&mut app, &ctx);
        assert_eq!(hashes_dialog(&app), [("abc.txt".to_string(), ABC.to_string())]);
    }

    #[test]
    fn hashes_entries_inside_archive() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        std::fs::create_dir_all(root.join("src/docs")).unwrap();
        std::fs::write(root.join("src/docs/abc.txt"), "abc").unwrap();
        let ar = root.join("h.zip");
        ziptool_core::compress(&[root.join("src/docs")], &ar, Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();
        let ctx = egui::Context::default();
        let mut app = App::new(Some(ar.clone()));
        wait(&mut app, &ctx);
        app.navigate(Location::Archive { path: ar, inner: "docs".into() });
        app.sel = [0].into();
        app.cmd_hash();
        wait(&mut app, &ctx);
        assert_eq!(hashes_dialog(&app), [("abc.txt".to_string(), ABC.to_string())]);
        // 暫存資料夾已刪除
        let leftover = std::fs::read_dir(std::env::temp_dir().join("Zipo"))
            .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with(&format!("{}-hash-", std::process::id()))))
            .unwrap_or(false);
        assert!(!leftover, "不應留下暫存資料夾");
    }
}
