// release 版不跳出主控台視窗
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Zipo 檔案管理員：版面與操作方式參考 7-Zip File Manager。

use ziptool_core::{F, L};
mod browser;
#[cfg(debug_assertions)]
mod devshot;
mod dialogs;
mod jobs;
mod localize;
// 效能診斷只在開發版提供：正式版不應依環境變數寫檔、自動捲動或自行關閉
#[cfg(debug_assertions)]
mod perf;
mod preview;
mod shell;
mod theme;
mod update;
mod widgets;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
        let text = F!("Zipo {} 發生錯誤
{info}

{bt}
", "Zipo {} ran into an error
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
                .set_title(L!("Zipo 發生錯誤", "Zipo ran into an error"))
                .set_description(F!("{info}

詳細資訊已寫入：
{}", "{info}

Details were written to:
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
                .ok_or_else(|| L!("找不到可用的顯示卡", "No usable graphics adapter was found").to_string())
        }));
    }
    // 檔案總管選取多個檔案時每個各啟動一個 Zipo：合併成一個視窗，其餘直接結束
    let start = match parse_args(std::env::args_os().skip(1).collect()) {
        StartAction::Add(p) => shell::collect("add", p).map(StartAction::Add),
        StartAction::ExtractHere(p) => shell::collect("extract-here", p).map(StartAction::ExtractHere),
        StartAction::ExtractFolder(p) => shell::collect("extract-folder", p).map(StartAction::ExtractFolder),
        other => Some(other),
    };
    let Some(start) = start else { return Ok(()) };
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
            ziptool_core::i18n::set_english(app.language.english());
            // 瀏覽、預覽、解壓縮同一組分割檔時只接合一次（結束時刪除）
            ziptool_core::keep_joined_volumes(true);
            // 舊版註冊的右鍵選單或預設程式缺少新的副檔名時補上
            if let Ok(exe) = std::env::current_exe() {
                shell::refresh(shell::ROOT, shell::SOFTWARE, &exe);
            }
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
    ExtractHere(Vec<PathBuf>),
    ExtractFolder(Vec<PathBuf>),
}

fn parse_args(args: Vec<std::ffi::OsString>) -> StartAction {
    let mut it = args.into_iter();
    let Some(first) = it.next() else { return StartAction::None };
    let rest: Vec<PathBuf> = it.map(PathBuf::from).collect();
    match first.to_str() {
        Some("--add") if !rest.is_empty() => StartAction::Add(rest),
        Some("--extract-here") if !rest.is_empty() => StartAction::ExtractHere(rest),
        Some("--extract-folder") if !rest.is_empty() => StartAction::ExtractFolder(rest),
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
    // 測試時不實際開啟檔案或資料夾
    if cfg!(test) {
        return;
    }
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

/// 介面語言的設定
#[derive(Clone, Copy, PartialEq, Debug)]
enum Language {
    /// 依系統的顯示語言
    Auto,
    Chinese,
    English,
}

impl Language {
    fn key(self) -> &'static str {
        match self {
            Language::Auto => "auto",
            Language::Chinese => "zh",
            Language::English => "en",
        }
    }

    fn from_key(k: &str) -> Self {
        match k {
            "zh" => Language::Chinese,
            "en" => Language::English,
            _ => Language::Auto,
        }
    }

    fn english(self) -> bool {
        match self {
            Language::Auto => ziptool_core::i18n::system_prefers_english(),
            Language::Chinese => false,
            Language::English => true,
        }
    }
}

/// 預覽窗格目前顯示的項目
struct Preview {
    /// 位置與名稱；選取改變時重新讀取
    key: String,
    name: String,
    size: Option<u64>,
    modified: Option<SystemTime>,
    state: PreviewState,
    /// 選取改變或關閉預覽時設為 true，背景中還在解壓縮的工作立即停止
    cancel: Arc<AtomicBool>,
    texture: Option<egui::TextureHandle>,
}

enum PreviewState {
    Loading(JoinHandle<preview::Content>),
    Ready(preview::Content),
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// 拖曳時已解出的項目；壓縮檔沒有變動時，再次拖曳同樣的項目不必重新解壓縮
struct DragCache {
    archive: PathBuf,
    stamp: Option<(u64, Option<SystemTime>)>,
    /// 排序後的項目名稱
    entries: Vec<String>,
    items: Vec<PathBuf>,
}

/// 壓縮檔內的壓縮檔：解到暫存資料夾後瀏覽（唯讀）
struct Nested {
    /// 暫存資料夾中的壓縮檔
    path: PathBuf,
    /// 開啟它的位置（外層壓縮檔內的資料夾），「上一層」回到這裡
    parent: Location,
}

/// 從壓縮檔內開啟的檔案：在其他程式中修改並儲存後，詢問是否存回壓縮檔
struct Watched {
    archive: PathBuf,
    /// 壓縮檔內的完整名稱
    entry: String,
    /// 解到暫存資料夾的檔案
    file: PathBuf,
    /// 上次確認時的大小與修改時間
    stamp: Option<(u64, Option<SystemTime>)>,
    /// 壓縮檔的大小與修改時間（開啟時，或 Zipo 自己更新它之後）；不同代表被其他程式修改或取代
    archive_stamp: Option<(u64, Option<SystemTime>)>,
    password: Option<String>,
}

struct Loading {
    path: PathBuf,
    inner: String,
    password: Option<String>,
    handle: JoinHandle<Result<Vec<Entry>, Error>>,
    /// 由上一頁／下一頁觸發的讀取，完成時不要再記入瀏覽紀錄
    from_history: bool,
    /// 放棄這次讀取時（Loading 被丟棄）通知背景工作（例如接合大型分割檔）立即停止
    _cancel: CancelOnDrop,
}

/// 被丟棄時設定取消旗標
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// 背景工作的取消旗標
struct CancelFlag(Arc<AtomicBool>);

impl ziptool_core::Progress for CancelFlag {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
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
    /// 從壓縮檔內開啟的檔案已修改，詢問是否存回（`watched` 的索引）
    SaveBack(usize),
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
    /// 從壓縮檔內開啟、需要偵測修改的檔案
    watched: Vec<Watched>,
    /// 已開啟的壓縮檔內的壓縮檔
    nested: Vec<Nested>,
    drag_cache: Vec<DragCache>,
    /// 顯示預覽窗格（檢視 › 預覽窗格，Alt+P）
    preview_open: bool,
    language: Language,
    preview: Option<Preview>,
    last_watch_check: std::time::Instant,
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
            watched: Vec::new(),
            preview_open: false,
            language: Language::Auto,
            preview: None,
            nested: Vec::new(),
            drag_cache: Vec::new(),
            last_watch_check: std::time::Instant::now(),
            #[cfg(debug_assertions)]
            perf: perf::Perf::from_env(),
            #[cfg(debug_assertions)]
            devshot: None,
        };
        let get = |k: &str| storage.and_then(|s| s.get_string(k));
        app.auto_update = get("auto_update").as_deref() != Some("0");
        app.open_after = get("open_after").as_deref() == Some("1");
        app.preview_open = get("preview").as_deref() == Some("1");
        // 沒有語言設定：從舊版升級（已有其他設定）時維持中文，避免更新後介面突然變成英文；全新安裝才依系統語言
        app.language = match get("language") {
            Some(k) => Language::from_key(&k),
            None if get("sort_key").is_some() => Language::Chinese,
            None => Language::Auto,
        };
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
            StartAction::ExtractHere(archives) => {
                let dir = parent(&archives[0]);
                self.navigate(Location::Dir(dir.clone()));
                let opts = ziptool_core::ExtractOptions { overwrite: ziptool_core::Overwrite::Ask, ..Default::default() };
                self.spawn(Task::Extract { archives, dest: dir, opts, smart: false, here: true, then: Then::default() });
            }
            StartAction::ExtractFolder(archives) => {
                self.navigate(Location::Dir(parent(&archives[0])));
                self.dialog = Some(Dialog::Extract(self.with_open_after_extract(ExtractDialog::new(ExtractSource::Files(archives), None))));
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
        storage.set_string("preview", if self.preview_open { "1" } else { "0" }.into());
        storage.set_string("language", self.language.key().into());
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
            self.status_error = Some(F!("找不到：{}", "Not found: {}", p.display()));
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
                        L!("沒有讀取這個資料夾的權限", "You don't have permission to read this folder").to_string()
                    } else {
                        ziptool_core::describe(&e)
                    };
                    self.status_error = Some(F!("無法開啟 {}：{why}", "Cannot open {}: {why}", p.display()));
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
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = CancelFlag(cancel.clone());
        let handle = std::thread::spawn(move || ziptool_core::list_with_progress(&p, pw.as_deref(), &flag));
        self.loading = Some(Loading { path, inner, password, handle, from_history: self.history_nav, _cancel: CancelOnDrop(cancel) });
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
        self.rows = match (&self.loc, &self.cache) {
            // 壓縮檔內：搜尋目前資料夾以下的所有層
            (Location::Archive { path, inner }, Some(c)) if !f.is_empty() && c.path == *path => browser::archive_search(&c.entries, inner, &f),
            _ => self.all_rows.iter().filter(|r| f.is_empty() || r.display.to_lowercase().contains(&f) || r.name.to_lowercase().contains(&f)).cloned().collect(),
        };
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
        // 壓縮檔內的壓縮檔：從它的根目錄回到外層壓縮檔
        if let Location::Archive { path, inner } = &self.loc
            && inner.is_empty()
            && let Some(n) = self.nested.iter().find(|n| &n.path == path)
        {
            let parent = n.parent.clone();
            return self.navigate(parent);
        }
        if let Some(parent) = self.loc.parent() {
            self.navigate(parent);
        }
    }

    /// 目前位於壓縮檔內的壓縮檔（暫存的副本，唯讀）
    #[cfg(test)]
    fn in_nested(&self) -> bool {
        matches!(&self.loc, Location::Archive { path, .. } if self.is_nested(path))
    }

    /// 目前的壓縮檔不能修改的原因：壓縮檔內的壓縮檔、ISO 映像檔、分割的壓縮檔
    fn read_only_reason(&self) -> Option<&'static str> {
        let Location::Archive { path, .. } = &self.loc else { return None };
        if self.is_nested(path) {
            Some(nested_read_only())
        } else if ziptool_core::is_first_volume(path) {
            Some(L!("分割的壓縮檔無法直接修改。若要修改，請先解壓縮，再重新壓縮。", "Split archives cannot be modified directly. To change it, extract it first and compress again."))
        } else if Format::from_file_name(&file_name(path)).is_some_and(|f| f.is_read_only()) {
            Some(L!("ISO 映像檔只能瀏覽與解壓縮，無法修改。", "ISO images can only be browsed and extracted, not modified."))
        } else {
            None
        }
    }

    fn is_nested(&self, path: &Path) -> bool {
        self.nested.iter().any(|n| n.path == path)
    }

    fn go_address(&mut self) {
        let text = self.addr.trim().trim_matches('"').to_string();
        if text.is_empty() || text == L!("電腦", "Computer") {
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
        self.status_error = Some(F!("找不到路徑：{text}", "Path not found: {text}"));
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
                } else if r.is_archive || ziptool_core::is_first_volume(&path) || Format::detect(&path).is_ok() {
                    self.navigate(Location::Archive { path, inner: String::new() });
                } else if path.is_file() {
                    open_path(&path);
                } else {
                    // 不是可開啟的檔案（例如讀不到的連結）：不交給外部程式，避免跳出系統錯誤視窗
                    self.status_error = Some(F!("無法開啟 {}", "Cannot open {}", r.name));
                }
            }
            Location::Archive { path, inner } => {
                if r.is_dir {
                    let inner = if inner.is_empty() { r.name.clone() } else { format!("{inner}/{}", r.name) };
                    self.navigate(Location::Archive { path, inner });
                } else if Format::from_file_name(&r.name).is_some() {
                    let password = self.cache.as_ref().and_then(|c| c.password.clone());
                    self.spawn(Task::OpenNested { archive: path, entry: self.inner_name(&r), strip: inner, password });
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
    fn prepare_drag_out(&mut self, mut entries: Vec<String>) -> Option<Vec<PathBuf>> {
        let Location::Archive { path, inner } = self.loc.clone() else { return None };
        entries.sort();
        // 之前已解出過同樣的項目，且壓縮檔沒有變動：直接拖曳
        let archive_stamp = file_stamp(&path);
        self.drag_cache.retain(|c| c.items.iter().all(|i| i.exists()));
        if let Some(c) = self.drag_cache.iter().find(|c| c.archive == path && c.entries == entries && c.stamp == archive_stamp) {
            return Some(c.items.clone());
        }
        // 拖曳前要先解完；太大時改在背景解壓縮並顯示進度，完成後再拖曳一次
        let size: u64 = self.selected_rows().iter().filter_map(|r| r.size).sum();
        if size > DRAG_OUT_MAX {
            let password = self.cache.as_ref().and_then(|c| c.password.clone());
            self.spawn(Task::PrepareDrag { archive: path, entries, strip: inner, password });
            return None;
        }
        let cache_entries = entries.clone();
        let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let dir = std::env::temp_dir().join("Zipo").join(format!("{}-drag-{stamp}", std::process::id()));
        let password = self.cache.as_ref().and_then(|c| c.password.clone());
        // 在畫面執行緒上解壓縮：宣告的大小可能造假，實際寫出超過上限就停止，改在背景處理
        let opts = ziptool_core::ExtractOptions {
            password,
            selection: Some(entries),
            strip_prefix: inner.clone(),
            max_bytes: Some(DRAG_OUT_MAX),
            ..Default::default()
        };
        match ziptool_core::extract(&path, &dir, &opts, &ziptool_core::NoProgress) {
            Ok(r) if !r.issues.is_empty() => {
                let i = &r.issues[0];
                self.notice(L!("無法拖曳", "Cannot drag"), F!("{}：{}", "{}: {}", i.name, i.reason));
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
            Err(Error::TooLarge { .. }) => {
                let _ = std::fs::remove_dir_all(&dir);
                self.spawn(Task::PrepareDrag { archive: path, entries: cache_entries, strip: inner, password: opts.password });
                return None;
            }
            Err(e) => {
                self.notice(L!("無法拖曳", "Cannot drag"), e.to_string());
                return None;
            }
        }
        let items: Vec<PathBuf> = std::fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        if items.is_empty() {
            return None;
        }
        self.drag_cache.push(DragCache { archive: path, stamp: archive_stamp, entries: cache_entries, items: items.clone() });
        Some(items)
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
            return self.notice(L!("無法更新", "Cannot update"), L!("找不到目前執行的程式。", "Cannot find the running program."));
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
                Some(UpdateWork::Check { manual, handle }) => match handle.join().unwrap_or_else(|_| Err(L!("檢查更新時發生異常", "An error occurred while checking for updates").into())) {
                    Ok(r) if update::is_newer(&r.version, env!("CARGO_PKG_VERSION")) => self.pending_release = Some(r),
                    Ok(_) if manual => self.notice(L!("已是最新版本", "You're up to date"), F!("目前使用的 Zipo {} 已是最新版本。", "Zipo {} is the latest version.", env!("CARGO_PKG_VERSION"))),
                    Err(e) if manual => self.notice(L!("無法檢查更新", "Cannot check for updates"), e),
                    // 自動檢查失敗（例如沒有網路）不打擾使用者
                    _ => {}
                },
                Some(UpdateWork::Install(handle)) => match handle.join().unwrap_or_else(|_| Err(L!("更新時發生異常", "An error occurred while updating").into())) {
                    Ok(exe) => {
                        if let Err(e) = std::process::Command::new(&exe).spawn() {
                            self.notice(L!("更新完成", "Update complete"), F!("新版本已安裝，但無法自動重新啟動（{e}）。請手動開啟 Zipo。", "The new version is installed, but Zipo could not restart automatically ({e}). Please open Zipo yourself."));
                        } else {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                    Err(e) => {
                        self.dialog = None;
                        self.notice(L!("更新失敗", "Update failed"), F!("{e}\n\n原本的程式沒有變動。", "{e}\n\nThe installed program was not changed."));
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
            if let Some(files) = rfd::FileDialog::new().set_title(L!("選擇要加入壓縮檔的檔案", "Choose files to add to the archive")).pick_files() {
                self.confirm_add_into_archive(files);
            }
            return;
        }
        let paths: Vec<PathBuf> = self.selected_rows().iter().filter(|r| !r.is_drive).filter_map(|r| self.row_path(r)).collect();
        if paths.is_empty() {
            return self.notice(L!("加入", "Add"), L!("請先選取要加入壓縮檔的檔案或資料夾。", "Select the files or folders to compress first."));
        }
        self.dialog = Some(Dialog::Add(self.with_open_after_add(AddDialog::new(paths, self.add_format, self.add_level))));
    }

    /// 在壓縮檔內加入檔案（加到目前瀏覽的資料夾）。
    fn confirm_add_into_archive(&mut self, files: Vec<PathBuf>) {
        let Location::Archive { path, inner } = self.loc.clone() else { return };
        if let Some(why) = self.read_only_reason() {
            return self.notice(L!("加入", "Add"), why);
        }
        if Format::detect(&path).is_ok_and(|f| f.is_single()) {
            return self.notice(L!("加入", "Add"), L!("這種格式只能包含單一檔案，無法再加入其他檔案。", "This format can contain only a single file; no other files can be added."));
        }
        let names: Vec<String> = files.iter().take(3).map(|p| file_name(p)).collect();
        let more = if files.len() > 3 { F!(" 等 {} 個項目", " and {} items in total", files.len()) } else { String::new() };
        let place = if inner.is_empty() { L!("根目錄", "the root").to_string() } else { F!("「{inner}」資料夾", "the \"{inner}\" folder") };
        let message = F!("要把 {}{more} 加入 {} 的{place}嗎？\n壓縮檔中同名的項目會被取代。", "Add {}{more} to {place} of {}?\nItems with the same name in the archive will be replaced.", names.join(L!("、", ", ")), file_name(&path));
        let password = self.cache.as_ref().and_then(|c| c.password.clone());
        let task = Task::Update { archive: path, add: files, into: inner, remove: Vec::new(), password };
        self.dialog = Some(Dialog::ConfirmUpdate { task, title: L!("加入到壓縮檔", "Add to archive").into(), message, danger: false });
    }

    fn cmd_delete(&mut self) {
        let Location::Archive { path, .. } = self.loc.clone() else {
            return self.notice(L!("刪除", "Delete"), L!("刪除只能用在壓縮檔內的項目。Zipo 不會刪除你電腦上的檔案。", "Delete only works on items inside an archive. Zipo never deletes files on your computer."));
        };
        if let Some(why) = self.read_only_reason() {
            return self.notice(L!("刪除", "Delete"), why);
        }
        let remove: Vec<String> = self.selected_rows().iter().map(|r| self.inner_name(r)).collect();
        if remove.is_empty() {
            return self.notice(L!("刪除", "Delete"), L!("請先選取要從壓縮檔刪除的項目。", "Select the items to delete from the archive first."));
        }
        if Format::detect(&path).is_ok_and(|f| f.is_single()) {
            return self.notice(L!("刪除", "Delete"), L!("這種格式只能包含單一檔案，無法刪除其中的項目。", "This format contains only a single file; its contents cannot be deleted."));
        }
        let message = F!(
            "要從 {} 刪除選取的 {} 個項目嗎？\n資料夾會連同其中的內容一起刪除，這個動作無法復原。", "Delete the {1} selected item(s) from {0}?\nFolders are deleted with their contents. This cannot be undone.",
            file_name(&path),
            remove.len()
        );
        let password = self.cache.as_ref().and_then(|c| c.password.clone());
        let task = Task::Update { archive: path, add: Vec::new(), into: String::new(), remove, password };
        self.dialog = Some(Dialog::ConfirmUpdate { task, title: L!("從壓縮檔刪除", "Delete from archive").into(), message, danger: true });
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
                    return self.notice(L!("解壓縮", "Extract"), L!("請先選取要解壓縮的壓縮檔。", "Select the archives to extract first."));
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
                    return self.notice(L!("測試", "Test"), L!("請先選取要測試的壓縮檔。", "Select the archives to test first."));
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
                lines.push((L!("路徑", "Path").into(), path.display().to_string()));
                lines.push((L!("格式", "Format").into(), Format::detect(path).map(|f| f.name().to_string()).unwrap_or_default()));
                lines.push((L!("檔案", "File").into(), files.to_string()));
                lines.push((L!("資料夾", "Folder").into(), dirs.to_string()));
                lines.push((L!("原始大小", "Original size").into(), human(size)));
                lines.push((L!("壓縮檔大小", "Archive size").into(), human(packed)));
                if size > 0 {
                    lines.push((L!("壓縮率", "Ratio").into(), format!("{:.1}%", packed as f64 * 100.0 / size as f64)));
                }
                lines.push((L!("加密", "Encryption").into(), if c.entries.iter().any(|e| e.encrypted) || c.password.is_some() { L!("是", "Yes") } else { L!("否", "No") }.into()));
                let title = file_name(path);
                self.dialog = Some(Dialog::Info { title, lines });
            } else {
                self.notice(L!("內容", "Properties"), L!("請先選取項目。", "Select an item first."));
            }
            return;
        }
        let size: u64 = rows.iter().filter_map(|r| r.size).sum();
        if rows.len() == 1 {
            let r = rows[0];
            lines.push((L!("名稱", "Name").into(), r.display.clone()));
            if r.display != r.name {
                lines.push((L!("實際名稱", "Actual name").into(), r.name.clone()));
            }
            lines.push((L!("類型", "Type").into(), r.type_label()));
            if let Some(p) = self.row_path(r) {
                lines.push((L!("位置", "Location").into(), p.parent().map(|d| d.display().to_string()).unwrap_or_default()));
            } else {
                lines.push((L!("位置", "Location").into(), self.loc.display()));
            }
        } else {
            lines.push((L!("項目", "Items").into(), F!("{} 個", "{}", rows.len())));
        }
        lines.push((L!("大小", "Size").into(), F!("{}（{} 位元組）", "{} ({} bytes)", human(size), size)));
        if rows.iter().any(|r| r.packed.is_some()) {
            let packed: u64 = rows.iter().filter_map(|r| r.packed).sum();
            lines.push((L!("壓縮後", "Packed").into(), human(packed)));
        }
        if rows.len() == 1 && rows[0].modified.is_some() {
            lines.push((L!("修改日期", "Modified").into(), format_time(rows[0].modified)));
        }
        if rows.iter().any(|r| r.encrypted) {
            lines.push((L!("加密", "Encryption").into(), L!("是", "Yes").into()));
        }
        let title = if rows.len() == 1 { rows[0].display.clone() } else { L!("內容", "Properties").into() };
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

    /// 依目前選取的單一檔案更新預覽：選取改變時在背景讀取（壓縮檔內的檔案先解到暫存資料夾）
    fn update_preview(&mut self, ctx: &egui::Context) {
        let row = match self.selected_rows().as_slice() {
            [r] if !r.is_dir && !r.is_drive => (*r).clone(),
            _ => {
                self.preview = None;
                return;
            }
        };
        let key = format!("{}|{}", self.loc.display(), row.name);
        if self.preview.as_ref().is_some_and(|p| p.key == key) {
            if let Some(p) = &mut self.preview
                && let PreviewState::Loading(h) = &p.state
            {
                if h.is_finished() {
                    let PreviewState::Loading(h) = std::mem::replace(&mut p.state, PreviewState::Ready(preview::Content::Unavailable(String::new())))
                    else {
                        unreachable!()
                    };
                    p.state = PreviewState::Ready(h.join().unwrap_or_else(|_| preview::Content::Unavailable(L!("讀取時發生異常", "An error occurred while reading").into())));
                } else {
                    ctx.request_repaint_after(Duration::from_millis(50));
                }
            }
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let state = if preview::kind(&row.name).is_none() {
            PreviewState::Ready(preview::Content::Unavailable(String::new()))
        } else {
            match &self.loc {
                Location::Dir(d) => {
                    let path = d.join(&row.name);
                    PreviewState::Loading(std::thread::spawn(move || preview::load(&path)))
                }
                Location::Archive { path, inner } if row.size.unwrap_or(0) <= preview::MAX_ARCHIVE_PREVIEW => {
                    let (archive, strip, entry) = (path.clone(), inner.clone(), self.inner_name(&row));
                    let password = self.cache.as_ref().and_then(|c| c.password.clone());
                    let cancel = cancel.clone();
                    PreviewState::Loading(std::thread::spawn(move || preview_inside(&archive, &entry, &strip, password, cancel)))
                }
                Location::Archive { .. } => PreviewState::Ready(preview::Content::Unavailable(F!(
                    "超過 {} 的檔案不預覽", "Files larger than {} are not previewed",
                    ziptool_core::format_size(preview::MAX_ARCHIVE_PREVIEW)
                ))),
                Location::Computer => return,
            }
        };
        self.preview = Some(Preview { key, name: row.display.clone(), size: row.size, modified: row.modified, state, cancel, texture: None });
    }

    fn preview_pane(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        let Some(pv) = &mut self.preview else {
            ui.centered_and_justified(|ui| ui.label(RichText::new(L!("選取檔案以預覽", "Select a file to preview")).color(p.text2)));
            return;
        };
        ui.add(egui::Label::new(RichText::new(&pv.name).strong().color(p.text)).wrap());
        let mut info = vec![];
        if let Some(sz) = pv.size {
            info.push(human(sz));
        }
        if let PreviewState::Ready(preview::Content::Image(_, [w, h])) = &pv.state {
            info.insert(0, format!("{w} × {h}"));
        }
        if pv.modified.is_some() {
            info.push(format_time(pv.modified));
        }
        ui.label(RichText::new(info.join("・")).size(12.0).color(p.text2));
        ui.add_space(8.0);
        match &pv.state {
            PreviewState::Loading(_) => {
                ui.add(egui::Spinner::new().color(p.accent));
            }
            PreviewState::Ready(preview::Content::Image(img, _)) => {
                let tex = pv.texture.get_or_insert_with(|| ui.ctx().load_texture("preview", img.clone(), egui::TextureOptions::LINEAR));
                let avail = ui.available_size();
                let [w, h] = img.size.map(|v| v as f32);
                let scale = (avail.x / w).min(avail.y / h).min(1.0);
                ui.image((tex.id(), vec2(w * scale, h * scale)));
            }
            PreviewState::Ready(preview::Content::Text { text, truncated }) => {
                egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(text.as_str()).monospace().size(12.5).color(p.text)).selectable(true).extend());
                    if *truncated {
                        ui.label(RichText::new(L!("（只顯示開頭的部分）", "(only the beginning is shown)")).size(12.0).color(p.text2));
                    }
                });
            }
            PreviewState::Ready(preview::Content::Unavailable(why)) => {
                ui.label(RichText::new(if why.is_empty() { L!("這種檔案無法預覽", "This kind of file cannot be previewed") } else { why.as_str() }).color(p.text2));
            }
        }
    }

    /// 開始偵測從壓縮檔內開啟的檔案。解出的檔名與項目名稱不同時（名稱被清理過）不偵測，避免存回成另一個項目
    fn watch(&mut self, archive: PathBuf, entry: String, file: PathBuf, password: Option<String>) {
        let base = entry.rsplit('/').next().unwrap_or(&entry);
        // 壓縮檔內的壓縮檔是暫存副本，存回也不會改到外層，所以不偵測
        if file_name(&file) != base || self.is_nested(&archive) {
            return;
        }
        self.watched.retain(|w| w.file != file);
        self.watched.push(Watched { archive_stamp: file_stamp(&archive), archive, entry, stamp: file_stamp(&file), file, password });
    }

    /// 每秒檢查一次開啟的檔案是否被修改；有修改時詢問是否存回（沒有其他對話框或工作時）
    fn check_watched(&mut self, ctx: &egui::Context) {
        if self.watched.is_empty() {
            return;
        }
        ctx.request_repaint_after(Duration::from_secs(1));
        if self.last_watch_check.elapsed() < Duration::from_secs(1) || self.dialog.is_some() || self.busy() {
            return;
        }
        self.last_watch_check = std::time::Instant::now();
        self.find_modified();
    }

    fn find_modified(&mut self) {
        // 暫存檔已被刪除（例如程式結束清除）時不再偵測
        self.watched.retain(|w| w.file.exists());
        if let Some(i) = self.watched.iter().position(|w| file_stamp(&w.file) != w.stamp) {
            self.dialog = Some(Dialog::SaveBack(i));
        }
    }

    /// 壓縮檔在開啟這個檔案之後被其他程式修改或取代時，存回前要提醒的文字
    fn save_back_warning(&self, i: usize) -> Option<String> {
        let w = self.watched.get(i)?;
        (file_stamp(&w.archive) != w.archive_stamp).then(|| {
            F!(
                "注意：開啟這個檔案之後，「{}」已被其他程式修改或取代。存回會寫進目前的這個壓縮檔。",
                "Note: \"{}\" was changed or replaced by another program after this file was opened. Saving will write into the archive as it is now.",
                file_name(&w.archive)
            )
        })
    }

    /// 回答存回詢問：記下目前的狀態（下次修改時再問），`save` 為真時更新壓縮檔
    fn answer_save_back(&mut self, i: usize, save: bool) {
        let Some(w) = self.watched.get_mut(i) else { return };
        w.stamp = file_stamp(&w.file);
        if save {
            let into = w.entry.rsplit_once('/').map(|(dir, _)| dir.to_string()).unwrap_or_default();
            let task = Task::Update { archive: w.archive.clone(), add: vec![w.file.clone()], into, remove: Vec::new(), password: w.password.clone() };
            self.spawn(task);
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
            return self.notice(L!("計算雜湊值", "Calculate hashes"), L!("請先選取檔案或資料夾。", "Select a file or folder first."));
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
            match l.handle.join().unwrap_or_else(|_| Err(Error::Unsupported(L!("讀取時發生異常", "An error occurred while reading").into()))) {
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
                    self.status_error = Some(F!("無法開啟 {}：{e}", "Cannot open {}: {e}", file_name(&l.path)));
                    self.addr = self.loc.display();
                }
            }
        }

        if self.job.as_ref().is_some_and(|j| j.is_finished()) {
            let (task, res) = self.job.take().unwrap().join();
            if let (Task::OpenInside { archive, entry, password, .. }, Ok(out)) = (&task, &res)
                && let Some(file) = &out.opened
            {
                self.watch(archive.clone(), entry.clone(), file.clone(), password.clone());
            }
            // Zipo 自己更新了壓縮檔（存回、加入、刪除）：不算被其他程式取代
            if let (Task::Update { archive, .. }, Ok(_)) = (&task, &res) {
                for w in self.watched.iter_mut().filter(|w| &w.archive == archive) {
                    w.archive_stamp = file_stamp(archive);
                }
            }
            if let (Task::PrepareDrag { archive, entries, .. }, Ok(out)) = (&task, &res)
                && !out.prepared.is_empty()
            {
                let mut entries = entries.clone();
                entries.sort();
                self.drag_cache.push(DragCache { archive: archive.clone(), stamp: file_stamp(archive), entries, items: out.prepared.clone() });
            }
            if let (Task::OpenNested { archive, strip, .. }, Ok(out)) = (&task, &res)
                && let Some(file) = &out.opened
            {
                let parent = Location::Archive { path: archive.clone(), inner: strip.clone() };
                self.nested.push(Nested { path: file.clone(), parent });
                self.navigate(Location::Archive { path: file.clone(), inner: String::new() });
            }
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
                        Task::OpenInside { archive, .. } | Task::OpenNested { archive, .. } | Task::PrepareDrag { archive, .. } => file_name(archive),
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
                Err(Error::Cancelled) => self.status_error = Some(L!("已取消", "Cancelled").into()),
                Err(e) => {
                    self.dialog = Some(Dialog::Notice {
                        title: L!("發生錯誤", "Error").into(),
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
        if ui.button(F!("{}  重新整理\tF5", "{}  Refresh\tF5", icon::ARROWS_CLOCKWISE)).clicked() {
            self.refresh();
        }
        ui.separator();
        ui.label(RichText::new(L!("排序方式", "Sort by")).size(12.0).color(pal(ui).text2));
        for (key, label) in [(SortKey::Name, L!("名稱", "Name")), (SortKey::Modified, L!("修改日期", "Modified")), (SortKey::Type, L!("類型", "Type")), (SortKey::Size, L!("大小", "Size"))] {
            if ui.radio(self.sort_key == key, label).clicked() {
                self.sort_key = key;
                self.resort();
            }
        }
        ui.separator();
        ui.checkbox(&mut self.preview_open, L!("預覽窗格\tAlt+P", "Preview pane\tAlt+P")).on_hover_text(L!("在右側預覽選取的圖片或文字檔", "Preview the selected image or text file on the right"));
        if ui
            .checkbox(&mut self.show_system, L!("顯示受保護的系統項目", "Show protected system items"))
            .on_hover_text(L!("例如家目錄中的 My Documents、Application Data 等相容性連結；檔案總管預設也不顯示", "For example compatibility links such as My Documents and Application Data in your home folder; File Explorer hides them too"))
            .changed()
        {
            self.refresh();
        }
        ui.menu_button(L!("ZIP 檔名編碼", "ZIP file name encoding"), |ui| {
            let current = ziptool_core::zip_codepage();
            let choices = [
                (None, L!("自動（依系統）", "Automatic (system)")),
                (Some(950), L!("繁體中文（Big5）", "Traditional Chinese (Big5)")),
                (Some(936), L!("簡體中文（GBK）", "Simplified Chinese (GBK)")),
                (Some(932), L!("日文（Shift_JIS）", "Japanese (Shift_JIS)")),
                (Some(949), L!("韓文（EUC-KR）", "Korean (EUC-KR)")),
                (Some(1252), L!("西歐語系（Windows-1252）", "Western European (Windows-1252)")),
            ];
            for (cp, label) in choices {
                if ui.radio(current == cp, label).clicked() {
                    ziptool_core::set_zip_codepage(cp);
                    self.refresh();
                }
            }
        })
        .response
        .on_hover_text(L!("舊式 zip（例如 Windows 內建壓縮產生的）沒有記錄檔名編碼；中文檔名出現亂碼時可在這裡切換", "Older zips (for example those made by Windows) don't record the file name encoding; switch here if names look garbled"));
        ui.separator();
        let dark = ui.visuals().dark_mode;
        let (glyph, label) = if dark { (icon::SUN, L!("淺色模式", "Light mode")) } else { (icon::MOON, L!("深色模式", "Dark mode")) };
        if ui.button(format!("{glyph}  {label}")).clicked() {
            ui.ctx().set_theme(if dark { egui::Theme::Light } else { egui::Theme::Dark });
        }
    }

    /// 「⋯」選單：開啟壓縮檔、選取、檔案總管整合、關於、結束
    fn more_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(220.0);
        if ui.button(F!("{}  開啟壓縮檔…", "{}  Open archive…", icon::FOLDER_OPEN)).clicked()
            && let Some(p) = rfd::FileDialog::new().pick_file()
        {
            self.open_path_arg(p);
        }
        ui.separator();
        if ui.button(F!("{}  全選\tCtrl+A", "{}  Select all\tCtrl+A", icon::CHECK_SQUARE)).clicked() {
            self.sel = (0..self.rows.len()).collect();
        }
        if ui.button(F!("{}  取消選取", "{}  Select none", icon::SQUARE)).clicked() {
            self.sel.clear();
        }
        if ui.button(F!("{}  反向選取", "{}  Invert selection", icon::SWAP)).clicked() {
            self.sel = (0..self.rows.len()).filter(|i| !self.sel.contains(i)).collect();
        }
        ui.separator();
        let installed = shell::is_installed(shell::ROOT);
        let label = if installed { L!("移除檔案總管右鍵選單", "Remove from File Explorer context menu") } else { L!("加入檔案總管右鍵選單", "Add to File Explorer context menu") };
        let r = ui.button(format!("{}  {label}", icon::CURSOR_CLICK)).on_hover_text(
            L!("在檔案總管的右鍵選單加入「加入壓縮檔」「解壓縮到這裡」等項目（Windows 11 位於「顯示其他選項」）。\n只修改目前使用者的設定，隨時可以移除。", "Adds items such as \"Add to archive\" and \"Extract here\" to the File Explorer context menu (on Windows 11, under \"Show more options\").\nOnly the current user's settings are changed, and it can be removed at any time."),
        );
        if r.clicked() {
            let res = if installed {
                shell::uninstall(shell::ROOT)
            } else {
                std::env::current_exe().and_then(|exe| shell::install(shell::ROOT, &exe))
            };
            let (title, msg) = match (res, installed) {
                (Ok(()), false) => (L!("已加入右鍵選單", "Context menu added"), L!("在檔案總管中對檔案或壓縮檔按右鍵即可使用。Windows 11 請先點選「顯示其他選項」。", "Right-click a file or archive in File Explorer to use it. On Windows 11, choose \"Show more options\" first.").to_string()),
                (Ok(()), true) => (L!("已移除右鍵選單", "Context menu removed"), L!("檔案總管的右鍵選單已恢復原狀。", "The File Explorer context menu has been restored.").to_string()),
                (Err(e), _) => (L!("無法變更右鍵選單", "Cannot change the context menu"), ziptool_core::describe(&e)),
            };
            self.notice(title, msg);
        }
        let r = ui.button(F!("{}  設為壓縮檔預設程式…", "{}  Set as default app for archives…", icon::ARCHIVE)).on_hover_text(
            L!("讓 Zipo 成為 .zip、.7z 等壓縮檔的預設開啟程式。\nWindows 不允許程式自行變更預設程式，接下來會開啟「設定」，請在那裡選取 Zipo。", "Make Zipo the default app for .zip, .7z and other archives.\nWindows doesn't let apps change defaults themselves, so Settings will open next; choose Zipo there."),
        );
        if r.clicked() {
            match std::env::current_exe().and_then(|exe| shell::register_default(shell::SOFTWARE, &exe)) {
                Ok(()) => {
                    shell::open_default_apps_settings();
                    self.notice(
                        L!("請在「設定」中完成", "Finish in Settings"),
                        L!("已開啟 Windows「設定 › 預設應用程式 › Zipo」。請點選 .zip（以及其他想要的格式），再選擇 Zipo。", "Windows Settings › Default apps › Zipo is now open. Click .zip (and any other formats you want), then choose Zipo."),
                    );
                }
                Err(e) => self.notice(L!("無法註冊預設程式", "Cannot register as a default app"), ziptool_core::describe(&e)),
            }
        }
        if shell::is_default_registered(shell::SOFTWARE) {
            let r = ui.button(F!("{}  移除預設程式註冊", "{}  Remove default app registration", icon::X_CIRCLE)).on_hover_text(L!("從 Windows 的預設程式清單移除 Zipo。", "Remove Zipo from the Windows default apps list."));
            if r.clicked() {
                match shell::unregister_default(shell::SOFTWARE) {
                    Ok(()) => self.notice(L!("已移除預設程式註冊", "Default app registration removed"), L!("壓縮檔會改由 Windows 原本的程式開啟。", "Archives will open with the original Windows app again.")),
                    Err(e) => self.notice(L!("無法移除預設程式註冊", "Cannot remove the default app registration"), ziptool_core::describe(&e)),
                }
            }
        }
        ui.separator();
        let checking = matches!(self.update_work, Some(UpdateWork::Check { .. }));
        if ui.add_enabled(!checking, egui::Button::new(F!("{}  檢查更新…", "{}  Check for updates…", icon::ARROWS_CLOCKWISE))).clicked() {
            self.start_update_check(true);
        }
        ui.checkbox(&mut self.auto_update, L!("啟動時自動檢查更新", "Check for updates at startup"));
        ui.separator();
        // 兩種語言並列，切換錯了也找得回來
        ui.label(RichText::new("語言 / Language").size(12.0).color(pal(ui).text2));
        for (lang, label) in [(Language::Auto, L!("依系統設定", "System default")), (Language::Chinese, "繁體中文"), (Language::English, "English")] {
            if ui.radio(self.language == lang, label).clicked() {
                self.language = lang;
                ziptool_core::i18n::set_english(lang.english());
            }
        }
        if ui.button(F!("{}  關於 Zipo", "{}  About Zipo", icon::INFO)).clicked() {
            self.dialog = Some(Dialog::About);
        }
        if ui.button(F!("{}  結束", "{}  Exit", icon::SIGN_OUT)).clicked() {
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
        let nested = self.read_only_reason().is_some();
        ToolbarState {
            in_archive,
            compress: (in_archive && !nested) || (!in_archive && selectable),
            extract: in_archive || has_archive,
            test: in_archive || has_archive,
            info: has_sel || in_archive,
            delete: in_archive && has_sel && !nested,
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
                button(ui, true, icon::PLUS_CIRCLE, L!("加入", "Add"), L!("選擇檔案加入目前開啟的壓縮檔", "Choose files to add to the open archive"), "", Action::Add);
            } else {
                button(ui, st.compress, icon::FILE_ZIP, L!("壓縮", "Compress"), L!("把選取的檔案或資料夾壓縮成壓縮檔", "Compress the selected files or folders into an archive"), L!("先選取要壓縮的檔案或資料夾", "Select files or folders to compress first"), Action::Add);
            }
            let extract_tip = if st.in_archive { L!("解壓縮選取的項目（未選取則全部）", "Extract the selected items (everything if nothing is selected)") } else { L!("解壓縮選取的壓縮檔", "Extract the selected archives") };
            button(ui, st.extract, icon::ARROW_SQUARE_OUT, L!("解壓縮", "Extract"), extract_tip, L!("選取的項目中沒有壓縮檔", "No archive is selected"), Action::Extract);
            button(ui, st.test, icon::SEAL_CHECK, L!("測試", "Test"), L!("測試壓縮檔是否完整、沒有損壞", "Test whether the archive is intact"), L!("選取的項目中沒有壓縮檔", "No archive is selected"), Action::Test);
            separator_v(ui);
            button(ui, st.info, icon::INFO, L!("內容", "Properties"), L!("顯示選取項目的詳細資訊", "Show details of the selected items"), L!("先選取項目", "Select an item first"), Action::Info);
            // 刪除只用在壓縮檔內的項目；在一般資料夾不顯示，避免誤以為會刪除電腦上的檔案
            if st.in_archive {
                button(ui, st.delete, icon::TRASH, L!("刪除", "Delete"), L!("從壓縮檔刪除選取的項目（Delete）", "Delete the selected items from the archive (Delete)"), L!("先選取要刪除的項目", "Select items to delete first"), Action::Delete);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let more = command_button(ui, icon::DOTS_THREE, None).on_hover_text(L!("更多", "More"));
                egui::Popup::menu(&more).show(|ui| self.more_menu(ui));
                let view = command_button(ui, icon::SLIDERS_HORIZONTAL, Some(L!("檢視", "View"))).on_hover_text(L!("排序、顯示方式與外觀", "Sorting, display and appearance"));
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
            let mut out = vec![(L!("本機", "This PC").to_string(), Location::Computer)];
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
        self.crumbs_for(&self.loc, &dir_parts)
    }

    fn crumbs_for(&self, loc: &Location, dir_parts: &dyn Fn(&Path) -> Vec<(String, Location)>) -> Vec<(String, Location)> {
        match loc {
            Location::Computer => vec![(L!("本機", "This PC").into(), Location::Computer)],
            Location::Dir(d) => dir_parts(d),
            Location::Archive { path, inner } => {
                // 壓縮檔內的壓縮檔：接在外層壓縮檔的路徑之後，不顯示暫存資料夾
                let mut out = match self.nested.iter().find(|n| &n.path == path) {
                    Some(n) => self.crumbs_for(&n.parent, dir_parts),
                    None => path.parent().map(dir_parts).unwrap_or_default(),
                };
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
            if ui.add_enabled_ui(!self.back.is_empty(), |ui| icon_button(ui, icon::ARROW_LEFT)).inner.on_hover_text(L!("上一頁（Alt+←）", "Back (Alt+←)")).clicked() {
                self.go_back();
            }
            if ui.add_enabled_ui(!self.forward.is_empty(), |ui| icon_button(ui, icon::ARROW_RIGHT)).inner.on_hover_text(L!("下一頁（Alt+→）", "Forward (Alt+→)")).clicked() {
                self.go_forward();
            }
            if ui.add_enabled_ui(self.loc.parent().is_some(), |ui| icon_button(ui, icon::ARROW_UP)).inner.on_hover_text(L!("上一層（Backspace）", "Up (Backspace)")).clicked() {
                self.go_up();
            }
            ui.add_space(4.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.allocate_ui_with_layout(vec2(240.0, CONTROL_H), Layout::left_to_right(Align::Center), |ui| {
                    input_frame(ui, icon::MAGNIFYING_GLASS, |ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.filter)
                                .frame(egui::Frame::NONE)
                                .hint_text(if matches!(self.loc, Location::Archive { .. }) { F!("搜尋 {here}（含子資料夾）", "Search {here} (including subfolders)") } else { F!("搜尋 {here}", "Search {here}") })
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
                                let more = ui.add(egui::Button::new(RichText::new("…").color(p.text2)).frame(false)).on_hover_text(L!("顯示上層資料夾", "Show parent folders"));
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
                ui.label(RichText::new(F!("正在讀取 {}…", "Reading {}…", file_name(&l.path))).color(p.text2));
                stop = ui.add(secondary_button(ui, L!("取消", "Cancel"))).clicked();
            });
            if stop {
                // 放棄這次讀取；背景執行緒讀完後結果會被丟棄
                self.loading = None;
                self.addr = self.loc.display();
            }
            return;
        }
        let in_archive = matches!(self.loc, Location::Archive { .. });
        let nested = self.read_only_reason().is_some();
        let mut click: Option<(usize, Modifiers)> = None;
        let mut double: Option<usize> = None;
        let mut ctx_action: Option<Action> = None;
        let mut right_click: Option<usize> = None;
        let mut drag_row: Option<usize> = None;
        let mut sort_click: Option<SortKey> = None;
        let modifiers = ui.input(|i| i.modifiers);

        // 欄位順序與檔案總管相同：名稱、修改日期、類型（壓縮檔內為壓縮後大小）、大小
        let cols: Vec<(SortKey, &str, bool)> = if in_archive {
            vec![(SortKey::Name, L!("名稱", "Name"), false), (SortKey::Modified, L!("修改日期", "Modified"), false), (SortKey::Packed, L!("壓縮後", "Packed"), true), (SortKey::Size, L!("大小", "Size"), true)]
        } else {
            vec![(SortKey::Name, L!("名稱", "Name"), false), (SortKey::Modified, L!("修改日期", "Modified"), false), (SortKey::Type, L!("類型", "Type"), false), (SortKey::Size, L!("大小", "Size"), true)]
        };

        let mut table = TableBuilder::new(ui)
            .id_salt(("files", in_archive, self.preview_open))
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
                        // 整個標題格都可以點（不只是文字），滑過時顯示底色
                        let cell = ui.max_rect();
                        let hit = ui.interact(cell, ui.id().with(("sort", k)), Sense::click());
                        if hit.hovered() {
                            ui.painter().rect_filled(cell, 0.0, p.hover);
                        }
                        if hit.clicked() {
                            sort_click = Some(*key);
                        }
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
                            ui.add(egui::Label::new(RichText::new(text.trim()).size(12.0).color(p.text2)).selectable(false).sense(Sense::hover()));
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
                            name.on_hover_text(F!("實際名稱：{}", "Actual name: {}", r.name));
                        }
                        if r.encrypted {
                            ui.label(RichText::new(icon::LOCK_SIMPLE).size(14.0).color(p.text2)).on_hover_text(L!("已加密", "Encrypted"));
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
                        if ui.button(F!("{}  開啟", "{}  Open", icon::FOLDER_OPEN)).clicked() {
                            ctx_action = Some(Action::Open);
                        }
                        ui.separator();
                        if !in_archive && !r.is_drive && ui.button(F!("{}  壓縮…", "{}  Compress…", icon::FILE_ARCHIVE)).clicked() {
                            ctx_action = Some(Action::Add);
                        }
                        if (in_archive || is_archive_file) && ui.button(F!("{}  解壓縮…", "{}  Extract…", icon::ARROW_SQUARE_OUT)).clicked() {
                            ctx_action = Some(Action::Extract);
                        }
                        if (in_archive || is_archive_file) && ui.button(F!("{}  測試", "{}  Test", icon::SEAL_CHECK)).clicked() {
                            ctx_action = Some(Action::Test);
                        }
                        if in_archive && !nested && ui.button(F!("{}  從壓縮檔刪除", "{}  Delete from archive", icon::TRASH)).clicked() {
                            ctx_action = Some(Action::Delete);
                        }
                        if !r.is_drive && ui.button(F!("{}  計算雜湊值", "{}  Calculate hashes", icon::FINGERPRINT)).clicked() {
                            ctx_action = Some(Action::Hash);
                        }
                        ui.separator();
                        if ui.button(F!("{}  內容", "{}  Properties", icon::INFO)).clicked() {
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
            ui.label(RichText::new(F!("{} 個項目", "{} items", self.rows.len())).size(12.0).color(p.text2));
            if !self.sel.is_empty() {
                let size: u64 = self.sel.iter().filter_map(|&i| self.rows[i].size).sum();
                let has_size = self.sel.iter().any(|&i| self.rows[i].size.is_some_and(|s| s > 0));
                let size = if has_size { F!("，{}", ", {}", human(size)) } else { String::new() };
                ui.label(RichText::new(F!("已選取 {} 個項目{size}", "{} selected{size}", self.sel.len())).size(12.0).color(p.text2));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let (Location::Archive { .. }, Some(c)) = (&self.loc, &self.cache) {
                    let (fmt, size) = (c.format, c.stamp.0);
                    let enc = if c.entries.iter().any(|e| e.encrypted) || c.password.is_some() { F!("  ·  {} 已加密", "  ·  {} encrypted", icon::LOCK_SIMPLE) } else { String::new() };
                    ui.label(RichText::new(F!("{fmt} 壓縮檔  ·  {}{enc}", "{fmt} archive  ·  {}{enc}", human(size))).size(12.0).color(p.text2));
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
        if ctx.input(|i| i.modifiers.alt && i.key_pressed(Key::P)) {
            self.preview_open = !self.preview_open;
            return;
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
                    if title == L!("發生錯誤", "Error") || !issues.is_empty() { (icon::WARNING_CIRCLE, p.danger) } else { (glyph, color) };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(glyph).size(22.0).color(color));
                    ui.label(RichText::new(title.as_str()).size(16.0).strong().color(p.text));
                });
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(message.as_str()).color(p.text)).wrap());
                if !issues.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new(F!("略過 {} 個項目：", "{} item(s) skipped:", issues.len())).strong().color(p.text));
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
                    close = ui.add(primary_button(ui, L!("確定", "OK"))).clicked();
                    if let Some(d) = open_dir
                        && ui.add(secondary_button(ui, L!("開啟資料夾", "Open folder"))).clicked()
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
                    ui.label(RichText::new(L!("這個壓縮檔的壓縮比異常高", "This archive has an unusually high compression ratio")).size(16.0).strong().color(p.text));
                });
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(message.as_str()).color(p.text)).wrap());
                ui.label(RichText::new(L!("惡意的壓縮檔可能解出遠超過預期的資料、塞滿磁碟。只有在確定來源可信時才繼續。", "A malicious archive can expand to far more data than expected and fill up the disk. Continue only if you trust the source.")).color(p.text2));
                ui.add_space(6.0);
                let (mut go, mut stop) = (false, false);
                ui.allocate_ui_with_layout(vec2(ui.available_width(), 32.0), Layout::right_to_left(Align::Center), |ui| {
                    stop = ui.add(primary_button(ui, L!("取消", "Cancel"))).clicked();
                    go = ui.add(secondary_button(ui, L!("仍要解壓縮", "Extract anyway"))).clicked();
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
                    stop = ui.add(secondary_button(ui, L!("取消", "Cancel"))).clicked();
                    go = ui.add(primary_button(ui, if *danger { L!("刪除", "Delete") } else { L!("加入", "Add") })).clicked();
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
                    ui.label(RichText::new(L!("要執行壓縮檔中的程式嗎？", "Run a program from the archive?")).size(16.0).strong().color(p.text));
                });
                ui.add_space(8.0);
                ui.label(RichText::new(name.as_str()).strong().color(p.text));
                ui.label(RichText::new(L!("這是可執行的檔案。來源不明的壓縮檔可能夾帶惡意程式，只有在確定來源可信時才執行。", "This is an executable file. Archives from unknown sources may contain malware; run it only if you trust the source.")).color(p.text2));
                ui.add_space(6.0);
                let (mut go, mut stop) = (false, false);
                ui.allocate_ui_with_layout(vec2(ui.available_width(), 32.0), Layout::right_to_left(Align::Center), |ui| {
                    stop = ui.add(primary_button(ui, L!("取消", "Cancel"))).clicked();
                    go = ui.add(secondary_button(ui, L!("執行", "Run"))).clicked();
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
                ui.label(RichText::new(L!("以 7-Zip 為藍本、使用 Rust 撰寫的壓縮工具。", "An archiver modeled on 7-Zip, written in Rust.")).color(p.text));
                ui.label(RichText::new(L!("支援 7z、zip、tar、tar.gz、tar.bz2、tar.xz、tar.zst、gz、bz2、xz、zst。", "Supports 7z, zip, tar, tar.gz, tar.bz2, tar.xz, tar.zst, gz, bz2, xz, zst and ISO.")).color(p.text2));
                let (ok, cancel) = dialog_buttons_single(ui);
                keep = !(ok || cancel);
            }
            Dialog::UpdateAvailable(release) => {
                let p = pal(ui);
                ui.set_width(440.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::ARROW_CIRCLE_UP).size(22.0).color(p.accent));
                    ui.label(RichText::new(F!("有新版本：Zipo {}", "New version: Zipo {}", release.version)).size(16.0).strong().color(p.text));
                });
                ui.add_space(8.0);
                let detail = match &release.asset {
                    Some(a) => F!(
                        "目前使用的是 {}。
更新檔約 {}，驗證完整後會取代程式並自動重新啟動。", "You're using {}.
The update is about {}; once verified it replaces the program and restarts automatically.",
                        env!("CARGO_PKG_VERSION"),
                        ziptool_core::format_size(a.size)
                    ),
                    None => F!("目前使用的是 {}。
這個版本需要到下載頁面手動更新。", "You're using {}.
This version must be updated manually from the download page.", env!("CARGO_PKG_VERSION")),
                };
                ui.add(egui::Label::new(RichText::new(detail).color(p.text)).wrap());
                if link(ui, L!("查看更新內容", "See what's new")).clicked() {
                    open_path(Path::new(&release.page));
                }
                ui.add_space(6.0);
                let (mut go, mut later) = (false, false);
                ui.allocate_ui_with_layout(vec2(ui.available_width(), 32.0), Layout::right_to_left(Align::Center), |ui| {
                    later = ui.add(secondary_button(ui, L!("稍後", "Later"))).clicked();
                    go = ui.add(primary_button(ui, if release.asset.is_some() { L!("立即更新", "Update now") } else { L!("開啟下載頁面", "Open download page") })).clicked();
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
                dialog_title(ui, icon::FINGERPRINT, L!("雜湊值", "Hashes"));
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
                                if ui.small_button(icon::COPY).on_hover_text(L!("複製", "Copy")).clicked() {
                                    ui.ctx().copy_text(value.to_string());
                                }
                                ui.end_row();
                            }
                        });
                        ui.add_space(10.0);
                    }
                });
                ui.add_space(4.0);
                ui.label(RichText::new(L!("比對：貼上網站提供的雜湊值", "Compare: paste the hash published by the website")).color(p.text2));
                ui.add(egui::TextEdit::singleline(expected).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace));
                if !expected.trim().is_empty() {
                    let hit = rows.iter().find_map(|(name, h)| h.matches(expected).map(|algo| (name, algo)));
                    match hit {
                        Some((name, algo)) => ui.label(RichText::new(F!("{}  符合「{name}」的 {algo}", "{}  Matches the {algo} of \"{name}\"", icon::CHECK_CIRCLE)).color(p.success)),
                        None => ui.label(RichText::new(F!("{}  不符合任何一個檔案的雜湊值", "{}  Doesn't match any file's hash", icon::X_CIRCLE)).color(p.danger)),
                    };
                }
                let (ok, cancel) = dialog_buttons_single(ui);
                keep = !(ok || cancel);
            }
            Dialog::SaveBack(i) => {
                let i = *i;
                let p = pal(ui);
                ui.set_width(440.0);
                let (name, archive) = self.watched.get(i).map(|w| (file_name(&w.file), file_name(&w.archive))).unwrap_or_default();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::FLOPPY_DISK).size(22.0).color(p.accent));
                    ui.label(RichText::new(L!("要把修改存回壓縮檔嗎？", "Save changes back to the archive?")).size(16.0).strong().color(p.text));
                });
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(F!("「{name}」已在其他程式中修改並儲存。\n要把新版本更新到「{archive}」嗎？", "\"{name}\" was changed and saved in another program.\nUpdate \"{archive}\" with the new version?")).color(p.text)).wrap());
                if let Some(warning) = self.save_back_warning(i) {
                    ui.add_space(6.0);
                    ui.add(egui::Label::new(RichText::new(warning).color(p.danger)).wrap());
                }
                ui.add_space(6.0);
                let (mut save, mut skip) = (false, false);
                ui.allocate_ui_with_layout(vec2(ui.available_width(), 32.0), Layout::right_to_left(Align::Center), |ui| {
                    skip = ui.add(secondary_button(ui, L!("不要", "Don't save"))).clicked();
                    save = ui.add(primary_button(ui, L!("存回壓縮檔", "Save to archive"))).clicked();
                });
                skip |= ui.input(|i| i.key_pressed(Key::Escape));
                if save || skip {
                    keep = false;
                    // 先關閉這個對話框，再開始更新（更新完成會顯示結果）
                    self.dialog = None;
                    self.answer_save_back(i, save);
                }
            }
            Dialog::Updating => {
                let p = pal(ui);
                ui.set_width(400.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(20.0).color(p.accent));
                    ui.label(RichText::new(L!("正在更新 Zipo…", "Updating Zipo…")).size(16.0).strong().color(p.text));
                });
                ui.add_space(8.0);
                ui.label(RichText::new(L!("正在下載並驗證更新檔，完成後會自動重新啟動。", "Downloading and verifying the update; Zipo will restart when it's done.")).color(p.text2));
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
                    ui.label(RichText::new(L!("目的資料夾已經有同名的檔案：", "The destination already has a file with this name:")).strong().color(p.text));
                    ui.add(egui::Label::new(RichText::new(q.display().to_string()).size(12.5).color(p.text2)).wrap());
                    if let Ok(m) = std::fs::metadata(q) {
                        let info = F!("現有檔案：{}，{}", "Existing file: {}, {}", human(m.len()), format_time(m.modified().ok()));
                        ui.label(RichText::new(info).size(12.5).color(p.text2));
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new(L!("要用壓縮檔中的檔案取代嗎？", "Replace it with the file from the archive?")).color(p.text));
                    ui.horizontal_wrapped(|ui| {
                        use ziptool_core::OverwriteAnswer as A;
                        let choices = [(L!("取代", "Replace"), A::Yes), (L!("全部取代", "Replace all"), A::YesAll), (L!("略過", "Skip"), A::No), (L!("全部略過", "Skip all"), A::NoAll), (L!("全部自動重新命名", "Rename all"), A::RenameAll)];
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
                k(ui, L!("已經過", "Elapsed"));
                v(ui, format_duration(s.elapsed));
                k(ui, L!("進度", "Progress"));
                v(ui, format!("{:.0}%", frac * 100.0));
                ui.end_row();
                k(ui, L!("剩餘時間", "Remaining"));
                v(ui, s.remaining.map(format_duration).unwrap_or_else(|| "--:--:--".into()));
                k(ui, L!("已處理", "Processed"));
                v(ui, format!("{} / {}", human(s.done), human(s.total)));
                ui.end_row();
                k(ui, L!("速度", "Speed"));
                v(ui, format!("{}/s", human(s.speed as u64)));
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 32.0), Layout::right_to_left(Align::Center), |ui| {
                let label = if s.cancelling { L!("正在取消…", "Cancelling…") } else { L!("取消", "Cancel") };
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
        ok = ui.add(primary_button(ui, L!("確定", "OK"))).clicked();
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

/// 解出壓縮檔內的單一檔案到暫存資料夾後讀取預覽，再刪除暫存資料夾
fn preview_inside(archive: &Path, entry: &str, strip: &str, password: Option<String>, cancel: Arc<AtomicBool>) -> preview::Content {
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let dir = std::env::temp_dir().join("Zipo").join(format!("{}-preview-{stamp}", std::process::id()));
    // 宣告的大小可能造假：實際寫出超過上限就停止
    let opts = ziptool_core::ExtractOptions {
        password,
        selection: Some(vec![entry.to_string()]),
        strip_prefix: strip.to_string(),
        max_bytes: Some(preview::MAX_ARCHIVE_PREVIEW),
        ..Default::default()
    };
    let content = match ziptool_core::extract(archive, &dir, &opts, &CancelFlag(cancel)) {
        // 解出的檔名可能經過清理（例如 C:x.txt → C_x.txt），不可用項目名稱組路徑
        Ok(r) if r.issues.is_empty() => match jobs::extracted_file(&dir) {
            Some(file) => preview::load(&file),
            None => preview::Content::Unavailable(L!("無法讀取這個項目", "Cannot read this item").into()),
        },
        Ok(r) => preview::Content::Unavailable(r.issues.first().map(|i| i.reason.clone()).unwrap_or_default()),
        Err(Error::PasswordRequired | Error::WrongPassword) => preview::Content::Unavailable(L!("需要密碼才能預覽", "A password is required to preview").into()),
        Err(Error::TooLarge { limit }) => preview::Content::Unavailable(F!("超過 {} 的檔案不預覽", "Files larger than {} are not previewed", ziptool_core::format_size(limit))),
        Err(e) => preview::Content::Unavailable(e.to_string()),
    };
    let _ = std::fs::remove_dir_all(&dir);
    content
}

fn nested_read_only() -> &'static str {
    L!(
        "這是壓縮檔內的壓縮檔（暫存的副本），只能瀏覽、解壓縮，無法直接修改。若要修改，請先把它解壓縮出來。",
        "This is an archive inside an archive (a temporary copy); it can be browsed and extracted but not modified. To change it, extract it first."
    )
}

/// 拖曳時直接解壓縮的大小上限；更大時改在背景解壓縮（顯示進度），完成後再拖曳一次
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
    cleanup_temp_in(&std::env::temp_dir().join("Zipo"), only_mine);
}

/// 行程是否仍在執行
#[cfg(windows)]
fn process_running(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, E_ACCESSDENIED, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    // SAFETY: 只查詢狀態，取得的控制代碼用完即關閉
    unsafe {
        match OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            Ok(h) => {
                let running = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
                let _ = CloseHandle(h);
                running
            }
            // 存在但沒有權限開啟（例如以系統管理員身分執行）
            Err(e) => e.code() == E_ACCESSDENIED,
        }
    }
}

/// 其他平台無法簡單判斷，依時間清理
#[cfg(not(windows))]
fn process_running(_pid: u32) -> bool {
    false
}

fn cleanup_temp_in(base: &Path, only_mine: bool) {
    let Ok(dir) = std::fs::read_dir(base) else { return };
    let prefix = format!("{}-", std::process::id());
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d > Duration::from_secs(86_400));
        // 名稱開頭是建立它的行程編號（核心接回分割檔的是 zipo-join-編號）；該行程仍在執行就保留
        let owner = name.strip_prefix("zipo-join-").unwrap_or(&name).split('-').next().and_then(|p| p.parse::<u32>().ok());
        if (only_mine && name.starts_with(&prefix)) || (!only_mine && old && !owner.is_some_and(process_running)) {
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

/// 測試用：會在壓縮檔內計算雜湊值的測試依序執行，檢查暫存資料夾時才不會看到其他測試正在使用的
#[cfg(test)]
static HASH_INSIDE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        // 截圖用的執行（只在開發版）不寫入使用者的設定，避免改掉上次的資料夾等
        #[cfg(debug_assertions)]
        if self.devshot.is_some() {
            return;
        }
        self.save_settings(storage);
    }

    /// 程式結束時清除本程式建立的暫存資料夾（開啟壓縮檔內的檔案、拖曳到程式外）
    fn on_exit(&mut self) {
        ziptool_core::keep_joined_volumes(false);
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
        self.check_watched(&ctx);
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
        if self.preview_open {
            self.update_preview(&ctx);
            egui::Panel::right("preview")
                .resizable(true)
                .default_size(320.0)
                .min_size(220.0)
                .max_size(640.0)
                .frame(egui::Frame::new().fill(p.bg).inner_margin(16))
                .show(ui, |ui| self.preview_pane(ui));
        }
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
                    "preview" => self.preview_open = true,
                    "saveback" => {
                        let file = std::env::temp_dir().join("報告.docx");
                        self.watched.push(Watched { archive: "專案資料.zip".into(), entry: "報告.docx".into(), file, stamp: None, archive_stamp: None, password: None });
                        self.dialog = Some(Dialog::SaveBack(0));
                    }
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
                            self.spawn(Task::Extract { archives: vec![path.clone()], dest, opts, smart: true, here: false, then: Then::default() });
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
            here: false,
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
        // 放棄讀取時通知背景工作停止（例如正在接合大型分割檔）
        let cancel = app.loading.as_ref().unwrap()._cancel.0.clone();
        let elsewhere = t.path().join("src");
        app.navigate(Location::Dir(elsewhere.clone()));
        assert!(cancel.load(Ordering::Relaxed), "應通知背景讀取停止");
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
        let task = Task::Extract { archives: vec![ar], dest: out.join("proj"), opts: Default::default(), smart: true, here: false, then: Then::default() };
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
        assert_eq!(parse_args(args(&["--extract-here", "x.zip"])), StartAction::ExtractHere(vec!["x.zip".into()]));
        assert_eq!(parse_args(args(&["--extract-folder", "x.zip"])), StartAction::ExtractFolder(vec!["x.zip".into()]));
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
        app.apply_start(StartAction::ExtractHere(vec![ar]));
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

    /// 右鍵選單對多個壓縮檔（合併成一次）：「解壓縮到這裡」各自解到所在的資料夾；「解壓縮到資料夾」開一個對話框
    #[test]
    fn extract_several_archives_from_context_menu() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        let mut archives = Vec::new();
        for n in ["一", "二"] {
            let src = root.join(format!("{n}.txt"));
            std::fs::write(&src, n).unwrap();
            let ar = root.join(format!("{n}.zip"));
            ziptool_core::compress(std::slice::from_ref(&src), &ar, Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();
            std::fs::remove_file(&src).unwrap();
            archives.push(ar);
        }
        let ctx = egui::Context::default();
        let mut app = App::new(None);
        app.apply_start(StartAction::ExtractHere(archives.clone()));
        for _ in 0..400 {
            app.poll(&ctx);
            if app.job.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(app.dialog, Some(Dialog::Notice { ok: true, .. })));
        assert_eq!(std::fs::read_to_string(root.join("一.txt")).unwrap(), "一", "直接解到所在的資料夾");
        assert_eq!(std::fs::read_to_string(root.join("二.txt")).unwrap(), "二");

        let mut app = App::new(None);
        app.apply_start(StartAction::ExtractFolder(archives));
        assert!(matches!(&app.dialog, Some(Dialog::Extract(d)) if d.archive_count() == 2));
    }

    /// 右鍵選單對分割壓縮檔的第一個（.001）「解壓縮到這裡」「解壓縮到資料夾」
    #[test]
    fn extract_split_volumes_from_context_menu() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        let src = root.join("doc");
        std::fs::create_dir(&src).unwrap();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        std::fs::write(src.join("big.bin"), &data).unwrap();
        let ar = root.join("20261013.zip");
        let opts = ziptool_core::CompressOptions { level: Some(0), volume_size: Some(100_000), ..Default::default() };
        ziptool_core::compress(std::slice::from_ref(&src), &ar, Format::Zip, &opts, &ziptool_core::NoProgress).unwrap();
        std::fs::remove_dir_all(&src).unwrap();
        let first = root.join("20261013.zip.001");
        assert!(root.join("20261013.zip.004").is_file(), "應分割成四個檔案");

        let ctx = egui::Context::default();
        let mut app = App::new(None);
        app.apply_start(StartAction::ExtractHere(vec![first.clone()]));
        for _ in 0..400 {
            app.poll(&ctx);
            if app.job.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(app.dialog, Some(Dialog::Notice { ok: true, .. })), "{:?}", app.dialog.as_ref().map(|_| ()));
        assert_eq!(std::fs::read(src.join("big.bin")).unwrap(), data);

        // 解壓縮到資料夾：預設資料夾名稱去掉 .zip.001
        assert_eq!(dialogs::default_dest(&first), root.join("20261013"));
        // 不是壓縮檔的分割檔（影片.mkv.001）：解出的就是一個檔案，預設放在同一個資料夾
        assert_eq!(dialogs::default_dest(&root.join("影片.mkv.001")), root);
    }
}

/// 啟動時的清理：超過一天的暫存資料夾，若建立它的 Zipo 仍在執行（例如開著等待存回）就保留
#[cfg(all(test, windows))]
mod cleanup_tests {
    use super::*;

    #[test]
    fn keeps_folders_of_running_instances() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path();
        let mut done = std::process::Command::new("cmd").args(["/C", "exit"]).spawn().unwrap();
        let dead = done.id();
        done.wait().unwrap();
        let alive = std::process::id();
        let make = |name: &str| {
            let d = base.join(name);
            std::fs::create_dir(&d).unwrap();
            std::fs::write(d.join("f.txt"), "x").unwrap();
            let old = SystemTime::now() - Duration::from_secs(3 * 86_400);
            // 資料夾要用 FILE_FLAG_BACKUP_SEMANTICS 才能開啟
            let f = std::os::windows::fs::OpenOptionsExt::custom_flags(std::fs::OpenOptions::new().write(true), 0x0200_0000).open(&d).unwrap();
            f.set_modified(old).unwrap();
            d
        };
        let running = make(&format!("{alive}-preview-1"));
        let running_join = make(&format!("zipo-join-{alive}-2"));
        let gone = make(&format!("{dead}-drag-3"));
        let gone_join = make(&format!("zipo-join-{dead}-4"));
        let other = make("unknown");
        cleanup_temp_in(base, false);
        assert!(running.exists() && running_join.exists(), "執行中的 Zipo 的暫存資料夾不可刪除");
        assert!(!gone.exists() && !gone_join.exists() && !other.exists());
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

    /// 大型項目：先在背景準備（有進度），完成後再拖曳一次即可立即開始
    #[test]
    fn large_items_are_prepared_in_background() {
        let t = tempfile::tempdir().unwrap();
        let (mut app, ctx) = archive_in_docs(&real_path(t.path()), None);
        let i = app.rows.iter().position(|r| r.name == "a.txt").unwrap();
        app.rows[i].size = Some(DRAG_OUT_MAX + 1);
        app.sel = [i].into();
        assert!(app.prepare_drag_out(vec!["docs/a.txt".into()]).is_none());
        assert!(matches!(app.job.as_ref().map(|j| &j.task), Some(Task::PrepareDrag { .. })));
        load(&mut app, &ctx);
        assert!(matches!(&app.dialog, Some(Dialog::Notice { title, .. }) if title == "已準備好拖曳"));
        app.dialog = None;
        let items = app.prepare_drag_out(vec!["docs/a.txt".into()]).expect("第二次應直接拖曳");
        assert!(app.job.is_none());
        assert_eq!(std::fs::read_to_string(&items[0]).unwrap(), "A");
    }

    /// 宣告的大小造假（看起來很小、實際超過上限）：寫到上限就停止，改在背景解壓縮
    #[test]
    fn understated_size_falls_back_to_background() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        let ar = root.join("liar.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&ar).unwrap());
        z.start_file("big.bin", zip::write::SimpleFileOptions::default()).unwrap();
        let block = vec![0u8; 1 << 20];
        for _ in 0..(DRAG_OUT_MAX >> 20) + 1 {
            std::io::Write::write_all(&mut z, &block).unwrap();
        }
        z.finish().unwrap();
        let ctx = egui::Context::default();
        let mut app = App::new(Some(ar.clone()));
        load(&mut app, &ctx);
        let i = app.rows.iter().position(|r| r.name == "big.bin").unwrap();
        app.rows[i].size = Some(1);
        app.sel = [i].into();
        assert!(app.prepare_drag_out(vec!["big.bin".into()]).is_none());
        assert!(matches!(app.job.as_ref().map(|j| &j.task), Some(Task::PrepareDrag { .. })));
        // 等背景工作結束（解出約 257 MB）
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while app.job.is_some() && std::time::Instant::now() < deadline {
            app.poll(&ctx);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(matches!(&app.dialog, Some(Dialog::Notice { title, .. }) if title == "已準備好拖曳"));
    }

    /// 同樣的項目不重複解壓縮；壓縮檔改變後重新解壓縮
    #[test]
    fn drag_cache_reuses_until_archive_changes() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        let (mut app, _ctx) = archive_in_docs(&root, None);
        app.sel = [app.rows.iter().position(|r| r.name == "a.txt").unwrap()].into();
        let first = app.prepare_drag_out(vec!["docs/a.txt".into()]).unwrap();
        let again = app.prepare_drag_out(vec!["docs/a.txt".into()]).unwrap();
        assert_eq!(first, again, "未改變時應沿用");
        // 修改壓縮檔（時間往後，確保看得出變動）
        let ar = root.join("d.zip");
        let f = std::fs::File::options().append(true).open(&ar).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(30)).unwrap();
        drop(f);
        let changed = app.prepare_drag_out(vec!["docs/a.txt".into()]).unwrap();
        assert_ne!(first, changed, "壓縮檔改變後應重新解壓縮");
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
    /// 欄位標題：點整個標題格（不只是文字）就排序，與檔案總管相同
    #[test]
    fn clicking_anywhere_in_header_sorts() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        std::fs::write(root.join("a.txt"), "aaaa").unwrap();
        let ctx = egui::Context::default();
        let mut app = App::new(Some(root));
        load(&mut app, &ctx);
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
        // 每個欄位標題的空白處（文字以外）
        for (x, key) in [(200.0, SortKey::Name), (520.0, SortKey::Modified), (640.0, SortKey::Type), (700.0, SortKey::Size)] {
            let at = egui::pos2(x, CONTROL_H / 2.0);
            let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
            app.sort_key = if key == SortKey::Name { SortKey::Size } else { SortKey::Name };
            frame(&mut app, vec![egui::Event::PointerMoved(at), press(true)]);
            frame(&mut app, vec![egui::Event::PointerMoved(at), press(false)]);
            frame(&mut app, vec![egui::Event::PointerGone]);
            assert_eq!(app.sort_key, key, "x = {x}");
        }
    }

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

    /// 壓縮檔內的搜尋找遍所有層；結果可以直接操作（計算雜湊值、開啟資料夾）
    #[test]
    fn search_inside_archive_reaches_all_levels() {
        let _lock = crate::HASH_INSIDE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        std::fs::create_dir_all(root.join("src/docs/deep")).unwrap();
        std::fs::write(root.join("src/docs/deep/abc.txt"), "abc").unwrap();
        std::fs::write(root.join("src/docs/other.md"), "x").unwrap();
        let ar = root.join("s.zip");
        ziptool_core::compress(&[root.join("src/docs")], &ar, Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();
        let ctx = egui::Context::default();
        let mut app = App::new(Some(ar.clone()));
        wait(&mut app, &ctx);
        assert_eq!(app.rows.len(), 1, "根目錄只有 docs");
        app.filter = "ABC".into();
        app.resort();
        let names: Vec<&str> = app.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["docs/deep/abc.txt"]);
        app.sel = [0].into();
        app.cmd_hash();
        wait(&mut app, &ctx);
        // 名稱包含所在路徑，方便辨認是哪一層的檔案
        assert_eq!(hashes_dialog(&app), [("docs/deep/abc.txt".to_string(), ABC.to_string())]);

        // 搜尋到的資料夾：開啟後進入該資料夾
        app.dialog = None;
        app.filter = "deep".into();
        app.resort();
        let i = app.rows.iter().position(|r| r.is_dir).unwrap();
        app.open_row(i);
        assert_eq!(app.loc, Location::Archive { path: ar.clone(), inner: "docs/deep".into() });
        assert!(app.filter.is_empty(), "換資料夾後清除搜尋");
    }

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
        let _lock = crate::HASH_INSIDE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let leftover: Vec<String> = std::fs::read_dir(std::env::temp_dir().join("Zipo"))
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with(&format!("{}-hash-", std::process::id()))).collect())
            .unwrap_or_default();
        assert!(leftover.is_empty(), "不應留下暫存資料夾：{leftover:?}");
    }
}

#[cfg(test)]
mod save_back_tests {
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

    fn content(ar: &Path, entry: &str) -> (String, usize) {
        let entries = ziptool_core::list(ar, None).unwrap();
        let count = entries.iter().filter(|e| e.name == entry).count();
        let out = tempfile::tempdir().unwrap();
        let opts = ziptool_core::ExtractOptions { selection: Some(vec![entry.into()]), ..Default::default() };
        ziptool_core::extract(ar, out.path(), &opts, &ziptool_core::NoProgress).unwrap();
        (std::fs::read_to_string(out.path().join(entry)).unwrap(), count)
    }

    /// 修改檔案：內容不同且修改時間往後，確保偵測得到
    fn edit(file: &Path, text: &str, secs: u64) {
        std::fs::write(file, text).unwrap();
        let f = std::fs::File::options().write(true).open(file).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(secs)).unwrap();
    }

    #[test]
    fn edited_file_is_saved_back_into_archive() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        std::fs::create_dir_all(root.join("src/docs")).unwrap();
        std::fs::write(root.join("src/docs/report.txt"), "v1").unwrap();
        let ar = root.join("e.zip");
        ziptool_core::compress(&[root.join("src/docs")], &ar, Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();

        let ctx = egui::Context::default();
        let mut app = App::new(Some(ar.clone()));
        wait(&mut app, &ctx);
        app.navigate(Location::Archive { path: ar.clone(), inner: "docs".into() });
        app.open_row(0);
        wait(&mut app, &ctx);
        assert_eq!(app.watched.len(), 1);
        let file = app.watched[0].file.clone();

        // 沒有修改：不詢問
        app.find_modified();
        assert!(app.dialog.is_none());

        // 修改後詢問，存回：壓縮檔內容更新且不會重複
        edit(&file, "v2", 10);
        app.find_modified();
        assert!(matches!(app.dialog, Some(Dialog::SaveBack(0))));
        app.dialog = None;
        app.answer_save_back(0, true);
        wait(&mut app, &ctx);
        assert_eq!(content(&ar, "docs/report.txt"), ("v2".to_string(), 1));
        app.dialog = None;
        app.find_modified();
        assert!(app.dialog.is_none(), "存回後不應再詢問");

        // 再次修改、選「不要」：壓縮檔不變，也不會一直詢問
        edit(&file, "v3", 20);
        app.find_modified();
        assert!(matches!(app.dialog, Some(Dialog::SaveBack(0))));
        app.dialog = None;
        app.answer_save_back(0, false);
        assert!(app.job.is_none());
        app.find_modified();
        assert!(app.dialog.is_none());
        assert_eq!(content(&ar, "docs/report.txt"), ("v2".to_string(), 1));
        // 自己存回、或在 Zipo 中修改壓縮檔：不算被取代
        assert!(app.save_back_warning(0).is_none());

        // 壓縮檔被其他程式換掉（同名的另一個檔案）：存回前提醒
        std::fs::write(root.join("other.txt"), "其他").unwrap();
        ziptool_core::compress(&[root.join("other.txt")], &ar, Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();
        let f = std::fs::File::options().append(true).open(&ar).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(40)).unwrap();
        drop(f);
        edit(&file, "v4", 30);
        app.find_modified();
        assert!(matches!(app.dialog, Some(Dialog::SaveBack(0))));
        assert!(app.save_back_warning(0).is_some_and(|m| m.contains("e.zip")));
    }
}

#[cfg(test)]
mod preview_tests {
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

    /// 選取某一列並等預覽讀完
    fn preview_of<'a>(app: &'a mut App, ctx: &egui::Context, name: &str) -> &'a preview::Content {
        app.sel = [app.rows.iter().position(|r| r.name == name).unwrap()].into();
        for _ in 0..800 {
            app.update_preview(ctx);
            if matches!(app.preview.as_ref().map(|p| &p.state), Some(PreviewState::Ready(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        match &app.preview.as_ref().unwrap().state {
            PreviewState::Ready(c) => c,
            PreviewState::Loading(_) => panic!("預覽逾時"),
        }
    }

    #[test]
    fn previews_inside_archive() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        let src = root.join("src");
        std::fs::create_dir_all(src.join("docs")).unwrap();
        image::RgbImage::from_pixel(40, 30, image::Rgb([0, 120, 200])).save(src.join("docs/pic.png")).unwrap();
        std::fs::write(src.join("docs/note.txt"), "預覽內容").unwrap();
        std::fs::write(src.join("docs/tool.exe"), "MZ").unwrap();
        let ar = root.join("p.zip");
        ziptool_core::compress(&[src.join("docs")], &ar, Format::Zip, &Default::default(), &ziptool_core::NoProgress).unwrap();
        let ctx = egui::Context::default();
        let mut app = App::new(Some(ar.clone()));
        wait(&mut app, &ctx);
        app.navigate(Location::Archive { path: ar, inner: "docs".into() });
        app.preview_open = true;

        assert!(matches!(preview_of(&mut app, &ctx, "pic.png"), preview::Content::Image(_, [40, 30])));
        assert!(matches!(preview_of(&mut app, &ctx, "note.txt"), preview::Content::Text { text, truncated: false } if text == "預覽內容"));
        assert!(matches!(preview_of(&mut app, &ctx, "tool.exe"), preview::Content::Unavailable(m) if m.is_empty()));

        // 名稱含磁碟代號（C:x.txt）的項目：預覽的是實際解出的檔案，而不是 C: 磁碟上的其他檔案
        let odd = root.join("odd.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&odd).unwrap());
        z.start_file("C:x.txt", zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
        std::io::Write::write_all(&mut z, "壓縮檔內的內容".as_bytes()).unwrap();
        z.finish().unwrap();
        assert!(matches!(preview_inside(&odd, "C:x.txt", "", None, Default::default()), preview::Content::Text { text, .. } if text == "壓縮檔內的內容"));

        // 宣告的大小造假（實際超過預覽上限）：寫到上限就停止，不會解出整個檔案
        let liar = root.join("liar.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&liar).unwrap());
        z.start_file("big.txt", zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
        let block = vec![b'a'; 1 << 20];
        for _ in 0..(preview::MAX_ARCHIVE_PREVIEW >> 20) + 1 {
            std::io::Write::write_all(&mut z, &block).unwrap();
        }
        z.finish().unwrap();
        assert!(matches!(preview_inside(&liar, "big.txt", "", None, Default::default()), preview::Content::Unavailable(m) if m.contains("不預覽")));

        // 已取消：不解壓縮，也不留下暫存資料夾（這時沒有其他背景預覽）
        let previews = || -> Vec<std::ffi::OsString> {
            std::fs::read_dir(std::env::temp_dir().join("Zipo"))
                .map(|d| d.flatten().map(|e| e.file_name()).filter(|n| n.to_string_lossy().starts_with(&format!("{}-preview-", std::process::id()))).collect())
                .unwrap_or_default()
        };
        let before = previews();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        assert!(matches!(preview_inside(&odd, "C:x.txt", "", None, done), preview::Content::Unavailable(_)));
        assert!(previews().iter().all(|n| before.contains(n)), "取消後不應留下暫存資料夾");

        // 選取改變（或關閉預覽）時，前一個還在解壓縮的預覽立即停止
        let i = app.rows.iter().position(|r| r.name == "note.txt").unwrap();
        app.sel = [i].into();
        app.update_preview(&ctx);
        let cancel = app.preview.as_ref().unwrap().cancel.clone();
        assert!(!cancel.load(std::sync::atomic::Ordering::Relaxed));
        app.preview = None;
        assert!(cancel.load(std::sync::atomic::Ordering::Relaxed), "前一個預覽應取消");

        // 沒有選取或選取多個：不預覽
        app.sel = [0, 1].into();
        app.update_preview(&ctx);
        assert!(app.preview.is_none());

        // 太大的檔案：不解壓縮
        let i = app.rows.iter().position(|r| r.name == "pic.png").unwrap();
        app.rows[i].size = Some(preview::MAX_ARCHIVE_PREVIEW + 1);
        app.sel = [i].into();
        app.update_preview(&ctx);
        assert!(matches!(&app.preview.as_ref().unwrap().state, PreviewState::Ready(preview::Content::Unavailable(m)) if m.contains("不預覽")));
    }

    #[test]
    fn previews_files_on_disk() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        std::fs::write(root.join("a.md"), "# 標題").unwrap();
        let ctx = egui::Context::default();
        let mut app = App::new(Some(root));
        app.preview_open = true;
        assert!(matches!(preview_of(&mut app, &ctx, "a.md"), preview::Content::Text { text, .. } if text == "# 標題"));
    }
}

#[cfg(test)]
mod nested_tests {
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

    #[test]
    fn browse_archive_inside_archive() {
        let _lock = crate::HASH_INSIDE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        // inner.zip：x/y.txt
        std::fs::create_dir_all(root.join("in/x")).unwrap();
        std::fs::write(root.join("in/x/y.txt"), "abc").unwrap();
        std::fs::create_dir_all(root.join("out/docs")).unwrap();
        let none = &ziptool_core::NoProgress;
        ziptool_core::compress(&[root.join("in/x")], &root.join("out/docs/inner.zip"), Format::Zip, &Default::default(), none).unwrap();
        let outer = root.join("outer.zip");
        ziptool_core::compress(&[root.join("out/docs")], &outer, Format::Zip, &Default::default(), none).unwrap();

        let ctx = egui::Context::default();
        let mut app = App::new(Some(outer.clone()));
        wait(&mut app, &ctx);
        app.navigate(Location::Archive { path: outer.clone(), inner: "docs".into() });
        app.open_row(0);
        wait(&mut app, &ctx);
        assert!(app.in_nested(), "應進入內層壓縮檔：{:?}", app.loc);
        assert_eq!(app.rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["x"]);

        // 網址列：外層 › docs › inner.zip，不顯示暫存資料夾
        let labels: Vec<String> = app.breadcrumbs().into_iter().map(|c| c.0).collect();
        assert_eq!(&labels[labels.len() - 3..], ["outer.zip", "docs", "inner.zip"], "{labels:?}");
        assert!(!labels.iter().any(|l| l == "Zipo"), "{labels:?}");

        // 唯讀：不能加入或刪除；可以讀取內容
        let st = app.toolbar_state();
        assert!(!st.delete && !st.compress && st.extract);
        app.sel = [0].into();
        app.cmd_delete();
        assert!(matches!(&app.dialog, Some(Dialog::Notice { message, .. }) if message == nested_read_only()));
        app.dialog = None;
        app.confirm_add_into_archive(vec![root.join("outer.zip")]);
        assert!(matches!(&app.dialog, Some(Dialog::Notice { message, .. }) if message == nested_read_only()));
        app.dialog = None;
        app.cmd_hash();
        wait(&mut app, &ctx);
        assert!(matches!(&app.dialog, Some(Dialog::Hashes { rows, .. }) if rows[0].1.md5 == "900150983cd24fb0d6963f7d28e17f72"));
        app.dialog = None;

        // 上一層：回到外層壓縮檔的 docs
        app.go_up();
        wait(&mut app, &ctx);
        assert_eq!(app.loc, Location::Archive { path: outer, inner: "docs".into() });
        assert!(!app.in_nested());
    }
}

#[cfg(test)]
mod read_only_tests {
    use super::*;

    /// ISO 與分割的壓縮檔在圖形介面中也是唯讀，並說明原因
    #[test]
    fn iso_and_split_archives_are_read_only() {
        let t = tempfile::tempdir().unwrap();
        let root = real_path(t.path());
        std::fs::write(root.join("a.txt"), "a").unwrap();
        let opts = ziptool_core::CompressOptions { volume_size: Some(1 << 20), ..Default::default() };
        ziptool_core::compress(&[root.join("a.txt")], &root.join("v.zip"), Format::Zip, &opts, &ziptool_core::NoProgress).unwrap();
        let mut app = App::new(Some(root.clone()));
        for (path, word) in [(root.join("v.zip.001"), "分割"), (root.join("disc.iso"), "ISO")] {
            app.loc = Location::Archive { path, inner: String::new() };
            let st = app.toolbar_state();
            assert!(!st.delete && !st.compress, "{word}");
            app.cmd_delete();
            assert!(matches!(&app.dialog, Some(Dialog::Notice { message, .. }) if message.contains(word)), "{word}");
            app.dialog = None;
        }
        // 一般的壓縮檔可以修改
        app.loc = Location::Archive { path: root.join("x.zip"), inner: String::new() };
        assert!(app.read_only_reason().is_none());
    }
}

#[cfg(test)]
mod english_tests {
    use super::*;
    use ziptool_core::i18n::with_language;

    fn has_chinese(s: &str) -> bool {
        s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3000}'..='\u{303f}').contains(&c) || ('\u{ff00}'..='\u{ffef}').contains(&c))
    }

    /// 從舊版升級（已有設定、沒有語言設定）維持中文；全新安裝依系統；明確選擇的語言照用
    #[test]
    fn language_default_keeps_existing_users_in_chinese() {
        #[derive(Default)]
        struct Mem(std::collections::HashMap<String, String>);
        impl eframe::Storage for Mem {
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
        let mut old = Mem::default();
        eframe::Storage::set_string(&mut old, "sort_key", "name".into());
        assert_eq!(App::with_storage(None, Some(&old)).language, Language::Chinese);
        assert_eq!(App::with_storage(None, Some(&Mem::default())).language, Language::Auto);
        eframe::Storage::set_string(&mut old, "language", "en".into());
        assert_eq!(App::with_storage(None, Some(&old)).language, Language::English);
    }

    /// 英文介面：訊息、對話框文字與核心程式庫的錯誤都不含中文
    #[test]
    fn english_mode_has_no_chinese() {
        with_language(true, || {
            let t = tempfile::tempdir().unwrap();
            let mut app = App::new(Some(real_path(t.path())));
            app.cmd_delete();
            let Some(Dialog::Notice { title, message, .. }) = &app.dialog else { panic!() };
            assert!(!has_chinese(title) && !has_chinese(message), "{title} {message}");
            assert_eq!(Location::Computer.display(), "Computer");
            for e in [
                ziptool_core::Error::PasswordRequired,
                ziptool_core::Error::WrongPassword,
                ziptool_core::Error::Cancelled,
                ziptool_core::Error::NotEnoughSpace { need: 10, available: 1 },
                ziptool_core::Error::PossibleBomb { size: 1 << 31, packed: 1 },
                ziptool_core::Error::TooLarge { limit: 1 << 20 },
                ziptool_core::Error::SevenZ("x".into()),
            ] {
                assert!(!has_chinese(&e.to_string()), "{e}");
            }
            let bad = ziptool_core::list(&t.path().join("沒有.zip"), None).unwrap_err().to_string();
            assert!(!bad.contains("找不到"), "{bad}");
            assert_eq!(dialogs::LEVELS.map(|l| ziptool_core::i18n::pick(l.1, l.2))[3], "Normal");
        });
        // 離開後回到中文
        assert_eq!(Location::Computer.display(), "電腦");
    }
}
