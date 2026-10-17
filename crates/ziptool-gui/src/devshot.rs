//! 僅 debug 版：設定 `ZIPTOOL_SHOT=輸出.png` 時，自動截圖後關閉，用來檢查畫面。
//! 選用：`ZIPTOOL_SHOT_OPEN=路徑`、`ZIPTOOL_SHOT_SELECT=選取前幾列`、
//! `ZIPTOOL_SHOT_DIALOG=add|extract|info|about|update|updating|menu|view-menu`、`ZIPTOOL_SHOT_DARK=1`、`ZIPTOOL_SHOT_LANG=en|zh`。

use std::path::PathBuf;

use eframe::egui;

pub struct DevShot {
    out: PathBuf,
    /// 在工作進行中截圖（用來檢查進度視窗）
    pub progress: bool,
    frame: u32,
    requested: bool,
}

pub struct Setup {
    pub open: Option<PathBuf>,
    pub select: usize,
    pub dialog: Option<String>,
}

impl DevShot {
    pub fn from_env(ctx: &egui::Context) -> Option<(Self, Setup)> {
        let out = PathBuf::from(std::env::var_os("ZIPTOOL_SHOT")?);
        let dark = std::env::var("ZIPTOOL_SHOT_DARK").is_ok_and(|v| v == "1");
        if let Ok(lang) = std::env::var("ZIPTOOL_SHOT_LANG") {
            ziptool_core::i18n::set_english(lang == "en");
        }
        ctx.set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
        let setup = Setup {
            open: std::env::var_os("ZIPTOOL_SHOT_OPEN").map(PathBuf::from),
            select: std::env::var("ZIPTOOL_SHOT_SELECT").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
            dialog: std::env::var("ZIPTOOL_SHOT_DIALOG").ok(),
        };
        let progress = setup.dialog.as_deref().is_some_and(|d| d.starts_with("progress"));
        Some((Self { out, progress, frame: 0, requested: false }, setup))
    }

    /// 每個畫面呼叫一次；`ready` 為 true 後再等幾幀才截圖。
    pub fn tick(&mut self, ctx: &egui::Context, ready: bool) {
        ctx.request_repaint();
        if ready {
            self.frame += 1;
        }
        if self.frame == 10 && !self.requested {
            self.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let [w, h] = img.size;
            let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, bytes) {
                let _ = buf.save(&self.out);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

/// 截圖時忽略滑鼠：游標剛好停在視窗內時會出現滑過效果或跳出提示框，每次截圖的畫面不同
pub fn ignore_pointer(raw: &mut egui::RawInput) {
    raw.events.retain(|e| {
        !matches!(
            e,
            egui::Event::PointerMoved(_) | egui::Event::MouseMoved(_) | egui::Event::PointerButton { .. } | egui::Event::MouseWheel { .. } | egui::Event::Touch { .. }
        )
    });
    raw.events.push(egui::Event::PointerGone);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_pointer_hides_cursor() {
        let ctx = egui::Context::default();
        let mut raw = egui::RawInput::default();
        raw.events.push(egui::Event::PointerMoved(egui::pos2(100.0, 100.0)));
        ignore_pointer(&mut raw);
        let mut out = ctx.run_ui(raw, |_| {});
        out.textures_delta.clear();
        assert_eq!(ctx.input(|i| i.pointer.hover_pos()), None);
    }
}
