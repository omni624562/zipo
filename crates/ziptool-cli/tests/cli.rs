//! 直接執行 `zipo` 執行檔的端對端測試。

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// 執行 ziptool；`stdin` 會被送進標準輸入（模擬使用者回答）。
fn run(dir: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zipo"))
        // 測試比對中文訊息；CI 的系統語言是英文
        .env("ZIPO_LANG", "zh")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn tree(dir: &Path) {
    fs::create_dir_all(dir.join("data/sub")).unwrap();
    fs::write(dir.join("data/a.txt"), "第一個檔案\n".repeat(500)).unwrap();
    fs::write(dir.join("data/sub/b.txt"), "第二個檔案").unwrap();
}

#[test]
fn add_list_test_extract_roundtrip() {
    let t = tempfile::tempdir().unwrap();
    tree(t.path());
    for (fmt, name) in [("7z", "x.7z"), ("zip", "x.zip"), ("tar.zst", "x.tar.zst")] {
        let o = run(t.path(), &["a", name, "data", "-bd", "-mx9"], "");
        assert!(o.status.success(), "{fmt} a：{}", text(&o));

        let o = run(t.path(), &["l", name], "");
        assert!(o.status.success() && text(&o).contains("data/sub/b.txt"), "{fmt} l：{}", text(&o));

        let o = run(t.path(), &["t", name, "-bd"], "");
        assert!(o.status.success() && text(&o).contains("一切正常"), "{fmt} t：{}", text(&o));

        let out = format!("out_{fmt}");
        let o = run(t.path(), &["x", name, &format!("-o{out}"), "-bd"], "");
        assert!(o.status.success(), "{fmt} x：{}", text(&o));
        assert_eq!(fs::read(t.path().join(&out).join("data/a.txt")).unwrap(), fs::read(t.path().join("data/a.txt")).unwrap());
    }
}

#[test]
fn add_to_existing_archive_updates_it() {
    let t = tempfile::tempdir().unwrap();
    tree(t.path());
    // 不是壓縮檔的同名檔案：失敗，而且原檔不被破壞
    fs::write(t.path().join("x.7z"), "舊檔").unwrap();
    let o = run(t.path(), &["a", "x.7z", "data", "-bd"], "");
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(fs::read_to_string(t.path().join("x.7z")).unwrap(), "舊檔");
    fs::remove_file(t.path().join("x.7z")).unwrap();

    // 既有壓縮檔：a 會加入新檔，d 會刪除項目
    assert!(run(t.path(), &["a", "x.7z", "data", "-bd"], "").status.success());
    fs::write(t.path().join("extra.txt"), "加入的檔案").unwrap();
    let o = run(t.path(), &["a", "x.7z", "extra.txt", "-bd"], "");
    assert!(o.status.success() && text(&o).contains("正在更新"), "{}", text(&o));
    let o = run(t.path(), &["d", "x.7z", "data/sub", "-bd"], "");
    assert!(o.status.success(), "{}", text(&o));
    let l = text(&run(t.path(), &["l", "x.7z"], ""));
    assert!(l.contains("extra.txt") && l.contains("data/a.txt") && !l.contains("data/sub/b.txt"), "{l}");

    // 刪除不存在的項目：明確報錯
    let o = run(t.path(), &["d", "x.7z", "沒有這個"], "");
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o).contains("沒有"), "{}", text(&o));
}

#[test]
fn overwrite_prompt_answers_and_switches() {
    let t = tempfile::tempdir().unwrap();
    tree(t.path());
    run(t.path(), &["a", "x.zip", "data", "-bd"], "");
    let a = t.path().join("o/data/a.txt");
    let b = t.path().join("o/data/sub/b.txt");
    let reset = || {
        fs::create_dir_all(b.parent().unwrap()).unwrap();
        fs::write(&a, "舊").unwrap();
        fs::write(&b, "舊").unwrap();
    };

    // 回答 n、y：第一個保留、第二個取代（順序依壓縮檔內的項目順序）
    reset();
    let o = run(t.path(), &["x", "x.zip", "-oo", "-bd"], "n\ny\n");
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("檔案已存在"));
    let changed = [&a, &b].iter().filter(|p| fs::read_to_string(p).unwrap() != "舊").count();
    assert_eq!(changed, 1);

    // 非互動執行、沒有回答（EOF）：全部略過，不會誤刪資料
    reset();
    let o = run(t.path(), &["x", "x.zip", "-oo", "-bd"], "");
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(fs::read_to_string(&a).unwrap(), "舊");
    assert_eq!(fs::read_to_string(&b).unwrap(), "舊");

    // -y：全部取代，不詢問
    reset();
    let o = run(t.path(), &["x", "x.zip", "-oo", "-bd", "-y"], "");
    assert!(o.status.success() && !text(&o).contains("檔案已存在"), "{}", text(&o));
    assert_ne!(fs::read_to_string(&a).unwrap(), "舊");

    // -aou：自動改名
    reset();
    let o = run(t.path(), &["x", "x.zip", "-oo", "-bd", "-aou"], "");
    assert!(o.status.success(), "{}", text(&o));
    assert!(t.path().join("o/data/a (1).txt").exists());

    // q：中止
    reset();
    let o = run(t.path(), &["x", "x.zip", "-oo", "-bd"], "q\n");
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
}

#[test]
fn skipped_items_give_warning_exit_code() {
    let t = tempfile::tempdir().unwrap();
    let mut z = zip::ZipWriter::new(fs::File::create(t.path().join("evil.zip")).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    z.start_file("../evil.txt", opts).unwrap();
    z.write_all(b"x").unwrap();
    z.start_file("good.txt", opts).unwrap();
    z.write_all(b"ok").unwrap();
    z.finish().unwrap();

    let o = run(t.path(), &["x", "evil.zip", "-oout", "-bd"], "");
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(text(&o).contains("警告"));
    assert!(t.path().join("out/good.txt").exists());
    assert!(!t.path().join("evil.txt").exists());
}

#[test]
fn zip_bomb_is_refused_unless_confirmed() {
    let t = tempfile::tempdir().unwrap();
    {
        let mut z = zip::ZipWriter::new(fs::File::create(t.path().join("bomb.zip")).unwrap());
        let opts = zip::write::SimpleFileOptions::default().compression_level(Some(1)).large_file(true);
        z.start_file("zeros.bin", opts).unwrap();
        let chunk = vec![0u8; 1 << 20];
        for _ in 0..1025 {
            z.write_all(&chunk).unwrap();
        }
        z.finish().unwrap();
    }
    let o = run(t.path(), &["x", "bomb.zip", "-oout", "-bd"], "");
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(text(&o).contains("壓縮炸彈"), "{}", text(&o));
    assert!(!t.path().join("out/zeros.bin").exists());
}

#[test]
fn usage_errors_exit_7() {
    let t = tempfile::tempdir().unwrap();
    assert_eq!(run(t.path(), &["q"], "").status.code(), Some(7));
    assert_eq!(run(t.path(), &["a", "x.7z", "-mx12"], "").status.code(), Some(7));
    assert_eq!(run(t.path(), &["a", "x.7z", "-zz"], "").status.code(), Some(7));
}

/// 惡意壓縮檔可以在檔名中放入終端機控制字元（清除畫面、移動游標、改視窗標題、反轉文字方向），
/// 偽造列出的內容或蓋掉警告。輸出前必須轉成看得見的跳脫形式。
#[test]
fn control_characters_in_names_are_escaped() {
    let tmp = tempfile::tempdir().unwrap();
    let ar = tmp.path().join("evil.zip");
    {
        let mut z = zip::ZipWriter::new(fs::File::create(&ar).unwrap());
        let o = zip::write::SimpleFileOptions::default();
        for name in ["\x1b[2J\x1b[32m一切正常", "a\rb.txt", "\x1b]0;PWNED\x07t.txt", "doc\u{202E}txt.exe", "c\u{9b}31m.txt", "../\x1b[1A\x1b[2Kx.txt"] {
            z.start_file(name, o).unwrap();
            z.write_all(b"x").unwrap();
        }
        z.finish().unwrap();
    }
    let bad = |s: &str| s.chars().any(|c| (c.is_control() && c != '\n') || c == '\u{202E}');
    let l = run(tmp.path(), &["l", "evil.zip"], "");
    assert!(!bad(&text(&l)), "{:?}", text(&l));
    assert!(text(&l).contains(r"\u{1b}[2J"), "{}", text(&l));
    let x = run(tmp.path(), &["x", "-y", "-oout", "evil.zip"], "");
    assert!(!bad(&text(&x)), "{:?}", text(&x));
}

/// h 指令：計算雜湊值（以 "abc" 的標準測試向量驗證）
#[test]
fn hash_command() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("abc.txt"), "abc").unwrap();
    let o = run(tmp.path(), &["h", "-bd", "abc.txt"], "");
    assert!(o.status.success(), "{}", text(&o));
    let t = text(&o);
    assert!(t.contains("SHA-256 ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"), "{t}");
    assert!(t.contains("MD5     900150983cd24fb0d6963f7d28e17f72"), "{t}");
    assert!(t.contains("CRC32   352441c2"), "{t}");
    assert_eq!(run(tmp.path(), &["h", "不存在.txt"], "").status.code(), Some(2));
}

/// 不是壓縮檔的分割檔（影片.mkv.001）：列出一個檔案，格式顯示為分割檔
#[test]
fn list_split_plain_file() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("clip.mkv.001"), b"\x1aE\xdf\xa3 part one ").unwrap();
    fs::write(tmp.path().join("clip.mkv.002"), b"part two").unwrap();
    let o = run(tmp.path(), &["l", "clip.mkv.001"], "");
    let t = text(&o);
    assert!(o.status.success() && t.contains("格式：分割檔") && t.contains("clip.mkv"), "{t}");
}

/// -v 分割壓縮，再從 .001 解壓縮
#[test]
fn split_volumes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut x = 7u32;
    let data: Vec<u8> = (0..200_000)
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (x >> 16) as u8
        })
        .collect();
    fs::write(tmp.path().join("big.bin"), &data).unwrap();
    let o = run(tmp.path(), &["a", "-bd", "-v50k", "-tzip", "v.zip", "big.bin"], "");
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("已分割為"), "{}", text(&o));
    assert!(tmp.path().join("v.zip.001").is_file() && tmp.path().join("v.zip.004").is_file());
    let o = run(tmp.path(), &["x", "-bd", "-y", "-oout", "v.zip.001"], "");
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(fs::read(tmp.path().join("out/big.bin")).unwrap(), data);
    assert_eq!(run(tmp.path(), &["a", "-v0", "w.zip", "big.bin"], "").status.code(), Some(7));
}
