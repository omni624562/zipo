//! 共用元件。尺寸依 Windows 11 規範：控制項高 32 px、4 px 圓角、圖示 16／20 px。

use ziptool_core::L;
use eframe::egui::{self, Align, FontId, Layout, RichText, Sense, Stroke, TextStyle, vec2};
use egui_phosphor::regular as icon;

use crate::theme::{pal, semibold};

/// 所有控制項的標準高度
pub const CONTROL_H: f32 = 32.0;

pub fn link(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let p = pal(ui);
    ui.add(egui::Button::new(RichText::new(text).color(p.accent)).frame(false)).on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn primary_button<'a>(ui: &egui::Ui, label: &'a str) -> egui::Button<'a> {
    let p = pal(ui);
    egui::Button::new(RichText::new(label).color(p.on_accent))
        .fill(p.accent_fill)
        .stroke(Stroke::NONE)
        .corner_radius(4)
        .min_size(vec2(96.0, CONTROL_H))
}

pub fn secondary_button<'a>(ui: &egui::Ui, label: &'a str) -> egui::Button<'a> {
    let p = pal(ui);
    egui::Button::new(RichText::new(label).color(p.text))
        .fill(p.control)
        .stroke(Stroke::new(1.0, p.border_strong))
        .corner_radius(4)
        .min_size(vec2(96.0, CONTROL_H))
}

pub fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).color(pal(ui).text));
}

/// 工具列按鈕：20 px 圖示＋右側文字（Windows 11 建議的寬視窗樣式），高 32 px。
/// 圖示與文字都以實際排版後的高度垂直置中，不同圖示不會高低不一。
pub fn command_button(ui: &mut egui::Ui, glyph: &str, label: Option<&str>) -> egui::Response {
    let p = pal(ui);
    let enabled = ui.is_enabled();
    let fg = if enabled { p.text } else { p.muted };
    let icon_color = if enabled { p.accent } else { p.muted };
    let icon = ui.painter().layout_no_wrap(glyph.to_string(), FontId::proportional(20.0), icon_color);
    let text = label.map(|l| ui.painter().layout_no_wrap(l.to_string(), FontId::proportional(14.0), fg));
    let pad = if text.is_some() { 10.0 } else { 6.0 };
    let gap = 8.0;
    let w = pad * 2.0 + 20.0 + text.as_ref().map_or(0.0, |t| gap + t.size().x);
    let (rect, resp) = ui.allocate_exact_size(vec2(w.max(CONTROL_H), CONTROL_H), Sense::click());
    if enabled && (resp.hovered() || resp.has_focus()) {
        let fill = if resp.is_pointer_button_down_on() { p.pressed } else { p.hover };
        ui.painter().rect_filled(rect, 4, fill);
    }
    let x0 = if text.is_some() { rect.left() + pad } else { rect.center().x - 10.0 };
    let icon_pos = egui::pos2(x0 + (20.0 - icon.size().x) / 2.0, rect.center().y - icon.size().y / 2.0);
    ui.painter().galley(icon_pos, icon, icon_color);
    if let Some(t) = text {
        let pos = egui::pos2(x0 + 20.0 + gap, rect.center().y - t.size().y / 2.0);
        ui.painter().galley(pos, t, fg);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 只有圖示的 32×32 按鈕（上一頁、下一頁、上一層）。
pub fn icon_button(ui: &mut egui::Ui, glyph: &str) -> egui::Response {
    let p = pal(ui);
    let enabled = ui.is_enabled();
    let (rect, resp) = ui.allocate_exact_size(vec2(CONTROL_H, CONTROL_H), Sense::click());
    if enabled && resp.hovered() {
        ui.painter().rect_filled(rect, 4, if resp.is_pointer_button_down_on() { p.pressed } else { p.hover });
    }
    let g = ui.painter().layout_no_wrap(glyph.to_string(), FontId::proportional(16.0), if enabled { p.text } else { p.muted });
    let pos = rect.center() - g.size() / 2.0;
    ui.painter().galley(pos, g, p.text);
    resp
}

/// 工具列分隔線
pub fn separator_v(ui: &mut egui::Ui) {
    let p = pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(9.0, CONTROL_H), Sense::hover());
    ui.painter().vline(rect.center().x, rect.center().y - 12.0..=rect.center().y + 12.0, Stroke::new(1.0, p.border_strong));
}

/// 高 32 px 的外框輸入區：前置 16 px 圖示＋內容（內容在其中垂直置中）。
pub fn input_frame(ui: &mut egui::Ui, lead: &str, add: impl FnOnce(&mut egui::Ui)) -> egui::Response {
    let p = pal(ui);
    egui::Frame::new()
        .fill(p.input)
        .stroke(Stroke::new(1.0, p.border_strong))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(10, 0))
        .show(ui, |ui| {
            let w = ui.available_width();
            ui.allocate_ui_with_layout(vec2(w, CONTROL_H - 2.0), Layout::left_to_right(Align::Center), |ui| {
                // 撐滿可用寬度，外框不隨內容縮短
                ui.set_min_size(vec2(w, CONTROL_H - 2.0));
                ui.spacing_mut().item_spacing.x = 8.0;
                if !lead.is_empty() {
                    ui.label(RichText::new(lead).size(16.0).color(p.text2));
                }
                add(ui);
            });
        })
        .response
}

/// 放不下時從中間省略：保留開頭約三分之一、結尾約三分之二（檔名、目的資料夾最重要）
pub fn elide_middle(text: &str, max: f32, width: impl Fn(&str) -> f32) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let make = |keep: usize| {
        let head = keep / 3;
        let tail = keep - head;
        let mut s: String = chars[..head].iter().collect();
        s.push('…');
        s.extend(&chars[chars.len() - tail..]);
        s
    };
    // 找出放得下的最多字數
    let (mut lo, mut hi) = (0, chars.len() - 1);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if width(&make(mid)) <= max { lo = mid } else { hi = mid - 1 }
    }
    make(lo)
}

/// 路徑欄位，回傳是否按下「瀏覽」。
/// 沒有在編輯時，太長的路徑從中間省略（保留檔名），滑過時顯示完整路徑；點一下即可編輯。
pub fn path_field(ui: &mut egui::Ui, lead: &str, value: &mut String) -> bool {
    let mut clicked = false;
    let edit_id = ui.id().with(("path_field", lead));
    let editing = ui.data(|d| d.get_temp::<bool>(edit_id)).unwrap_or(false);
    input_frame(ui, lead, |ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            clicked = link(ui, L!("瀏覽…", "Browse…")).clicked();
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                if editing {
                    let r = ui.add(egui::TextEdit::singleline(value).id(edit_id).frame(egui::Frame::NONE).desired_width(ui.available_width()));
                    if r.lost_focus() {
                        ui.data_mut(|d| d.insert_temp(edit_id, false));
                    }
                } else {
                    let font = TextStyle::Body.resolve(ui.style());
                    let color = ui.visuals().text_color();
                    let max = ui.available_width() - 4.0;
                    let shown = elide_middle(value, max, |t| ui.fonts_mut(|f| f.layout_no_wrap(t.to_string(), font.clone(), color).size().x));
                    let r = ui.add(egui::Label::new(shown).sense(Sense::click()).truncate()).on_hover_cursor(egui::CursorIcon::Text);
                    let r = if r.rect.width() > 0.0 && shown_differs(value, max, ui) { r.on_hover_text(value.as_str()) } else { r };
                    // 點整個欄位都能開始編輯
                    if r.clicked() || ui.interact(ui.max_rect(), edit_id.with("area"), Sense::click()).clicked() {
                        ui.data_mut(|d| d.insert_temp(edit_id, true));
                        ui.memory_mut(|m| m.request_focus(edit_id));
                    }
                }
            });
        });
    });
    clicked
}

/// 顯示的路徑是否經過省略（是的話滑過時顯示完整路徑）
fn shown_differs(value: &str, max: f32, ui: &egui::Ui) -> bool {
    let font = TextStyle::Body.resolve(ui.style());
    ui.fonts_mut(|f| f.layout_no_wrap(value.to_string(), font, egui::Color32::WHITE).size().x) > max
}

pub fn password_field(ui: &mut egui::Ui, value: &mut String, show: bool, hint: &str) -> egui::Response {
    let mut resp = None;
    input_frame(ui, icon::LOCK_SIMPLE, |ui| {
        resp = Some(ui.add(
            egui::TextEdit::singleline(value)
                .frame(egui::Frame::NONE)
                .password(!show)
                .hint_text(hint)
                .desired_width(ui.available_width()),
        ));
    });
    let resp = resp.unwrap();
    // 密碼欄不保留復原紀錄：egui 會把每次的內容以明文存在記憶體中，對話框關閉後仍在
    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), resp.id) {
        state.clear_undoer();
        state.store(ui.ctx(), resp.id);
    }
    resp
}

/// 進度條：4 px 高、2 px 圓角（Windows 11 ProgressBar 樣式）
pub fn progress_bar(ui: &mut egui::Ui, frac: f32) {
    let p = pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), Sense::hover());
    ui.painter().rect_filled(rect, 2, p.sunken);
    let mut fill = rect;
    fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
    ui.painter().rect_filled(fill, 2, p.accent);
}

/// 對話框底部的「確定／取消」列；回傳 (確定, 取消)。主要按鈕在左、取消在右（Windows 慣例）。
pub fn dialog_buttons(ui: &mut egui::Ui, ok_label: &str) -> (bool, bool) {
    let mut ok = false;
    let mut cancel = false;
    ui.add_space(12.0);
    ui.allocate_ui_with_layout(vec2(ui.available_width(), CONTROL_H), Layout::right_to_left(Align::Center), |ui| {
        cancel = ui.add(secondary_button(ui, L!("取消", "Cancel"))).clicked();
        ok = ui.add(primary_button(ui, ok_label)).clicked();
    });
    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
    let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
    (ok || enter, cancel || esc)
}

/// 對話框標題：20 px 半粗體（Windows 11 ContentDialog 標題樣式）
pub fn dialog_title(ui: &mut egui::Ui, glyph: &str, title: &str) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new(glyph).size(20.0).color(p.accent));
        ui.label(RichText::new(title).text_style(TextStyle::Heading).family(semibold()).color(p.text));
    });
    ui.add_space(12.0);
}

pub fn error_text(ui: &mut egui::Ui, msg: &str) {
    let p = pal(ui);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(icon::WARNING_CIRCLE).color(p.danger));
        ui.label(RichText::new(msg).color(p.danger));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 太長的路徑從中間省略：保留開頭（磁碟）與結尾（檔名），不超過可用寬度
    #[test]
    fn long_paths_keep_their_end() {
        let width = |t: &str| t.chars().count() as f32;
        let path = r"C:\Users\nan\AppData\Local\Temp\claude\project\release043\zipo-0.4.3.zip";
        let short = elide_middle(path, 32.0, width);
        assert!(short.starts_with("C:\\") && short.ends_with("zipo-0.4.3.zip") && short.contains('…'), "{short}");
        assert!(width(&short) <= 32.0, "{short}");
        // 放得下：不變
        assert_eq!(elide_middle(r"C:\a\b.zip", 32.0, width), r"C:\a\b.zip");
        // 連檔名都放不下：至少保留檔名的結尾
        let tiny = elide_middle(path, 8.0, width);
        assert!(tiny.contains('…') && tiny.ends_with(".zip") && width(&tiny) <= 8.0, "{tiny}");
    }

    /// 路徑欄位：平常顯示省略後的路徑，點一下就能編輯，輸入的文字寫回欄位
    #[test]
    fn path_field_becomes_editable_on_click() {
        let ctx = egui::Context::default();
        let mut value = r"C:\Users\nan\AppData\Local\Temp\claude\很長的資料夾名稱\再一層\out".to_string();
        let frame = |events: Vec<egui::Event>, value: &mut String| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(300.0, 100.0))),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                path_field(ui, "", value);
            });
            out.textures_delta.clear();
        };
        frame(vec![], &mut value);
        let at = egui::pos2(60.0, 16.0);
        let btn = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        frame(vec![egui::Event::PointerMoved(at), btn(true)], &mut value);
        frame(vec![egui::Event::PointerMoved(at), btn(false)], &mut value);
        frame(vec![], &mut value);
        frame(vec![egui::Event::Key { key: egui::Key::End, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }, egui::Event::Text("2".into())], &mut value);
        assert!(value.ends_with("out2"), "{value}");
    }

    /// 密碼欄不可保留復原紀錄：egui 會把每次輸入的內容以明文存在記憶體中，對話框關閉後仍在
    #[test]
    fn password_field_keeps_no_undo_history() {
        let ctx = egui::Context::default();
        let mut pw = String::new();
        let mut id = None;
        for text in ["", "secret", "-more", "!"] {
            let events = if text.is_empty() { vec![] } else { vec![egui::Event::Text(text.into())] };
            let mut out = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
                let r = password_field(ui, &mut pw, false, "");
                r.request_focus();
                id = Some(r.id);
            });
            out.textures_delta.clear();
        }
        assert_eq!(pw, "secret-more!");
        let state = egui::TextEdit::load_state(&ctx, id.unwrap()).unwrap();
        let undo = state.undoer();
        assert!(!undo.is_in_flux() && format!("{undo:?}").contains("undo count: 0"), "不應留下復原紀錄：{undo:?}");
    }
}
