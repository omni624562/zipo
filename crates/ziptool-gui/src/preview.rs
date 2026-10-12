//! 預覽窗格：讀取圖片與文字檔的內容。
//!
//! 壓縮檔內的檔案可能是惡意的，圖片解碼時限制尺寸與記憶體，文字只讀開頭的一段。

use std::io::Read;
use std::path::Path;

use eframe::egui;

/// 壓縮檔內的檔案要先解出才能預覽，超過此大小不預覽
pub const MAX_ARCHIVE_PREVIEW: u64 = 50 << 20;
/// 文字檔只讀開頭這麼多位元組
const TEXT_LIMIT: usize = 64 << 10;
/// 預覽圖的最大邊長（縮小後再顯示，節省記憶體）
const THUMB_MAX: u32 = 1024;

const IMAGE_EXT: [&str; 7] = ["png", "jpg", "jpeg", "gif", "bmp", "webp", "jfif"];
const TEXT_EXT: [&str; 44] = [
    "txt", "md", "markdown", "json", "xml", "csv", "tsv", "log", "ini", "cfg", "conf", "toml", "yaml", "yml", "rs", "py", "js",
    "ts", "jsx", "tsx", "c", "h", "cpp", "hpp", "cs", "java", "kt", "go", "rb", "php", "html", "htm", "css", "scss", "sh",
    "bat", "cmd", "ps1", "sql", "srt", "nfo", "gitignore", "editorconfig", "properties",
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Image,
    Text,
}

/// 依副檔名判斷能不能預覽
pub fn kind(name: &str) -> Option<Kind> {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    if IMAGE_EXT.contains(&ext.as_str()) {
        Some(Kind::Image)
    } else if TEXT_EXT.contains(&ext.as_str()) {
        Some(Kind::Text)
    } else {
        None
    }
}

pub enum Content {
    /// 縮小後的圖片與原始尺寸
    Image(egui::ColorImage, [u32; 2]),
    /// 文字內容；`truncated` 表示只顯示了開頭
    Text { text: String, truncated: bool },
    /// 無法預覽（附原因，可為空字串）
    Unavailable(String),
}

/// 讀取檔案內容供預覽
pub fn load(path: &Path) -> Content {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match kind(&name) {
        Some(Kind::Image) => load_image(path).unwrap_or_else(|e| Content::Unavailable(format!("無法顯示圖片：{e}"))),
        Some(Kind::Text) => load_text(path).unwrap_or_else(|e| Content::Unavailable(format!("無法讀取：{e}"))),
        None => Content::Unavailable(String::new()),
    }
}

fn load_image(path: &Path) -> Result<Content, String> {
    let mut reader = image::ImageReader::open(path).map_err(|e| e.to_string())?.with_guessed_format().map_err(|e| e.to_string())?;
    // 惡意圖片可以宣告極大的尺寸讓解碼時耗盡記憶體
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(256 << 20);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?;
    let size = [img.width(), img.height()];
    let img = if size[0] > THUMB_MAX || size[1] > THUMB_MAX { img.thumbnail(THUMB_MAX, THUMB_MAX) } else { img };
    let rgba = img.to_rgba8();
    let image = egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
    Ok(Content::Image(image, size))
}

fn load_text(path: &Path) -> Result<Content, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(TEXT_LIMIT as u64 + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    let truncated = bytes.len() > TEXT_LIMIT;
    bytes.truncate(TEXT_LIMIT);
    Ok(match decode_text(&bytes) {
        Some(text) => Content::Text { text, truncated },
        None => Content::Unavailable("看起來不是文字檔".into()),
    })
}

/// 文字解碼：UTF-8／UTF-16（有 BOM），不是 UTF-8 時以 Big5 解讀；含有 NUL 的視為二進位檔
pub fn decode_text(bytes: &[u8]) -> Option<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Some(String::from_utf8_lossy(rest).into_owned());
    }
    let utf16 = |rest: &[u8], le: bool| {
        let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|c| if le { u16::from_le_bytes(*c) } else { u16::from_be_bytes(*c) }).collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Some(utf16(rest, true));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return Some(utf16(rest, false));
    }
    if bytes.contains(&0) {
        return None;
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Some(s.to_string()),
        // 只是被截斷在多位元組字元中間
        Err(e) if e.error_len().is_none() => Some(String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned()),
        Err(_) => Some(encoding_rs::BIG5.decode(bytes).0.into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(kind("照片.JPG"), Some(Kind::Image));
        assert_eq!(kind("a/b/readme.md"), Some(Kind::Text));
        assert_eq!(kind("setup.exe"), None);
        assert_eq!(kind("沒有副檔名"), None);
    }

    #[test]
    fn text_decoding() {
        assert_eq!(decode_text("中文 UTF-8".as_bytes()).as_deref(), Some("中文 UTF-8"));
        assert_eq!(decode_text(b"\xEF\xBB\xBFbom").as_deref(), Some("bom"));
        let mut le = vec![0xFF, 0xFE];
        le.extend("寬字元".encode_utf16().flat_map(|u| u.to_le_bytes()));
        assert_eq!(decode_text(&le).as_deref(), Some("寬字元"));
        // Big5 的「中文」
        assert_eq!(decode_text(&[0xA4, 0xA4, 0xA4, 0xE5]).as_deref(), Some("中文"));
        // 截斷在多位元組字元中間：去掉不完整的部分
        let mut cut = "測試".as_bytes().to_vec();
        cut.pop();
        assert_eq!(decode_text(&cut).as_deref(), Some("測"));
        // 二進位檔
        assert_eq!(decode_text(b"MZ\x90\x00\x03"), None);
    }

    #[test]
    fn loads_images_and_text() {
        let tmp = tempfile::tempdir().unwrap();
        // 2000×1000 的圖片：縮小到最長邊 1024，記錄原始尺寸
        let png = tmp.path().join("big.png");
        image::RgbImage::from_pixel(2000, 1000, image::Rgb([200, 30, 30])).save(&png).unwrap();
        match load(&png) {
            Content::Image(img, size) => {
                assert_eq!(size, [2000, 1000]);
                assert_eq!(img.size, [1024, 512]);
            }
            _ => panic!("應為圖片"),
        }
        let txt = tmp.path().join("long.txt");
        std::fs::write(&txt, "字".repeat(30_000)).unwrap();
        match load(&txt) {
            Content::Text { text, truncated } => {
                assert!(truncated);
                assert!(text.len() <= TEXT_LIMIT && text.chars().all(|c| c == '字'));
            }
            _ => panic!("應為文字"),
        }
        // 副檔名是圖片但內容不是：顯示原因，不會當掉
        let fake = tmp.path().join("fake.jpg");
        std::fs::write(&fake, "not an image").unwrap();
        assert!(matches!(load(&fake), Content::Unavailable(m) if m.contains("無法顯示圖片")));
    }

    /// 宣告極大尺寸的圖片會被拒絕，而不是耗盡記憶體
    #[test]
    fn huge_declared_image_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let png = tmp.path().join("bomb.png");
        // 只有檔頭的 PNG：宣告 60000×60000
        let mut data = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
        data.extend(60000u32.to_be_bytes());
        data.extend(60000u32.to_be_bytes());
        data.extend([8, 2, 0, 0, 0]);
        let crc = crc32(&data[12..]);
        data.extend(crc.to_be_bytes());
        std::fs::write(&png, data).unwrap();
        match load(&png) {
            Content::Unavailable(m) => assert!(m.to_lowercase().contains("limit"), "應因尺寸限制而拒絕：{m}"),
            _ => panic!("不應解碼"),
        }
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in bytes {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
}
