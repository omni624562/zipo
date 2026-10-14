//! Zipo 命令列：採用 7-Zip（7z.exe）的指令語法。
//!
//!   zipo <指令> [<參數>...] <壓縮檔> [<檔案名稱>...]

use ziptool_core::{F, L};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Instant, SystemTime};

use indicatif::{ProgressBar, ProgressStyle};
use ziptool_core::{CompressOptions, Error, ExtractOptions, Filter, Format, Overwrite, OverwriteAnswer, Pattern, Progress, format_size};

const HELP_ZH: &str = "\
用法：zipo <指令> [<參數>...] <壓縮檔> [<檔案名稱>...]

<指令>
  a : 加入到壓縮檔（壓縮檔已存在時加入或取代同名項目）
  d : 從壓縮檔刪除項目
  e : 解壓縮，不含資料夾結構
  h : 計算檔案的雜湊值（CRC32、MD5、SHA-1、SHA-256），資料夾則計算其中每個檔案
  l : 列出內容
  rn : 重新命名壓縮檔內的項目：依序寫出舊名稱、新名稱（可以多組；資料夾連同其中的內容）
  t : 測試完整性
  u : 更新壓縮檔（同 a）
  x : 解壓縮，保留完整路徑
  i : 顯示支援的格式

<參數>
  --       : 之後的引數一律視為檔案名稱（使用 * 等萬用字元時建議加上，
             避免以 - 開頭的檔名被當成參數，例如名為 -p123 的檔案變成密碼）
  -ao{a|s|u} : 同名檔處理：a 全部覆寫、s 略過、u 自動改名（未指定時逐一詢問）
  -bd      : 不顯示進度條
  -i[r]!{名稱} : 只處理符合的項目，可用 * 與 ? 萬用字元；加 r 為每一層都比對，否則只比對最上層
  -x[r]!{名稱} : 略過符合的項目（資料夾連同其中的內容），寫法同 -i
  -m{參數} : 壓縮方法參數，支援 -mx{0-9}（或 -mx={0-9}）、-mhe={on|off}、
             -mcp={字碼頁}（舊式 zip 的檔名編碼，例如 950＝Big5、936＝GBK；預設依系統）
  -o{資料夾} : 輸出資料夾
  -p{密碼} : 設定密碼；只寫 -p 則以互動方式輸入（建議：寫在命令列的密碼
             會被其他程式看到，也會留在命令歷程中）
  -r       : 檔案名稱中的萬用字元（例如 *.txt）也搜尋子資料夾；資料夾一律包含其中的內容
  -v{大小} : 分割成多個檔案，例如 -v100m、-v4g（產生 名稱.001、名稱.002…）；
             解壓縮時指定 名稱.001 即可
  -t{格式} : 壓縮檔格式：7z、zip、tar、tar.gz、tar.bz2、tar.xz、tar.zst、gzip、bzip2、xz、zstd
  -y       : 所有詢問都回答「是」（不互動詢問）

範例
  zipo a backup.7z 文件 照片 -mx9 -p -mhe=on
  zipo x backup.7z -oD:\\還原 -aou
  zipo e logs.tar.zst -o平面 logs/app.log logs/err.log
  zipo a backup.7z 新增的檔案.txt
  zipo d backup.7z 文件/舊資料
  zipo a 專案.7z 專案 -xr!node_modules -xr!*.log
  zipo x backup.7z *.jpg -r -o照片
  zipo rn backup.7z 文件/舊.txt 文件/新.txt
  zipo l backup.7z
  zipo t backup.7z -p密碼
";

const HELP_EN: &str = "\
Usage: zipo <command> [<switches>...] <archive> [<file names>...]

<Commands>
  a : Add files to archive (adds or replaces items when the archive exists)
  d : Delete items from archive
  e : Extract files without folder structure
  h : Calculate file hashes (CRC32, MD5, SHA-1, SHA-256); for folders, every file inside
  l : List contents
  rn : Rename items in archive: old name, then new name (several pairs allowed; folders with their contents)
  t : Test integrity
  u : Update archive (same as a)
  x : Extract files with full paths
  i : Show supported formats

<Switches>
  --       : Treat all following arguments as file names (recommended with wildcards
             such as *, so a file named like -p123 is not taken as a switch)
  -ao{a|s|u} : Existing files: a overwrite all, s skip, u auto rename (ask if not set)
  -bd      : Hide the progress bar
  -i[r]!{name} : Only process matching items; * and ? wildcards allowed; with r every level is matched,
             otherwise only the top level
  -x[r]!{name} : Skip matching items (folders with their contents); same syntax as -i
  -m{param} : Compression parameters: -mx{0-9} (or -mx={0-9}), -mhe={on|off},
             -mcp={codepage} (file name encoding of old zips, e.g. 950 = Big5, 936 = GBK;
             system default if not set)
  -o{folder} : Output folder
  -p{password} : Set the password; -p alone asks for it interactively (recommended:
             passwords on the command line are visible to other programs and kept in history)
  -r       : Wildcards in file names (such as *.txt) also search subfolders; folders always include their contents
  -v{size} : Split into volumes, e.g. -v100m, -v4g (creates name.001, name.002…);
             extract by giving name.001
  -t{format} : Archive format: 7z, zip, tar, tar.gz, tar.bz2, tar.xz, tar.zst, gzip, bzip2, xz, zstd
  -y       : Answer yes to all questions (non-interactive)

Examples
  zipo a backup.7z Documents Photos -mx9 -p -mhe=on
  zipo x backup.7z -oD:\\Restore -aou
  zipo e logs.tar.zst -oflat logs/app.log logs/err.log
  zipo a backup.7z new-file.txt
  zipo d backup.7z Documents/old
  zipo a project.7z project -xr!node_modules -xr!*.log
  zipo x backup.7z *.jpg -r -oPhotos
  zipo rn backup.7z Documents/old.txt Documents/new.txt
  zipo l backup.7z
  zipo t backup.7z -pPassword
";

fn help() -> &'static str {
    ziptool_core::i18n::pick(HELP_ZH, HELP_EN)
}

/// 7-Zip 的結束代碼
mod exit {
    pub const OK: u8 = 0;
    /// 完成，但有項目被略過
    pub const WARNING: u8 = 1;
    pub const FATAL: u8 = 2;
    pub const USAGE: u8 = 7;
}

#[derive(Debug, PartialEq)]
enum Cmd {
    Add,
    Delete,
    Extract,
    ExtractFlat,
    List,
    Test,
    Info,
    Hash,
    Rename,
}

#[derive(Debug, Default)]
struct Args {
    cmd: Option<Cmd>,
    archive: Option<PathBuf>,
    files: Vec<String>,
    out: Option<PathBuf>,
    /// `Some("")` 代表只寫了 -p，需互動輸入
    password: Option<String>,
    format: Option<Format>,
    level: Option<u32>,
    encrypt_names: bool,
    /// 舊式 zip 檔名的字碼頁（-mcp）
    codepage: Option<u32>,
    /// `None`：加了 -y 就覆寫，否則逐一詢問
    overwrite: Option<Overwrite>,
    yes: bool,
    no_progress: bool,
    /// 分割大小（-v）
    volume: Option<u64>,
    /// -i／-x 的篩選
    filter: Filter,
    /// -r：檔案名稱中的萬用字元也搜尋子資料夾
    recurse: bool,
}

fn parse_format(s: &str) -> Option<Format> {
    let s = s.to_ascii_lowercase();
    let alias = match s.as_str() {
        "gzip" => "gz",
        "bzip2" => "bz2",
        other => other,
    };
    Format::from_name(alias)
}

fn parse(argv: Vec<String>) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = argv.into_iter();
    let cmd = it.next().ok_or(L!("缺少指令", "Missing command"))?;
    a.cmd = Some(match cmd.to_ascii_lowercase().as_str() {
        "a" | "u" => Cmd::Add,
        "d" => Cmd::Delete,
        "x" => Cmd::Extract,
        "e" => Cmd::ExtractFlat,
        "l" => Cmd::List,
        "t" => Cmd::Test,
        "i" => Cmd::Info,
        "h" => Cmd::Hash,
        "rn" => Cmd::Rename,
        other => return Err(F!("不支援的指令：{other}", "Unsupported command: {other}")),
    });
    let mut only_files = false;
    for arg in it {
        if only_files || !arg.starts_with('-') || arg == "-" {
            if a.archive.is_none() {
                a.archive = Some(PathBuf::from(arg));
            } else {
                a.files.push(arg);
            }
            continue;
        }
        let sw = &arg[1..];
        let lower = sw.to_ascii_lowercase();
        match lower.as_str() {
            "-" => only_files = true,
            "y" => a.yes = true,
            "r" => a.recurse = true,
            "bd" => a.no_progress = true,
            "aoa" => a.overwrite = Some(Overwrite::Overwrite),
            "aos" => a.overwrite = Some(Overwrite::Skip),
            "aou" => a.overwrite = Some(Overwrite::Rename),
            "mhe" | "mhe=on" | "mhe+" => a.encrypt_names = true,
            "mhe=off" | "mhe-" => a.encrypt_names = false,
            _ if lower.starts_with("mcp") => {
                let v = lower[3..].trim_start_matches('=');
                a.codepage = Some(v.parse().map_err(|_| F!("無效的字碼頁：{arg}", "Invalid code page: {arg}"))?);
            }
            _ if lower.starts_with("mx") => {
                let v = lower[2..].trim_start_matches('=');
                let lv: u32 = v.parse().map_err(|_| F!("無效的壓縮等級：{arg}", "Invalid compression level: {arg}"))?;
                if lv > 9 {
                    return Err(F!("壓縮等級須為 0–9：{arg}", "Compression level must be 0–9: {arg}"));
                }
                a.level = Some(lv);
            }
            _ if lower.starts_with('v') => {
                a.volume = Some(ziptool_core::parse_volume_size(&sw[1..]).ok_or_else(|| F!("無效的分割大小：{arg}", "Invalid volume size: {arg}"))?);
            }
            // 保留大小寫：路徑與密碼
            _ if lower.starts_with('o') => a.out = Some(PathBuf::from(&sw[1..])),
            _ if lower.starts_with('p') => a.password = Some(sw[1..].to_string()),
            _ if lower.starts_with('t') => {
                a.format = Some(parse_format(&sw[1..]).ok_or_else(|| F!("不支援的格式：{}", "Unsupported format: {}", &sw[1..]))?)
            }
            // -x!名稱、-xr!名稱、-i!名稱、-ir!名稱
            _ if lower.starts_with('x') || lower.starts_with('i') => {
                let bad = || F!("不支援的參數：{arg}（寫法為 -x!名稱 或 -xr!名稱）", "Unsupported switch: {arg} (use -x!name or -xr!name)");
                let (head, pat) = sw.split_once('!').ok_or_else(bad)?;
                let recursive = match &lower[1..head.len()] {
                    "" => false,
                    "r" => true,
                    _ => return Err(bad()),
                };
                if pat.is_empty() {
                    return Err(bad());
                }
                let p = Pattern::new(pat, recursive);
                if lower.starts_with('x') { a.filter.exclude.push(p) } else { a.filter.include.push(p) }
            }
            _ => return Err(F!("不支援的參數：{arg}", "Unsupported switch: {arg}")),
        }
    }
    if a.cmd == Some(Cmd::Rename) && (a.files.is_empty() || a.files.len() % 2 != 0) {
        return Err(L!("rn 指令需要成對的舊名稱與新名稱", "The rn command needs pairs of old and new names").into());
    }
    Ok(a)
}

// ---------- 進度條 ----------

struct Bar(ProgressBar);

impl Bar {
    fn new(hidden: bool) -> Self {
        let pb = if hidden { ProgressBar::hidden() } else { ProgressBar::new(0) };
        pb.set_style(
            ProgressStyle::with_template("{percent:>3}% {bytes:>10} / {total_bytes:<10} {binary_bytes_per_sec:>12}  {wide_msg}")
                .unwrap(),
        );
        Bar(pb)
    }
}

impl Progress for Bar {
    fn start(&self, total: u64) {
        self.0.set_length(total);
        self.0.set_position(0);
    }
    fn advance(&self, n: u64) {
        self.0.inc(n);
    }
    fn entry(&self, name: &str) {
        self.0.set_message(clean(name));
    }
    /// 與 7z 相同的同名檔詢問；輸入結束（非互動執行）時視為「全部略過」，不會誤刪資料。
    fn ask_overwrite(&self, path: &Path) -> OverwriteAnswer {
        self.0.suspend(|| {
            loop {
                eprint!(
                    "{}", F!("\n檔案已存在：{}\n要取代嗎？(Y)是 / (N)否 / (A)全部是 / (S)全部略過 / (U)全部自動改名 / (Q)中止：", "\nFile already exists: {}\nReplace it? (Y)es / (N)o / (A)lways / (S)kip all / a(U)to rename all / (Q)uit: ",
                    clean(&path.display().to_string())
                ));
                let mut line = String::new();
                if matches!(std::io::stdin().read_line(&mut line), Ok(0) | Err(_)) {
                    return OverwriteAnswer::NoAll;
                }
                return match line.trim().to_ascii_lowercase().as_str() {
                    "y" => OverwriteAnswer::Yes,
                    "n" => OverwriteAnswer::No,
                    "a" => OverwriteAnswer::YesAll,
                    "s" => OverwriteAnswer::NoAll,
                    "u" => OverwriteAnswer::RenameAll,
                    "q" => OverwriteAnswer::Cancel,
                    _ => continue,
                };
            }
        })
    }
}

/// 有項目被略過時回傳的錯誤，對應結束代碼 1。
#[derive(Debug)]
struct Warnings(usize);

impl std::fmt::Display for Warnings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", F!("有 {} 個項目被略過", "{} item(s) were skipped", self.0))
    }
}

impl std::error::Error for Warnings {}

/// 壓縮檔內的名稱可能含有終端機控制字元（清除畫面、移動游標、改視窗標題）或反轉文字方向的字元，
/// 惡意檔案可藉此偽造輸出或蓋掉警告。輸出前一律轉成看得見的跳脫形式，例如 `\u{1b}`。
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| {
            let bidi = matches!(c, '\u{200E}' | '\u{200F}' | '\u{061C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}');
            if c.is_control() || bidi { c.escape_unicode().to_string() } else { c.to_string() }
        })
        .collect()
}

/// 可能含有名稱的多行訊息（例如錯誤說明）：保留換行，其餘控制字元跳脫
fn clean_lines(s: &str) -> String {
    s.split('\n').map(clean).collect::<Vec<_>>().join("\n")
}

fn confirm(question: &str) -> bool {
    eprint!("{}", F!("{question} (y/N)：", "{question} (y/N): "));
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok() && line.trim().eq_ignore_ascii_case("y")
}

// ---------- 共用 ----------

fn prompt_password(confirm: bool) -> anyhow::Result<String> {
    let pw = rpassword::prompt_password(L!("輸入密碼：", "Enter password: "))?;
    // 建立加密壓縮檔時不接受空白密碼（等於沒有加密）
    if confirm && pw.is_empty() {
        anyhow::bail!("{}", F!("密碼不可空白", "The password cannot be empty"));
    }
    if confirm && rpassword::prompt_password(L!("再次輸入密碼：", "Re-enter password: "))? != pw {
        anyhow::bail!("{}", F!("兩次輸入的密碼不一致", "The passwords do not match"));
    }
    Ok(pw)
}

fn interactive(a: &Args) -> bool {
    !a.yes && std::io::stdin().is_terminal()
}

/// 執行需要密碼的動作；遇到加密或密碼錯誤時，在互動模式下詢問並重試。
fn with_password<T>(a: &Args, mut f: impl FnMut(Option<&str>) -> ziptool_core::Result<T>) -> anyhow::Result<T> {
    let mut pw = match &a.password {
        Some(p) if p.is_empty() => Some(prompt_password(false)?),
        other => other.clone(),
    };
    loop {
        match f(pw.as_deref()) {
            Err(e @ (Error::PasswordRequired | Error::WrongPassword)) if interactive(a) => {
                eprintln!("{e}");
                let p = prompt_password(false)?;
                if p.is_empty() {
                    return Err(e.into());
                }
                pw = Some(p);
            }
            r => return Ok(r?),
        }
    }
}

fn format_time(t: Option<SystemTime>) -> String {
    let Some(d) = t.and_then(ziptool_core::local_time) else { return " ".repeat(19) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        d.year(),
        u8::from(d.month()),
        d.day(),
        d.hour(),
        d.minute(),
        d.second()
    )
}

fn archive_path(a: &Args) -> anyhow::Result<&Path> {
    a.archive.as_deref().ok_or_else(|| anyhow::anyhow!("{}", F!("缺少壓縮檔名稱", "Missing archive name")))
}

// ---------- 指令 ----------

fn cmd_add(a: &Args) -> anyhow::Result<()> {
    let mut archive = archive_path(a)?.to_path_buf();
    let name = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // 與 7-Zip 相同：未指定格式時用 7z；壓縮檔沒有副檔名時自動補上
    let format = match (a.format, Format::from_file_name(&name)) {
        (Some(f), _) | (None, Some(f)) => f,
        (None, None) => Format::SevenZ,
    };
    if Format::from_file_name(&name).is_none() {
        archive.set_file_name(format!("{name}.{}", format.extension()));
    }

    let mut filter = a.filter.clone();
    let inputs: Vec<PathBuf> = if a.files.is_empty() {
        // 與 7-Zip 相同：未指定檔案時加入目前資料夾的所有內容
        let me = std::fs::canonicalize(&archive).ok();
        std::fs::read_dir(".")?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| std::fs::canonicalize(p).ok() != me)
            .collect()
    } else {
        let mut v = Vec::new();
        for f in &a.files {
            if !f.contains(['*', '?']) {
                v.push(PathBuf::from(f));
                continue;
            }
            // 萬用字元：與 7-Zip 相同，比對資料夾中的名稱；加 -r 時每一層都比對（保留資料夾結構）
            let p = Path::new(f);
            let dir = p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
            let pat = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let me = std::fs::canonicalize(&archive).ok();
            let only = Filter { include: vec![Pattern::new(&pat, false)], ..Default::default() };
            for e in std::fs::read_dir(dir)?.filter_map(|e| e.ok()) {
                if std::fs::canonicalize(e.path()).ok() == me {
                    continue;
                }
                if a.recurse || only.allows(&e.file_name().to_string_lossy()) {
                    v.push(e.path());
                }
            }
            if a.recurse {
                filter.include.push(Pattern::new(&pat, true));
            }
        }
        if v.is_empty() {
            anyhow::bail!("{}", F!("沒有符合的檔案：{}", "No matching files: {}", a.files.join(" ")));
        }
        v
    };

    let password = match &a.password {
        Some(p) if p.is_empty() => Some(prompt_password(true)?),
        other => other.clone(),
    };
    let opts = CompressOptions { level: a.level, password, encrypt_names: a.encrypt_names, volume_size: a.volume, filter };

    let bar = Bar::new(a.no_progress);
    if archive.exists() && a.volume.is_some() {
        anyhow::bail!("{}", F!("壓縮檔已存在，無法以分割方式更新：{}", "The archive already exists and cannot be updated as split volumes: {}", archive.display()));
    }
    if archive.exists() {
        // 與 7-Zip 相同：壓縮檔已存在時加入（同名項目取代），原檔在全部完成後才被替換
        println!("{}", F!("正在更新壓縮檔：{}", "Updating archive: {}", archive.display()));
        ziptool_core::update(&archive, &inputs, "", &[], &opts, &bar)?;
    } else {
        println!("{}", F!("正在建立壓縮檔：{}（{}）", "Creating archive: {} ({})", archive.display(), format.name()));
        ziptool_core::compress(&inputs, &archive, format, &opts, &bar)?;
    }
    bar.0.finish_and_clear();
    if a.volume.is_some() {
        // 分割後原檔已不存在，列出各個分割檔
        let parts: Vec<PathBuf> = (1..).map(|n| PathBuf::from(format!("{}.{n:03}", archive.display()))).take_while(|p| p.is_file()).collect();
        let total: u64 = parts.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
        println!("{}", F!("已分割為 {} 個檔案（{}）：{} …\n一切正常", "Split into {} files ({}): {} …\nEverything is OK", parts.len(), format_size(total), parts[0].display()));
        return Ok(());
    }
    let size = std::fs::metadata(&archive)?.len();
    println!("{}", F!("壓縮檔大小：{}\n一切正常", "Archive size: {}\nEverything is OK", format_size(size)));
    Ok(())
}

fn cmd_delete(a: &Args) -> anyhow::Result<()> {
    let archive = archive_path(a)?;
    if a.files.is_empty() {
        anyhow::bail!("{}", F!("請指定要刪除的項目（壓縮檔內的完整路徑）", "Specify the items to delete (full paths inside the archive)"));
    }
    let names = ziptool_core::list(archive, a.password.as_deref().filter(|p| !p.is_empty()))?;
    for f in &a.files {
        let f = f.trim_matches('/');
        if !names.iter().any(|e| e.name == f || e.name.starts_with(&format!("{f}/"))) {
            anyhow::bail!("{}", F!("壓縮檔中沒有：{f}", "Not in the archive: {f}"));
        }
    }
    let password = match &a.password {
        Some(p) if p.is_empty() => Some(prompt_password(false)?),
        other => other.clone(),
    };
    println!("{}", F!("正在從 {} 刪除 {} 個項目", "Deleting {1} item(s) from {0}", archive.display(), a.files.len()));
    let bar = Bar::new(a.no_progress);
    let opts = CompressOptions { level: a.level, password, encrypt_names: false, volume_size: None, filter: Filter::default() };
    ziptool_core::update(archive, &[], "", &a.files, &opts, &bar)?;
    bar.0.finish_and_clear();
    println!("{}", F!("一切正常", "Everything is OK"));
    Ok(())
}

/// 解壓縮與列出時，指定的檔案名稱與 -i 相同（可用萬用字元；加 -r 時每一層都比對）
fn name_filter(a: &Args) -> Filter {
    let mut f = a.filter.clone();
    f.include.extend(a.files.iter().map(|n| Pattern::new(n, a.recurse)));
    f
}

/// 與 7z 的 rn 指令相同：成對的舊名稱、新名稱
fn cmd_rename(a: &Args) -> anyhow::Result<()> {
    let archive = archive_path(a)?;
    let password = match &a.password {
        Some(p) if p.is_empty() => Some(prompt_password(false)?),
        other => other.clone(),
    };
    println!("{}", F!("正在重新命名 {} 中的 {} 個項目", "Renaming {1} item(s) in {0}", archive.display(), a.files.len() / 2));
    let bar = Bar::new(a.no_progress);
    let opts = CompressOptions { level: a.level, password, ..Default::default() };
    for [from, to] in a.files.as_chunks::<2>().0 {
        ziptool_core::rename(archive, from, to, &opts, &bar)?;
    }
    bar.0.finish_and_clear();
    println!("{}", F!("一切正常", "Everything is OK"));
    Ok(())
}

fn cmd_extract(a: &Args, flatten: bool) -> anyhow::Result<()> {
    let archive = archive_path(a)?;
    let dest = a.out.clone().unwrap_or_else(|| PathBuf::from("."));
    println!("{}", F!("正在解壓縮：{}", "Extracting: {}", archive.display()));
    let started = Instant::now();
    let overwrite = a.overwrite.unwrap_or(if a.yes { Overwrite::Overwrite } else { Overwrite::Ask });
    // -y 代表使用者已同意所有詢問，包含高壓縮比的警告
    let mut allow_high_ratio = a.yes;
    let report = loop {
        let bar = Bar::new(a.no_progress);
        let r = with_password(a, |pw| {
            let opts = ExtractOptions {
                password: pw.map(String::from),
                overwrite,
                selection: None,
                strip_prefix: String::new(),
                flatten,
                allow_high_ratio,
                max_bytes: None,
                filter: name_filter(a),
            };
            ziptool_core::extract(archive, &dest, &opts, &bar)
        });
        bar.0.finish_and_clear();
        match r {
            Err(e) if !allow_high_ratio && interactive(a) && matches!(e.downcast_ref::<Error>(), Some(Error::PossibleBomb { .. })) => {
                eprintln!("{}", F!("警告：{e}", "Warning: {e}"));
                if !confirm(L!("確定要繼續解壓縮嗎？", "Continue extracting?")) {
                    return Err(e);
                }
                allow_high_ratio = true;
            }
            r => break r?,
        }
    };
    for i in &report.issues {
        eprintln!("{}", F!("警告：{}：{}", "Warning: {}: {}", clean(&i.name), clean_lines(&i.reason)));
    }
    println!(
        "{}", F!("已解壓縮到：{}（{} 個檔案，{}，{:.1} 秒）", "Extracted to: {} ({} files, {}, {:.1} s)",
        dest.display(),
        report.files,
        format_size(report.bytes),
        started.elapsed().as_secs_f32()
    ));
    if !report.issues.is_empty() {
        return Err(Warnings(report.issues.len()).into());
    }
    println!("{}", F!("一切正常", "Everything is OK"));
    Ok(())
}

fn cmd_test(a: &Args) -> anyhow::Result<()> {
    let archive = archive_path(a)?;
    println!("{}", F!("正在測試：{}", "Testing: {}", archive.display()));
    let bar = Bar::new(a.no_progress);
    let report = with_password(a, |pw| ziptool_core::test(archive, pw, &bar))?;
    bar.0.finish_and_clear();
    println!("{}", F!("檔案：{}\n大小：{}\n一切正常", "Files: {}\nSize: {}\nEverything is OK", report.files, format_size(report.bytes)));
    Ok(())
}

/// 與 7z 的 h 指令相同：所有引數都是要計算的檔案或資料夾
fn cmd_hash(a: &Args) -> anyhow::Result<()> {
    let inputs: Vec<PathBuf> = a.archive.iter().cloned().chain(a.files.iter().map(PathBuf::from)).collect();
    if inputs.is_empty() {
        anyhow::bail!("{}", F!("缺少檔案名稱", "Missing file name"));
    }
    let bar = Bar::new(a.no_progress);
    let results = ziptool_core::hash_files(&inputs, &bar)?;
    bar.0.finish_and_clear();
    for (name, h) in &results {
        println!("{}", F!("{}（{}）", "{} ({})", clean(name), format_size(h.size)));
        for (algo, value) in h.all() {
            println!("  {algo:<8}{value}");
        }
        println!();
    }
    println!("{}", F!("{} 個檔案", "{} file(s)", results.len()));
    Ok(())
}

fn cmd_list(a: &Args) -> anyhow::Result<()> {
    let archive = archive_path(a)?;
    let filter = name_filter(a);
    let mut entries = with_password(a, |pw| ziptool_core::list(archive, pw))?;
    entries.retain(|e| filter.allows(&e.name));
    // 不是壓縮檔的分割檔（影片.mkv.001）：接回的就是原檔
    let format = match Format::detect(archive) {
        Ok(f) => f.name(),
        Err(_) if ziptool_core::is_first_volume(archive) => L!("分割檔", "Split file"),
        Err(e) => return Err(e.into()),
    };
    println!("{}", F!("壓縮檔：{}\n格式：{}\n", "Archive: {}\nFormat: {}\n", archive.display(), format));

    let rule = "------------------- ----- ------------ ------------  ------------------------";
    println!("{}", F!("       日期時間       屬性         大小     壓縮後大小  名稱", "   Date      Time    Attr         Size   Compressed  Name"));
    println!("{rule}");
    let (mut files, mut dirs, mut size, mut packed) = (0u64, 0u64, 0u64, 0u64);
    let mut any_encrypted = false;
    let num = |v: Option<u64>| v.map(|n| n.to_string()).unwrap_or_default();
    for e in &entries {
        let attr = if e.is_dir { "D...." } else { "....A" };
        let mark = if e.encrypted { "*" } else { "" };
        any_encrypted |= e.encrypted;
        println!(
            "{} {attr} {:>12} {:>12}  {}{mark}",
            format_time(e.modified),
            if e.is_dir { String::new() } else { num(e.size) },
            if e.is_dir { String::new() } else { num(e.compressed_size) },
            clean(&e.name)
        );
        if e.is_dir {
            dirs += 1;
        } else {
            files += 1;
            size += e.size.unwrap_or(0);
            packed += e.compressed_size.unwrap_or(0);
        }
    }
    println!("{rule}");
    let packed = if entries.iter().any(|e| e.compressed_size.is_some()) { packed.to_string() } else { String::new() };
    println!("{}", F!("{:19} {:5} {size:>12} {packed:>12}  {files} 個檔案，{dirs} 個資料夾", "{:19} {:5} {size:>12} {packed:>12}  {files} files, {dirs} folders", "", ""));
    if any_encrypted {
        println!("{}", F!("\n* 表示已加密", "\n* encrypted"));
    }
    Ok(())
}

fn cmd_info() {
    println!("{}", F!("格式        副檔名                     壓縮  解壓縮  密碼", "Format      Extensions                 Pack  Unpack  Password"));
    for f in Format::ALL {
        let exts = match f {
            Format::TarGz => "tar.gz tgz",
            Format::TarBz2 => "tar.bz2 tbz2 tbz",
            Format::TarXz => "tar.xz txz",
            Format::TarZst => "tar.zst tzst",
            Format::Zst => "zst zstd",
            other => other.extension(),
        };
        let pw = if f.supports_password() { "AES-256" } else { "" };
        println!("{}", F!("{:<11} {:<26} 是    是      {pw}", "{:<11} {:<26} yes   yes     {pw}", f.name(), exts));
    }
    println!("{}", F!("{:<11} {:<26} 否    是      （ISO 9660、Joliet；不支援只用 UDF 的映像檔）", "{:<11} {:<26} no    yes     (ISO 9660, Joliet; UDF-only images are not supported)", "iso", "iso"));
}

fn main() -> ExitCode {
    let english = match std::env::var("ZIPO_LANG").as_deref() {
        Ok("zh") => false,
        Ok("en") => true,
        _ => ziptool_core::i18n::system_prefers_english(),
    };
    ziptool_core::i18n::set_english(english);
    println!("{}", F!("Zipo {} ：以 7-Zip 為藍本的壓縮工具\n", "Zipo {} : an archiver modeled on 7-Zip\n", env!("CARGO_PKG_VERSION")));
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // 只寫 h 時顯示說明；h 後面接檔案時是計算雜湊值（與 7z 相同）
    if argv.is_empty() || matches!(argv[0].as_str(), "-h" | "--help" | "/?") || argv == ["h"] {
        print!("{}", help());
        return ExitCode::from(exit::OK);
    }
    let args = match parse(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{}", F!("命令列錯誤：{e}\n\n{}", "Command line error: {e}\n\n{}", help()));
            return ExitCode::from(exit::USAGE);
        }
    };
    ziptool_core::set_zip_codepage(args.codepage);
    let res = match args.cmd {
        Some(Cmd::Add) => cmd_add(&args),
        Some(Cmd::Delete) => cmd_delete(&args),
        Some(Cmd::Extract) => cmd_extract(&args, false),
        Some(Cmd::ExtractFlat) => cmd_extract(&args, true),
        Some(Cmd::List) => cmd_list(&args),
        Some(Cmd::Test) => cmd_test(&args),
        Some(Cmd::Hash) => cmd_hash(&args),
        Some(Cmd::Rename) => cmd_rename(&args),
        Some(Cmd::Info) | None => {
            cmd_info();
            Ok(())
        }
    };
    match res {
        Ok(()) => ExitCode::from(exit::OK),
        Err(e) if e.downcast_ref::<Warnings>().is_some() => {
            eprintln!("{}", clean_lines(&e.to_string()));
            ExitCode::from(exit::WARNING)
        }
        Err(e) => {
            eprintln!("{}", F!("錯誤：{}", "Error: {}", clean_lines(&format!("{e:#}"))));
            ExitCode::from(exit::FATAL)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Args {
        parse(s.split_whitespace().map(String::from).collect()).unwrap()
    }

    #[test]
    fn parses_7z_style_switches() {
        let a = p("a out.7z docs pics -mx=9 -pSecret -mhe=on -tzip -y -bd");
        assert_eq!(a.cmd, Some(Cmd::Add));
        assert_eq!(a.archive, Some(PathBuf::from("out.7z")));
        assert_eq!(a.files, ["docs", "pics"]);
        assert_eq!(a.level, Some(9));
        assert_eq!(a.password.as_deref(), Some("Secret"));
        assert!(a.encrypt_names && a.yes && a.no_progress);
        assert_eq!(a.format, Some(Format::Zip));

        let a = p("x a.zip -oD:\\Out -aou -p -mx3");
        assert_eq!(a.out, Some(PathBuf::from("D:\\Out")));
        assert_eq!(a.overwrite, Some(Overwrite::Rename));
        assert_eq!(a.password.as_deref(), Some(""));
        assert_eq!(a.level, Some(3));

        assert_eq!(p("a x -tgzip").format, Some(Format::Gz));
        assert_eq!(p("a x -ttar.zst").format, Some(Format::TarZst));
        assert_eq!(p("x a.7z -- -weird").files, ["-weird"]);
        assert_eq!(p("x a.zip -mcp=950").codepage, Some(950));
        assert_eq!(p("x a.zip -mcp936").codepage, Some(936));
        assert!(parse(vec!["x".into(), "a.zip".into(), "-mcp=big5".into()]).is_err());
        assert!(parse(vec!["q".into()]).is_err());
        assert!(parse(vec!["a".into(), "-mx12".into()]).is_err());
    }
}
