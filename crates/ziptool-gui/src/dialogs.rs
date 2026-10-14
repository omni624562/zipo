//! 對話框：壓縮、解壓縮、密碼。版面與選項參考 7-Zip。

use ziptool_core::{F, L};
use std::path::{Path, PathBuf};

use eframe::egui::{self, RichText};
use egui_phosphor::regular as icon;
use ziptool_core::{CompressOptions, ExtractOptions, Format, Overwrite};

use crate::jobs::{Task, Then};
use ziptool_core::i18n::pick;
use crate::theme::pal;
use crate::widgets::*;
use crate::{file_name, pick_save, pick_folder};

pub enum Outcome<T> {
    Pending,
    Cancel,
    Ok(T),
}

/// 7-Zip 的壓縮等級名稱
pub const LEVELS: [(u32, &str, &str); 6] =
    [(0, "儲存", "Store"), (1, "最快", "Fastest"), (3, "快速", "Fast"), (5, "標準", "Normal"), (7, "最大", "Maximum"), (9, "極限", "Ultra")];

/// 分割大小的選項（顯示文字, 位元組）；最後一項為自訂
const VOLUMES: [(&str, &str, u64); 5] = [
    ("不分割", "No split", 0),
    ("100 MB", "100 MB", 100 << 20),
    ("700 MB（CD）", "700 MB (CD)", 700 << 20),
    ("4 GB（FAT32 隨身碟）", "4 GB (FAT32 USB drive)", (4 << 30) - (1 << 20)),
    ("自訂…", "Custom…", 0),
];

/// 自訂分割大小的單位（顯示文字, 位元組）
const VOLUME_UNITS: [(&str, u64); 3] = [("KB", 1 << 10), ("MB", 1 << 20), ("GB", 1 << 30)];

pub struct AddDialog {
    inputs: Vec<PathBuf>,
    archive: String,
    format: Format,
    level: usize,
    password: String,
    password2: String,
    show_password: bool,
    /// 勾選「設定密碼」才顯示密碼欄並加密
    use_password: bool,
    encrypt_names: bool,
    /// 壓縮後將原始檔移到資源回收筒
    recycle: bool,
    /// 分割大小：`VOLUMES` 的索引；自訂時為 `volume_mb`（單位為 `VOLUME_UNITS[volume_unit]`）
    volume: usize,
    volume_mb: String,
    volume_unit: usize,
    pub open_folder: bool,
    error: Option<String>,
    /// 要壓縮的總大小（資料夾含所有內容）；在背景計算，算好前為 None
    size: Option<u64>,
    size_rx: Option<std::sync::mpsc::Receiver<u64>>,
}

/// 檔案與資料夾（含所有內容）的總大小；不跟隨符號連結
pub(crate) fn disk_size(paths: &[PathBuf]) -> u64 {
    let mut total = 0u64;
    let mut stack: Vec<PathBuf> = paths.to_vec();
    while let Some(p) = stack.pop() {
        let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
        if m.is_dir() {
            if let Ok(d) = std::fs::read_dir(&p) {
                stack.extend(d.flatten().map(|e| e.path()));
            }
        } else if m.is_file() {
            total = total.saturating_add(m.len());
        }
    }
    total
}

impl AddDialog {
    /// 總大小（算好了才有）
    fn total_size(&mut self) -> Option<u64> {
        if self.size.is_none()
            && let Some(rx) = &self.size_rx
            && let Ok(n) = rx.try_recv()
        {
            self.size = Some(n);
            self.size_rx = None;
        }
        self.size
    }

    /// 「要壓縮：a、b、c（3 個項目，共 8.2 MB）」
    fn summary(&mut self) -> String {
        let names: Vec<String> = self.inputs.iter().take(3).map(|p| file_name(p)).collect();
        let more = if self.inputs.len() > 3 { L!(" 等", " and more") } else { "" };
        let n = self.inputs.len();
        let size = self.total_size().map(crate::human).unwrap_or_else(|| L!("計算中…", "calculating…").into());
        F!("要壓縮：{}{more}（{n} 個項目，共 {size}）", "To compress: {}{more} ({n} item(s), {size} in total)", names.join(L!("、", ", ")))
    }

    pub fn new(inputs: Vec<PathBuf>, format: Format, level: usize) -> Self {
        let first = &inputs[0];
        let dir = first.parent().unwrap_or(Path::new("."));
        let stem = if inputs.len() == 1 {
            if first.is_dir() {
                file_name(first)
            } else {
                first.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| file_name(first))
            }
        } else {
            dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| L!("壓縮檔", "Archive").into())
        };
        // 單檔格式只能壓縮一個檔案；上次用單檔格式、這次選多個時改用 zip
        let format = if format.is_single() && (inputs.len() != 1 || first.is_dir()) { Format::Zip } else { format };
        let inputs_for_size = inputs.clone();
        Self {
            archive: dir.join(format!("{stem}.{}", format.extension())).display().to_string(),
            inputs,
            format,
            level: level.min(LEVELS.len() - 1),
            password: String::new(),
            password2: String::new(),
            show_password: false,
            use_password: false,
            encrypt_names: false,
            recycle: false,
            open_folder: false,
            volume: 0,
            volume_mb: String::new(),
            volume_unit: 1,
            error: None,
            size: None,
            size_rx: {
                let (tx, rx) = std::sync::mpsc::channel();
                let paths = inputs_for_size;
                std::thread::spawn(move || {
                    let _ = tx.send(disk_size(&paths));
                });
                Some(rx)
            },
        }
    }

    /// 這次選的格式與等級（記下來給下次使用）
    pub fn choice(&self) -> (Format, usize) {
        (self.format, self.level)
    }

    fn set_format(&mut self, f: Format) {
        if !f.supports_password() {
            self.use_password = false;
        }
        let path = PathBuf::from(&self.archive);
        let name = file_name(&path);
        let stem = Format::split_extension(&name).map(|(s, _)| s.to_string()).unwrap_or(name);
        self.archive = path.with_file_name(format!("{stem}.{}", f.extension())).display().to_string();
        self.format = f;
    }

    fn validate(&self) -> Result<Task, String> {
        let output = PathBuf::from(self.archive.trim());
        if self.archive.trim().is_empty() {
            return Err(L!("請輸入壓縮檔名稱", "Enter an archive name").into());
        }
        if self.format.is_single() && (self.inputs.len() != 1 || self.inputs[0].is_dir()) {
            return Err(F!("{} 只能壓縮單一檔案，請改用 7z、zip 或 tar 格式", "{} can only compress a single file; use the 7z, zip or tar format instead", self.format.name()));
        }
        if self.use_password && self.format.supports_password() && self.password.is_empty() {
            return Err(L!("請輸入密碼，或取消勾選「設定密碼」", "Enter a password, or uncheck \"Set a password\"").into());
        }
        let password = if self.use_password && self.format.supports_password() && !self.password.is_empty() {
            if !self.show_password && self.password != self.password2 {
                return Err(L!("兩次輸入的密碼不一致", "The passwords do not match").into());
            }
            Some(self.password.clone())
        } else {
            None
        };
        if output.exists() {
            return Err(L!("壓縮檔已存在，請換一個名稱（目前不支援更新既有壓縮檔）", "The archive already exists; choose another name (updating existing archives is not supported here)").into());
        }
        let volume_size = match self.volume {
            0 => None,
            i if i == VOLUMES.len() - 1 => Some(
                self.volume_mb
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite())
                    .map(|n| (n * VOLUME_UNITS[self.volume_unit].1 as f64) as u64)
                    .filter(|&n| n > 0)
                    .ok_or(L!("請輸入分割大小（大於 0 的數字，可以有小數）", "Enter the volume size (a number greater than 0; decimals are allowed)"))?,
            ),
            i => Some(VOLUMES[i].2),
        };
        if volume_size.is_some() && output.with_file_name(format!("{}.001", output.file_name().unwrap_or_default().to_string_lossy())).exists() {
            return Err(L!("分割檔已存在，請換一個名稱", "The volumes already exist; choose another name").into());
        }
        Ok(Task::Compress {
            inputs: self.inputs.clone(),
            output,
            format: self.format,
            opts: CompressOptions {
                level: Some(LEVELS[self.level].0),
                encrypt_names: self.encrypt_names && password.is_some() && self.format == Format::SevenZ,
                password,
                volume_size,
                filter: Default::default(),
            },
            then: Then { recycle: self.recycle, open_folder: self.open_folder },
        })
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> Outcome<Task> {
        let p = pal(ui);
        ui.set_width(560.0);
        dialog_title(ui, icon::FILE_ARCHIVE, L!("壓縮", "Compress"));
        let summary = self.summary();
        if self.size.is_none() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        ui.label(RichText::new(summary).color(p.text2));
        ui.add_space(6.0);

        field_label(ui, L!("壓縮檔", "Archive"));
        let format = self.format;
        if path_field(ui, icon::FILE_ZIP, &mut self.archive)
            && let Some(p) = pick_save(&self.archive, format)
        {
            self.archive = p.display().to_string();
        }
        ui.add_space(4.0);

        egui::Grid::new("add_opts").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
            field_label(ui, L!("格式", "Format"));
            ui.horizontal(|ui| {
                let mut f = self.format;
                egui::ComboBox::from_id_salt("fmt").width(130.0).selected_text(f.name()).show_ui(ui, |ui| {
                    for x in Format::ALL {
                        ui.selectable_value(&mut f, x, x.name());
                    }
                });
                if f != self.format {
                    self.set_format(f);
                }
                ui.label(RichText::new(format_hint(self.format)).size(12.5).color(p.text2));
            });
            ui.end_row();

            field_label(ui, L!("壓縮等級", "Level"));
            egui::ComboBox::from_id_salt("lvl").width(130.0).selected_text(pick(LEVELS[self.level].1, LEVELS[self.level].2)).show_ui(ui, |ui| {
                for (i, (n, zh, en)) in LEVELS.iter().enumerate() {
                    let name = pick(zh, en);
                    ui.selectable_value(&mut self.level, i, F!("{name}（{n}）", "{name} ({n})"));
                }
            });
            ui.end_row();
        });
        ui.add_space(4.0);

        // 密碼：勾選後才顯示欄位
        let ok = self.format.supports_password();
        ui.add_enabled_ui(ok, |ui| {
            let r = ui.checkbox(&mut self.use_password, L!("設定密碼（AES-256 加密）", "Set a password (AES-256 encryption)"));
            if !ok {
                r.on_disabled_hover_text(L!("這個格式不支援密碼，請改用 zip 或 7z", "This format does not support passwords; use zip or 7z"));
            }
        });
        if self.use_password && ok {
            ui.indent("pw", |ui| {
                password_field(ui, &mut self.password, self.show_password, L!("輸入密碼", "Enter password"));
                if !self.show_password {
                    password_field(ui, &mut self.password2, false, L!("再次輸入密碼", "Re-enter password"));
                }
                ui.checkbox(&mut self.show_password, L!("顯示密碼", "Show password"));
                ui.add_enabled_ui(self.format == Format::SevenZ, |ui| {
                    ui.checkbox(&mut self.encrypt_names, L!("連檔案名稱也加密（沒有密碼就看不到裡面有哪些檔案）", "Also encrypt file names (nothing is visible without the password)"))
                        .on_disabled_hover_text(L!("只有 7z 格式可以加密檔案名稱", "Only the 7z format can encrypt file names"));
                });
            });
        }
        ui.checkbox(&mut self.recycle, L!("壓縮後將原始檔移到資源回收筒", "Move the original files to the Recycle Bin afterwards"))
            .on_hover_text(L!("會先檢查壓縮檔是否完整，確認無誤才移除；可以從資源回收筒還原", "The archive is tested first and the files are removed only if it is intact; they can be restored from the Recycle Bin"));
        ui.checkbox(&mut self.open_folder, L!("完成後開啟所在資料夾", "Open the folder when done"));
        ui.add_space(4.0);

        // 少用的選項收起來（展開與否會記住）
        egui::CollapsingHeader::new(RichText::new(L!("更多選項", "More options")).color(p.text)).id_salt("add_more").show(ui, |ui| {
            egui::Grid::new("add_more_grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                field_label(ui, L!("分割成多個檔案", "Split into volumes"));
                egui::ComboBox::from_id_salt("vol").width(180.0).selected_text(pick(VOLUMES[self.volume].0, VOLUMES[self.volume].1)).show_ui(ui, |ui| {
                    for (i, (zh, en, _)) in VOLUMES.iter().enumerate() {
                        ui.selectable_value(&mut self.volume, i, pick(zh, en));
                    }
                });
                ui.end_row();
                if self.volume == VOLUMES.len() - 1 {
                    field_label(ui, "");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.volume_mb).desired_width(90.0).hint_text(L!("例如 500", "e.g. 500")));
                        egui::ComboBox::from_id_salt("vol_unit").width(70.0).selected_text(VOLUME_UNITS[self.volume_unit].0).show_ui(ui, |ui| {
                            for (i, (unit, _)) in VOLUME_UNITS.iter().enumerate() {
                                ui.selectable_value(&mut self.volume_unit, i, *unit);
                            }
                        });
                    });
                    ui.end_row();
                }
                field_label(ui, L!("壓縮方法", "Method"));
                let method = match self.format {
                    Format::SevenZ => "LZMA2",
                    Format::Zip => if LEVELS[self.level].0 == 0 { "Store" } else { "Deflate" },
                    Format::Tar => L!("無（只封裝）", "None (store only)"),
                    Format::TarGz | Format::Gz => "Deflate (gzip)",
                    Format::TarBz2 | Format::Bz2 => "BZip2",
                    Format::TarXz | Format::Xz => "LZMA2 (xz)",
                    Format::TarZst | Format::Zst => "Zstandard",
                    // 只能讀取，不會出現在可選的格式中
                    Format::Iso => "—",
                };
                ui.label(RichText::new(method).color(p.text2));
                ui.end_row();
            });
        });
        if let Some(e) = &self.error {
            error_text(ui, e);
        }
        let (ok, cancel) = dialog_buttons(ui, L!("壓縮", "Compress"));
        if cancel {
            return Outcome::Cancel;
        }
        if ok {
            match self.validate() {
                Ok(t) => return Outcome::Ok(t),
                Err(e) => self.error = Some(e),
            }
        }
        Outcome::Pending
    }
}

/// 格式旁的一句說明：幫助選擇
fn format_hint(f: Format) -> &'static str {
    match f {
        Format::Zip => L!("Windows、macOS 不必安裝軟體就能開啟", "Opens on Windows and macOS without extra software"),
        Format::SevenZ => L!("壓縮得最小；對方需要 7-Zip 或 Zipo", "Smallest; needs 7-Zip or Zipo to open"),
        f if f.is_single() => L!("只能壓縮單一檔案", "Compresses a single file only"),
        _ => L!("常用於 Linux、macOS", "Common on Linux and macOS"),
    }
}

impl Drop for AddDialog {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.password);
        zeroize::Zeroize::zeroize(&mut self.password2);
    }
}

impl Drop for ExtractDialog {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.password);
    }
}

impl Drop for PasswordDialog {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.input);
    }
}

pub enum ExtractSource {
    /// 本機上選取的壓縮檔
    Files(Vec<PathBuf>),
    /// 從壓縮檔內解出（選取的項目，或 `None` 代表全部）
    Inside { archive: PathBuf, selection: Option<Vec<String>>, strip: String },
}

pub struct ExtractDialog {
    source: ExtractSource,
    dest: String,
    flatten: bool,
    overwrite: Overwrite,
    password: String,
    show_password: bool,
    smart: bool,
    /// 解壓後將壓縮檔移到資源回收筒（只用於本機的壓縮檔）
    recycle: bool,
    pub open_folder: bool,
    error: Option<String>,
}

impl ExtractDialog {
    /// 測試用：要解壓縮的壓縮檔數量
    #[cfg(test)]
    pub fn archive_count(&self) -> usize {
        match &self.source {
            ExtractSource::Files(f) => f.len(),
            ExtractSource::Inside { .. } => 1,
        }
    }

    pub fn new(source: ExtractSource, password: Option<String>) -> Self {
        let dest = match &source {
            ExtractSource::Files(files) if files.len() == 1 => default_dest(&files[0]),
            ExtractSource::Files(files) => files[0].parent().map(Path::to_path_buf).unwrap_or_default(),
            ExtractSource::Inside { archive, .. } => default_dest(archive),
        };
        Self {
            source,
            dest: dest.display().to_string(),
            flatten: false,
            overwrite: Overwrite::Ask,
            password: password.unwrap_or_default(),
            show_password: false,
            smart: true,
            recycle: false,
            open_folder: false,
            error: None,
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> Outcome<Task> {
        let p = pal(ui);
        ui.set_width(520.0);
        dialog_title(ui, icon::FOLDER_OPEN, L!("解壓縮", "Extract"));
        let what = match &self.source {
            ExtractSource::Files(f) if f.len() == 1 => file_name(&f[0]),
            ExtractSource::Files(f) => F!("{} 個壓縮檔（各自解到同名資料夾）", "{} archives (each into a folder of the same name)", f.len()),
            ExtractSource::Inside { archive, selection: None, .. } => F!("{} 的全部內容", "Everything in {}", file_name(archive)),
            ExtractSource::Inside { archive, selection: Some(s), .. } => F!("{} 中選取的 {} 個項目", "{1} selected item(s) in {0}", file_name(archive), s.len()),
        };
        ui.label(RichText::new(what).color(p.text2));
        ui.add_space(6.0);

        field_label(ui, L!("解壓縮到", "Extract to"));
        if path_field(ui, icon::FOLDER, &mut self.dest)
            && let Some(d) = pick_folder(&self.dest)
        {
            self.dest = d.display().to_string();
        }
        ui.checkbox(&mut self.smart, L!("壓縮檔內只有一個資料夾時，不再多包一層同名資料夾", "If the archive holds a single folder, don't wrap it in another folder of the same name"))
            .on_hover_text(L!("例如 專案.zip 裡只有「專案」資料夾，就解成 …\\專案\\，而不是 …\\專案\\專案\\", "For example, if Project.zip contains only a \"Project\" folder, extract to …\\Project\\ instead of …\\Project\\Project\\"));
        if matches!(self.source, ExtractSource::Files(_)) {
            ui.checkbox(&mut self.recycle, L!("解壓後將壓縮檔移到資源回收筒", "Move the archive to the Recycle Bin afterwards"))
                .on_hover_text(L!("全部項目都順利解出才會移除；可以從資源回收筒還原", "Removed only if every item was extracted; it can be restored from the Recycle Bin"));
        }
        ui.checkbox(&mut self.open_folder, L!("完成後開啟資料夾", "Open the folder when done"));
        ui.add_space(6.0);

        field_label(ui, L!("密碼（未加密可留白）", "Password (leave blank if not encrypted)"));
        password_field(ui, &mut self.password, self.show_password, "");
        ui.checkbox(&mut self.show_password, L!("顯示密碼", "Show password"));
        ui.add_space(4.0);

        // 少用的選項收起來（展開與否會記住）
        egui::CollapsingHeader::new(RichText::new(L!("更多選項", "More options")).color(p.text))
            .id_salt("extract_more")
            .show(ui, |ui| {
                ui.columns(2, |cols| {
                    let ui = &mut cols[0];
                    ui.label(RichText::new(L!("資料夾結構", "Folders")).strong().color(p.text));
                    ui.radio_value(&mut self.flatten, false, L!("保留資料夾結構", "Keep the folder structure"));
                    ui.radio_value(&mut self.flatten, true, L!("所有檔案放在同一層", "Put all files in one folder"));

                    let ui = &mut cols[1];
                    ui.label(RichText::new(L!("目的地已有同名檔案時", "If a file already exists")).strong().color(p.text));
                    ui.radio_value(&mut self.overwrite, Overwrite::Ask, L!("每次詢問", "Ask each time"));
                    ui.radio_value(&mut self.overwrite, Overwrite::Overwrite, L!("取代", "Replace it"));
                    ui.radio_value(&mut self.overwrite, Overwrite::Skip, L!("略過，保留原有的檔案", "Skip and keep the existing file"));
                    ui.radio_value(&mut self.overwrite, Overwrite::Rename, L!("兩者都保留（新檔自動改名）", "Keep both (rename the new file)"));
                });
            });

        if let Some(e) = &self.error {
            error_text(ui, e);
        }
        let (ok, cancel) = dialog_buttons(ui, L!("解壓縮", "Extract"));
        if cancel {
            return Outcome::Cancel;
        }
        if ok {
            if self.dest.trim().is_empty() {
                self.error = Some(L!("請指定解壓縮的位置", "Choose where to extract").into());
                return Outcome::Pending;
            }
            let mut opts = ExtractOptions {
                password: (!self.password.is_empty()).then(|| self.password.clone()),
                overwrite: self.overwrite,
                flatten: self.flatten,
                ..Default::default()
            };
            let dest = PathBuf::from(self.dest.trim());
            let archives = match &self.source {
                ExtractSource::Files(f) => f.clone(),
                ExtractSource::Inside { archive, selection, strip } => {
                    opts.selection = selection.clone();
                    opts.strip_prefix = strip.clone();
                    vec![archive.clone()]
                }
            };
            let then = Then { recycle: self.recycle && matches!(self.source, ExtractSource::Files(_)), open_folder: self.open_folder };
            return Outcome::Ok(Task::Extract { archives, dest, opts, smart: self.smart && !self.flatten, here: false, then });
        }
        Outcome::Pending
    }
}

/// 預設解壓位置：壓縮檔所在資料夾下的同名資料夾（單檔格式則直接放在旁邊）。
pub fn default_dest(archive: &Path) -> PathBuf {
    let dir = archive.parent().unwrap_or(Path::new(".")).to_path_buf();
    let name = file_name(archive);
    match Format::split_extension(&name) {
        Some((_, f)) if f.is_single() => dir,
        Some((stem, _)) if !stem.is_empty() => dir.join(stem),
        // 不是壓縮檔的分割檔（影片.mkv.001）：解出的就是一個檔案
        None if ziptool_core::is_first_volume(archive) => dir,
        _ => dir.join(format!("{name}_extracted")),
    }
}

pub enum PasswordPurpose {
    Open { path: PathBuf, inner: String },
    Retry(Task),
}

pub struct PasswordDialog {
    pub purpose: PasswordPurpose,
    pub name: String,
    pub wrong: bool,
    input: String,
    show: bool,
    focused: bool,
}

impl PasswordDialog {
    pub fn new(purpose: PasswordPurpose, name: String, wrong: bool) -> Self {
        Self { purpose, name, wrong, input: String::new(), show: false, focused: false }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> Outcome<String> {
        let p = pal(ui);
        ui.set_width(380.0);
        dialog_title(ui, icon::LOCK, L!("輸入密碼", "Enter password"));
        ui.label(RichText::new(&self.name).color(p.text2));
        ui.add_space(6.0);
        let r = password_field(ui, &mut self.input, self.show, L!("密碼", "Password"));
        if !self.focused {
            r.request_focus();
            self.focused = true;
        }
        ui.checkbox(&mut self.show, L!("顯示密碼", "Show password"));
        if self.wrong {
            error_text(ui, L!("密碼錯誤，請再試一次", "Wrong password, please try again"));
        }
        ui.add_space(4.0);
        let (ok, cancel) = dialog_buttons(ui, L!("解鎖", "Unlock"));
        if cancel {
            Outcome::Cancel
        } else if ok && !self.input.is_empty() {
            Outcome::Ok(self.input.clone())
        } else {
            Outcome::Pending
        }
    }
}

/// 壓縮檔內重新命名（F2）：與檔案總管相同，一開始選取主檔名
pub struct RenameDialog {
    archive: PathBuf,
    /// 項目所在的資料夾（壓縮檔內的路徑，最上層為空字串）
    folder: String,
    old: String,
    pub name: String,
    /// 同一資料夾中其他項目的名稱
    taken: Vec<String>,
    password: Option<String>,
    focused: bool,
}

impl RenameDialog {
    pub fn new(archive: PathBuf, folder: String, old: String, taken: Vec<String>, password: Option<String>) -> Self {
        Self { archive, folder, name: old.clone(), old, taken, password, focused: false }
    }

    /// 新名稱不能用的原因
    pub fn problem(&self) -> Option<&'static str> {
        let n = self.name.trim();
        if n.is_empty() {
            Some(L!("請輸入名稱", "Enter a name"))
        } else if n.contains(['/', '\\']) || n == "." || n == ".." {
            Some(L!("名稱不能包含 / 或 \\", "The name cannot contain / or \\"))
        } else if self.taken.iter().any(|t| t.to_lowercase() == n.to_lowercase()) {
            Some(L!("這個資料夾中已有同名的項目", "An item with this name already exists in this folder"))
        } else {
            None
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> Outcome<Task> {
        let p = pal(ui);
        ui.set_width(380.0);
        dialog_title(ui, icon::PENCIL_SIMPLE, L!("重新命名", "Rename"));
        ui.label(RichText::new(&self.old).color(p.text2));
        ui.add_space(6.0);
        let r = ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(f32::INFINITY));
        if !self.focused {
            self.focused = true;
            r.request_focus();
            // 選取主檔名（不含副檔名），直接打字就能換掉
            let stem = match self.old.rfind('.') {
                Some(i) if i > 0 => self.old[..i].chars().count(),
                _ => self.old.chars().count(),
            };
            if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), r.id) {
                let range = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(stem));
                st.cursor.set_char_range(Some(range));
                st.store(ui.ctx(), r.id);
            }
        }
        let problem = self.problem();
        if let Some(why) = problem {
            error_text(ui, why);
        }
        let (ok, cancel) = dialog_buttons(ui, L!("重新命名", "Rename"));
        let name = self.name.trim();
        if cancel || (ok && name == self.old) {
            Outcome::Cancel
        } else if ok && problem.is_none() {
            let full = |n: &str| if self.folder.is_empty() { n.to_string() } else { format!("{}/{n}", self.folder) };
            Outcome::Ok(Task::Rename { archive: self.archive.clone(), from: full(&self.old), to: full(name), password: self.password.clone() })
        } else {
            Outcome::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 「要壓縮」顯示項目數與總大小（資料夾含所有內容，在背景計算）
    #[test]
    fn summary_shows_count_and_total_size() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("照片");
        std::fs::create_dir_all(dir.join("子資料夾")).unwrap();
        std::fs::write(dir.join("a.jpg"), vec![0u8; 1000]).unwrap();
        std::fs::write(dir.join("子資料夾").join("b.jpg"), vec![0u8; 500]).unwrap();
        let f = t.path().join("說明.txt");
        std::fs::write(&f, vec![0u8; 24]).unwrap();
        let mut d = AddDialog::new(vec![dir, f], Format::Zip, 3);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while d.total_size().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(d.total_size(), Some(1524));
        assert_eq!(d.summary(), "要壓縮：照片、說明.txt（2 個項目，共 1.5 KB）");
    }

    /// 自訂分割大小可以選單位（KB、MB、GB，預設 MB），也可以輸入小數
    #[test]
    fn custom_volume_size_with_unit() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("a.txt");
        std::fs::write(&f, "a").unwrap();
        let size = |text: &str, unit: usize| {
            let mut d = AddDialog::new(vec![f.clone()], Format::Zip, 3);
            d.volume = VOLUMES.len() - 1;
            d.volume_mb = text.into();
            d.volume_unit = unit;
            d.validate().map(|t| match t {
                Task::Compress { opts, .. } => opts.volume_size,
                _ => None,
            })
        };
        assert_eq!(size("500", 1), Ok(Some(500 << 20)), "預設 MB");
        assert_eq!(size("1.5", 2), Ok(Some(3 << 29)), "1.5 GB");
        assert_eq!(size("64", 0), Ok(Some(64 << 10)), "KB");
        assert!(size("0", 1).is_err() && size("abc", 1).is_err() && size("", 2).is_err() && size("inf", 2).is_err() && size("-5", 1).is_err());
        assert_eq!(AddDialog::new(vec![f.clone()], Format::Zip, 3).volume_unit, 1, "預設單位 MB");
    }

    /// 密碼只在勾選「設定密碼」時使用；勾選了卻沒輸入時提醒
    #[test]
    fn password_is_used_only_when_enabled() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("a.txt");
        std::fs::write(&f, "a").unwrap();
        let mut d = AddDialog::new(vec![f], Format::Zip, 3);
        d.password = "secret".into();
        d.password2 = "secret".into();
        let Ok(Task::Compress { opts, .. }) = d.validate() else { panic!() };
        assert_eq!(opts.password, None, "沒有勾選：不加密");
        d.use_password = true;
        let Ok(Task::Compress { opts, .. }) = d.validate() else { panic!() };
        assert_eq!(opts.password.as_deref(), Some("secret"));
        d.password.clear();
        d.password2.clear();
        assert!(d.validate().is_err_and(|e| e.contains("密碼")), "勾選了卻沒輸入");
    }
}
