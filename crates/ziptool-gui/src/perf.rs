//! 效能診斷：設定 `ZIPTOOL_PERF=記錄檔路徑` 時，自動捲動檔案清單、記錄每一幀的時間後關閉。
//! 用來分辨卡頓來自程式邏輯（CPU）還是繪圖（GPU／畫面呈現）。

use std::path::PathBuf;
use std::time::Instant;

use eframe::egui;

pub struct Perf {
    out: PathBuf,
    frames: Vec<(f32, f32)>,
    last: Option<Instant>,
    adapter: String,
}

impl Perf {
    pub fn from_env() -> Option<Self> {
        let out = PathBuf::from(std::env::var_os("ZIPTOOL_PERF")?);
        Some(Self { out, frames: Vec::new(), last: None, adapter: String::new() })
    }

    /// 回傳這一幀要捲動到的列；量測結束時回傳 `None` 並寫出報告、關閉視窗。
    pub fn tick(&mut self, ctx: &egui::Context, frame: &eframe::Frame, rows: usize) -> Option<usize> {
        if self.adapter.is_empty()
            && let Some(rs) = frame.wgpu_render_state()
        {
            let i = rs.adapter.get_info();
            self.adapter = format!("{} / {:?} / {:?}", i.name, i.backend, i.device_type);
        }
        let now = Instant::now();
        if let Some(last) = self.last {
            let cpu = frame.info().cpu_usage.unwrap_or(0.0) * 1000.0;
            self.frames.push((cpu, (now - last).as_secs_f32() * 1000.0));
        }
        self.last = Some(now);
        ctx.request_repaint();
        const FRAMES: usize = 300;
        if self.frames.len() >= FRAMES {
            self.report();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return None;
        }
        // 前 20 幀暖身，之後每幀往下捲 3 列，到底再回頭
        let n = self.frames.len().saturating_sub(20) * 3;
        let span = rows.max(1) * 2;
        let pos = n % span;
        Some(if pos < rows { pos } else { span - pos - 1 })
    }

    fn report(&self) {
        let stat = |f: &dyn Fn(&(f32, f32)) -> f32| {
            let mut v: Vec<f32> = self.frames.iter().skip(20).map(f).collect();
            v.sort_by(|a, b| a.total_cmp(b));
            let avg = v.iter().sum::<f32>() / v.len().max(1) as f32;
            let p95 = v[(v.len() * 95 / 100).min(v.len() - 1)];
            let max = *v.last().unwrap_or(&0.0);
            format!("平均 {avg:.2} ms、p95 {p95:.2} ms、最大 {max:.2} ms")
        };
        let text = format!(
            "繪圖裝置：{}\n程式邏輯（CPU）每幀：{}\n幀間隔：{}\n樣本數：{}\n",
            self.adapter,
            stat(&|f| f.0),
            stat(&|f| f.1),
            self.frames.len().saturating_sub(20)
        );
        let _ = std::fs::write(&self.out, text);
    }
}
