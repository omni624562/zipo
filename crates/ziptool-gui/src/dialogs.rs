//! 對話框：加入壓縮檔、解壓縮、密碼。版面與選項參考 7-Zip。

use std::path::{Path, PathBuf};

use eframe::egui::{self, RichText};
use egui_phosphor::regular as icon;
use ziptool_core::{CompressOptions, ExtractOptions, Format, Overwrite};

use crate::jobs::Task;
use crate::theme::pal;
use crate::widgets::*;
use crate::{file_name, pick_save, pick_folder};

pub enum Outcome<T> {
    Pending,
    Cancel,
    Ok(T),
}

/// 7-Zip 的壓縮等級名稱
pub const LEVELS: [(u32, &str); 6] = [(0, "儲存"), (1, "最快"), (3, "快速"), (5, "標準"), (7, "最大"), (9, "極限")];

pub struct AddDialog {
    inputs: Vec<PathBuf>,
    archive: String,
    format: Format,
    level: usize,
    password: String,
    password2: String,
    show_password: bool,
    encrypt_names: bool,
    error: Option<String>,
}

impl AddDialog {
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
            dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "壓縮檔".into())
        };
        // 單檔格式只能壓縮一個檔案；上次用單檔格式、這次選多個時改回 7z
        let format = if format.is_single() && (inputs.len() != 1 || first.is_dir()) { Format::SevenZ } else { format };
        Self {
            archive: dir.join(format!("{stem}.{}", format.extension())).display().to_string(),
            inputs,
            format,
            level: level.min(LEVELS.len() - 1),
            password: String::new(),
            password2: String::new(),
            show_password: false,
            encrypt_names: false,
            error: None,
        }
    }

    /// 這次選的格式與等級（記下來給下次使用）
    pub fn choice(&self) -> (Format, usize) {
        (self.format, self.level)
    }

    fn set_format(&mut self, f: Format) {
        let path = PathBuf::from(&self.archive);
        let name = file_name(&path);
        let stem = Format::split_extension(&name).map(|(s, _)| s.to_string()).unwrap_or(name);
        self.archive = path.with_file_name(format!("{stem}.{}", f.extension())).display().to_string();
        self.format = f;
    }

    fn validate(&self) -> Result<Task, String> {
        let output = PathBuf::from(self.archive.trim());
        if self.archive.trim().is_empty() {
            return Err("請輸入壓縮檔名稱".into());
        }
        if self.format.is_single() && (self.inputs.len() != 1 || self.inputs[0].is_dir()) {
            return Err(format!("{} 只能壓縮單一檔案，請改用 7z、zip 或 tar 格式", self.format.name()));
        }
        let password = if self.format.supports_password() && !self.password.is_empty() {
            if !self.show_password && self.password != self.password2 {
                return Err("兩次輸入的密碼不一致".into());
            }
            Some(self.password.clone())
        } else {
            None
        };
        if output.exists() {
            return Err("壓縮檔已存在，請換一個名稱（目前不支援更新既有壓縮檔）".into());
        }
        Ok(Task::Compress {
            inputs: self.inputs.clone(),
            output,
            format: self.format,
            opts: CompressOptions {
                level: Some(LEVELS[self.level].0),
                encrypt_names: self.encrypt_names && password.is_some() && self.format == Format::SevenZ,
                password,
            },
        })
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> Outcome<Task> {
        let p = pal(ui);
        ui.set_width(560.0);
        dialog_title(ui, icon::FILE_ARCHIVE, "加入壓縮檔");

        field_label(ui, "壓縮檔");
        let format = self.format;
        if path_field(ui, icon::FILE_ZIP, &mut self.archive)
            && let Some(p) = pick_save(&self.archive, format)
        {
            self.archive = p.display().to_string();
        }
        ui.add_space(10.0);

        ui.columns(2, |cols| {
            let ui = &mut cols[0];
            egui::Grid::new("add_opts").num_columns(2).spacing([10.0, 10.0]).show(ui, |ui| {
                field_label(ui, "壓縮格式");
                let mut f = self.format;
                egui::ComboBox::from_id_salt("fmt").width(150.0).selected_text(f.name()).show_ui(ui, |ui| {
                    for x in Format::ALL {
                        ui.selectable_value(&mut f, x, x.name());
                    }
                });
                if f != self.format {
                    self.set_format(f);
                }
                ui.end_row();

                field_label(ui, "壓縮等級");
                egui::ComboBox::from_id_salt("lvl").width(150.0).selected_text(LEVELS[self.level].1).show_ui(ui, |ui| {
                    for (i, (n, name)) in LEVELS.iter().enumerate() {
                        ui.selectable_value(&mut self.level, i, format!("{name}（{n}）"));
                    }
                });
                ui.end_row();

                field_label(ui, "壓縮方法");
                let method = match self.format {
                    Format::SevenZ => "LZMA2",
                    Format::Zip => if LEVELS[self.level].0 == 0 { "Store" } else { "Deflate" },
                    Format::Tar => "無（只封裝）",
                    Format::TarGz | Format::Gz => "Deflate (gzip)",
                    Format::TarBz2 | Format::Bz2 => "BZip2",
                    Format::TarXz | Format::Xz => "LZMA2 (xz)",
                    Format::TarZst | Format::Zst => "Zstandard",
                };
                ui.label(RichText::new(method).color(p.text));
                ui.end_row();
            });

            let ui = &mut cols[1];
            let ok = self.format.supports_password();
            egui::Frame::group(ui.style()).corner_radius(8).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("加密").strong().color(p.text));
                ui.add_enabled_ui(ok, |ui| {
                    password_field(ui, &mut self.password, self.show_password, if ok { "輸入密碼" } else { "此格式不支援密碼" });
                    if !self.show_password {
                        password_field(ui, &mut self.password2, false, "再次輸入密碼");
                    }
                    ui.checkbox(&mut self.show_password, "顯示密碼");
                    ui.add_enabled_ui(self.format == Format::SevenZ, |ui| {
                        ui.checkbox(&mut self.encrypt_names, "加密檔案名稱");
                    });
                    ui.label(RichText::new("加密方式：AES-256").size(12.0).color(p.text2));
                });
            });
        });

        ui.add_space(8.0);
        let names: Vec<String> = self.inputs.iter().take(3).map(|p| file_name(p)).collect();
        let more = if self.inputs.len() > 3 { format!(" 等 {} 個項目", self.inputs.len()) } else { String::new() };
        ui.label(RichText::new(format!("要加入：{}{more}", names.join("、"))).size(12.5).color(p.text2));
        if let Some(e) = &self.error {
            error_text(ui, e);
        }
        let (ok, cancel) = dialog_buttons(ui, "確定");
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
    error: Option<String>,
}

impl ExtractDialog {
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
            error: None,
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> Outcome<Task> {
        let p = pal(ui);
        ui.set_width(520.0);
        dialog_title(ui, icon::FOLDER_OPEN, "解壓縮");
        let what = match &self.source {
            ExtractSource::Files(f) if f.len() == 1 => file_name(&f[0]),
            ExtractSource::Files(f) => format!("{} 個壓縮檔（各自解到同名資料夾）", f.len()),
            ExtractSource::Inside { archive, selection: None, .. } => format!("{} 的全部內容", file_name(archive)),
            ExtractSource::Inside { archive, selection: Some(s), .. } => format!("{} 中選取的 {} 個項目", file_name(archive), s.len()),
        };
        ui.label(RichText::new(what).color(p.text2));
        ui.add_space(6.0);

        field_label(ui, "解壓縮到");
        if path_field(ui, icon::FOLDER, &mut self.dest)
            && let Some(d) = pick_folder(&self.dest)
        {
            self.dest = d.display().to_string();
        }
        ui.checkbox(&mut self.smart, "壓縮檔內只有一個資料夾時，不再多包一層同名資料夾")
            .on_hover_text("例如 專案.zip 裡只有「專案」資料夾，就解成 …\\專案\\，而不是 …\\專案\\專案\\");
        ui.add_space(10.0);

        ui.columns(2, |cols| {
            let ui = &mut cols[0];
            ui.label(RichText::new("路徑模式").strong().color(p.text));
            ui.radio_value(&mut self.flatten, false, "完整路徑");
            ui.radio_value(&mut self.flatten, true, "不含路徑");
            ui.add_space(6.0);
            ui.label(RichText::new("密碼").strong().color(p.text));
            password_field(ui, &mut self.password, self.show_password, "未加密可留白");
            ui.checkbox(&mut self.show_password, "顯示密碼");

            let ui = &mut cols[1];
            ui.label(RichText::new("覆寫模式").strong().color(p.text));
            ui.radio_value(&mut self.overwrite, Overwrite::Ask, "每次詢問");
            ui.radio_value(&mut self.overwrite, Overwrite::Overwrite, "直接覆寫");
            ui.radio_value(&mut self.overwrite, Overwrite::Skip, "略過已存在的檔案");
            ui.radio_value(&mut self.overwrite, Overwrite::Rename, "自動重新命名");
        });

        if let Some(e) = &self.error {
            error_text(ui, e);
        }
        let (ok, cancel) = dialog_buttons(ui, "確定");
        if cancel {
            return Outcome::Cancel;
        }
        if ok {
            if self.dest.trim().is_empty() {
                self.error = Some("請指定解壓縮的位置".into());
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
            return Outcome::Ok(Task::Extract { archives, dest, opts, smart: self.smart && !self.flatten });
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
        dialog_title(ui, icon::LOCK, "輸入密碼");
        ui.label(RichText::new(&self.name).color(p.text2));
        ui.add_space(6.0);
        let r = password_field(ui, &mut self.input, self.show, "密碼");
        if !self.focused {
            r.request_focus();
            self.focused = true;
        }
        ui.checkbox(&mut self.show, "顯示密碼");
        if self.wrong {
            error_text(ui, "密碼錯誤，請再試一次");
        }
        ui.add_space(4.0);
        let (ok, cancel) = dialog_buttons(ui, "確定");
        if cancel {
            Outcome::Cancel
        } else if ok && !self.input.is_empty() {
            Outcome::Ok(self.input.clone())
        } else {
            Outcome::Pending
        }
    }
}
