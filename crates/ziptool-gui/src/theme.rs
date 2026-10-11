//! 配色、字型與元件樣式：依 Windows 11（Fluent）設計規範。
//!
//! - 控制項 4 px 圓角、對話框與浮出視窗 8 px 圓角
//! - 內文 14 px、說明文字 12 px
//! - 字型：Segoe UI（英數）＋ Microsoft JhengHei UI（中文），與 Windows 原生介面相同

use std::sync::Arc;

use eframe::egui::{self, Color32, FontFamily, FontId, Stroke, TextStyle, Theme, Visuals, vec2};

pub struct Palette {
    /// 上方工具列、網址列與狀態列的底色
    pub chrome: Color32,
    /// 檔案清單等內容區的底色
    pub bg: Color32,
    /// 對話框、浮出視窗
    pub card: Color32,
    /// 次要底色（進度條軌道、說明區塊）
    pub sunken: Color32,
    pub input: Color32,
    /// 一般按鈕底色
    pub control: Color32,
    pub hover: Color32,
    pub pressed: Color32,
    /// 清單選取列
    pub selected: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub text: Color32,
    pub text2: Color32,
    pub muted: Color32,
    /// 強調色文字與圖示
    pub accent: Color32,
    /// 主要按鈕底色與其上的文字
    pub accent_fill: Color32,
    pub on_accent: Color32,
    pub success: Color32,
    pub danger: Color32,
    /// 檔案類型圖示色
    pub folder: Color32,
    pub archive: Color32,
    pub image: Color32,
    pub doc_red: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Windows 11 淺色主題（預設強調色）
pub const LIGHT: Palette = Palette {
    chrome: rgb(0xF3F3F3),
    bg: rgb(0xFBFBFB),
    card: rgb(0xFFFFFF),
    sunken: rgb(0xEBEBEB),
    input: rgb(0xFFFFFF),
    control: rgb(0xFBFBFB),
    hover: rgb(0xEAEAEA),
    pressed: rgb(0xE0E0E0),
    selected: rgb(0xDCEBFA),
    border: rgb(0xE5E5E5),
    border_strong: rgb(0xD1D1D1),
    text: rgb(0x1A1A1A),
    text2: rgb(0x5D5D5D),
    muted: rgb(0xA0A0A0),
    accent: rgb(0x005FB8),
    accent_fill: rgb(0x005FB8),
    on_accent: rgb(0xFFFFFF),
    success: rgb(0x0F7B0F),
    danger: rgb(0xC42B1C),
    folder: rgb(0xD29200),
    archive: rgb(0x7F56D9),
    image: rgb(0x1D9E75),
    doc_red: rgb(0xD13438),
};

/// Windows 11 深色主題
pub const DARK: Palette = Palette {
    chrome: rgb(0x202020),
    bg: rgb(0x272727),
    card: rgb(0x2B2B2B),
    sunken: rgb(0x1C1C1C),
    input: rgb(0x1F1F1F),
    control: rgb(0x2D2D2D),
    hover: rgb(0x383838),
    pressed: rgb(0x303030),
    selected: rgb(0x173B57),
    border: rgb(0x333333),
    border_strong: rgb(0x454545),
    text: rgb(0xFFFFFF),
    text2: rgb(0xC5C5C5),
    muted: rgb(0x787878),
    accent: rgb(0x60CDFF),
    accent_fill: rgb(0x60CDFF),
    on_accent: rgb(0x000000),
    success: rgb(0x6CCB5F),
    danger: rgb(0xFF99A4),
    folder: rgb(0xF2C14E),
    archive: rgb(0xB49CF0),
    image: rgb(0x5DCAA5),
    doc_red: rgb(0xFF8A8A),
};

pub fn pal(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

/// 標題用的半粗體字族
pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn setup(ctx: &egui::Context) {
    setup_fonts(ctx);
    ctx.set_visuals_of(Theme::Light, visuals(&LIGHT, Visuals::light()));
    ctx.set_visuals_of(Theme::Dark, visuals(&DARK, Visuals::dark()));
    ctx.all_styles_mut(|s| {
        s.text_styles = [
            (TextStyle::Small, FontId::proportional(12.0)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, FontId::proportional(14.0)),
            (TextStyle::Heading, FontId::new(20.0, semibold())),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ]
        .into();
        // 4 px 網格：元件間距 8、按鈕內距 12×6、控制項高度 32
        s.spacing.item_spacing = vec2(8.0, 8.0);
        s.spacing.button_padding = vec2(12.0, 6.0);
        s.spacing.interact_size.y = 32.0;
        s.spacing.menu_margin = egui::Margin::same(4);
    });
}

fn visuals(p: &Palette, mut v: Visuals) -> Visuals {
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.extreme_bg_color = p.input;
    v.faint_bg_color = p.sunken;
    v.hyperlink_color = p.accent;
    v.error_fg_color = p.danger;
    v.selection.bg_fill = p.selected;
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.slider_trailing_fill = true;
    // 對話框、選單等浮出元素 8 px；一般控制項 4 px
    v.window_corner_radius = 8.into();
    v.menu_corner_radius = 8.into();

    let w = &mut v.widgets;
    for (wv, fill) in [(&mut w.inactive, p.control), (&mut w.hovered, p.hover), (&mut w.active, p.pressed), (&mut w.open, p.hover)] {
        wv.corner_radius = 4.into();
        wv.weak_bg_fill = fill;
        wv.bg_fill = fill;
        wv.bg_stroke = Stroke::new(1.0, p.border_strong);
        wv.fg_stroke = Stroke::new(1.0, p.text);
        wv.expansion = 0.0;
    }
    w.hovered.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = 4.into();
    v
}

fn load(path: &str, index: u32) -> Option<egui::FontData> {
    let bytes = std::fs::read(path).ok()?;
    let mut data = egui::FontData::from_owned(bytes);
    data.index = index;
    Some(data)
}

/// 字型：英數用 Segoe UI，中文用 Microsoft JhengHei UI（微軟正黑體的介面版本，行距較緊湊），
/// 再加入 Phosphor 圖示字型；找不到時退回 egui 內建字型與其他中文字型。
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    let defaults = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();

    let mut add = |name: &str, candidates: &[(&str, u32)]| -> bool {
        match candidates.iter().find_map(|(p, i)| load(p, *i)) {
            Some(d) => {
                fonts.font_data.insert(name.into(), Arc::new(d));
                true
            }
            None => false,
        }
    };
    let latin = add("latin", &[(r"C:\Windows\Fonts\segoeui.ttf", 0)]);
    let latin_sb = add("latin_sb", &[(r"C:\Windows\Fonts\seguisb.ttf", 0)]);
    let cjk = add(
        "cjk",
        &[
            (r"C:\Windows\Fonts\msjh.ttc", 1), // Microsoft JhengHei UI
            (r"C:\Windows\Fonts\mingliu.ttc", 0),
            ("/System/Library/Fonts/PingFang.ttc", 0),
            ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0),
        ],
    );
    let cjk_bold = add("cjk_bold", &[(r"C:\Windows\Fonts\msjhbd.ttc", 1)]);

    // 圖示字型位於 Unicode 私人使用區，放在中文字型之前，避免被中文字型同區段的字形蓋掉
    let mut regular: Vec<String> = Vec::new();
    if latin {
        regular.push("latin".into());
    }
    regular.push("phosphor".into());
    if cjk {
        regular.push("cjk".into());
    }
    let rest: Vec<String> = defaults.iter().filter(|f| !regular.contains(f)).cloned().collect();
    regular.extend(rest);

    let mut bold: Vec<String> = Vec::new();
    if latin_sb {
        bold.push("latin_sb".into());
    }
    if cjk_bold {
        bold.push("cjk_bold".into());
    }
    let rest: Vec<String> = regular.iter().filter(|f| !bold.contains(f)).cloned().collect();
    bold.extend(rest);

    fonts.families.insert(FontFamily::Proportional, regular);
    fonts.families.insert(semibold(), bold);
    if cjk {
        fonts.families.entry(FontFamily::Monospace).or_default().push("cjk".into());
    }
    ctx.set_fonts(fonts);
}
