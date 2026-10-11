use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ziptool_core::{CompressOptions, Error, ExtractOptions, Format, NoProgress, Overwrite, Progress, compress, extract, list, test, update};

fn x(ar: &Path, out: &Path, pw: Option<&str>) -> ziptool_core::Result<()> {
    let opts = ExtractOptions { password: pw.map(String::from), ..Default::default() };
    extract(ar, out, &opts, &NoProgress).map(|_| ())
}

/// 建立測試資料夾：src/a.txt、src/sub/b.bin、src/empty/
fn make_tree(root: &Path) -> PathBuf {
    let src = root.join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::create_dir_all(src.join("empty")).unwrap();
    fs::write(src.join("a.txt"), "你好，世界\n".repeat(1000)).unwrap();
    let bin: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
    fs::write(src.join("sub").join("b.bin"), bin).unwrap();
    src
}

fn assert_same_tree(a: &Path, b: &Path) {
    for e in walkdir::WalkDir::new(a) {
        let e = e.unwrap();
        let rel = e.path().strip_prefix(a).unwrap();
        let other = b.join(rel);
        if e.file_type().is_dir() {
            assert!(other.is_dir(), "缺少資料夾 {}", other.display());
        } else {
            assert_eq!(fs::read(e.path()).unwrap(), fs::read(&other).unwrap(), "{}", rel.display());
        }
    }
}

#[test]
fn archive_formats_roundtrip() {
    for f in Format::ALL.into_iter().filter(|f| !f.is_single()) {
        let tmp = tempfile::tempdir().unwrap();
        let src = make_tree(tmp.path());
        let ar = tmp.path().join(format!("out.{}", f.extension()));
        let opts = CompressOptions { level: Some(9), ..Default::default() };
        compress(std::slice::from_ref(&src), &ar, f, &opts, &NoProgress).unwrap_or_else(|e| panic!("{f:?}: {e}"));

        let entries = list(&ar, None).unwrap();
        assert!(entries.iter().any(|e| e.name == "src/sub/b.bin"), "{f:?}");

        let out = tmp.path().join("out");
        x(&ar, &out, None).unwrap_or_else(|e| panic!("{f:?}: {e}"));
        assert_same_tree(&src, &out.join("src"));
    }
}

#[test]
fn single_formats_roundtrip() {
    for f in Format::ALL.into_iter().filter(|f| f.is_single()) {
        let tmp = tempfile::tempdir().unwrap();
        let src = make_tree(tmp.path()).join("a.txt");
        let ar = tmp.path().join(format!("a.txt.{}", f.extension()));
        compress(std::slice::from_ref(&src), &ar, f, &CompressOptions::default(), &NoProgress).unwrap();
        let out = tmp.path().join("out");
        x(&ar, &out, None).unwrap();
        assert_eq!(fs::read(&src).unwrap(), fs::read(out.join("a.txt")).unwrap(), "{f:?}");
    }
}

#[test]
fn single_format_rejects_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let src = make_tree(tmp.path());
    let err = compress(&[src], &tmp.path().join("x.gz"), Format::Gz, &Default::default(), &NoProgress);
    assert!(matches!(err, Err(Error::Unsupported(_))));
}

#[test]
fn password_roundtrip_and_wrong_password() {
    for f in [Format::Zip, Format::SevenZ] {
        let tmp = tempfile::tempdir().unwrap();
        let src = make_tree(tmp.path());
        let ar = tmp.path().join(format!("enc.{}", f.extension()));
        let opts = CompressOptions { password: Some("秘密123".into()), ..Default::default() };
        compress(std::slice::from_ref(&src), &ar, f, &opts, &NoProgress).unwrap();

        let r = x(&ar, &tmp.path().join("none"), None);
        assert!(matches!(r, Err(Error::PasswordRequired)), "{f:?} 無密碼：{r:?}");

        let r = x(&ar, &tmp.path().join("bad"), Some("wrong"));
        assert!(matches!(r, Err(Error::WrongPassword)), "{f:?} 錯誤密碼：{r:?}");

        let out = tmp.path().join("ok");
        x(&ar, &out, Some("秘密123")).unwrap();
        assert_same_tree(&src, &out.join("src"));
    }
}

#[test]
fn password_rejected_for_tar() {
    let tmp = tempfile::tempdir().unwrap();
    let src = make_tree(tmp.path());
    let opts = CompressOptions { password: Some("x".into()), ..Default::default() };
    let r = compress(&[src], &tmp.path().join("x.tar.gz"), Format::TarGz, &opts, &NoProgress);
    assert!(matches!(r, Err(Error::Unsupported(_))));
}

struct Counter {
    total: AtomicU64,
    done: AtomicU64,
    cancel_after: u64,
    cancelled: AtomicBool,
}
impl Progress for Counter {
    fn start(&self, total: u64) {
        self.total.store(total, Ordering::SeqCst);
    }
    fn advance(&self, n: u64) {
        let d = self.done.fetch_add(n, Ordering::SeqCst) + n;
        if d >= self.cancel_after {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[test]
fn progress_and_cancel() {
    let tmp = tempfile::tempdir().unwrap();
    let src = make_tree(tmp.path());
    let ar = tmp.path().join("p.zip");

    let p = Counter { total: 0.into(), done: 0.into(), cancel_after: u64::MAX, cancelled: false.into() };
    compress(std::slice::from_ref(&src), &ar, Format::Zip, &Default::default(), &p).unwrap();
    assert!(p.total.load(Ordering::SeqCst) > 0);
    assert_eq!(p.total.load(Ordering::SeqCst), p.done.load(Ordering::SeqCst));

    let ar2 = tmp.path().join("c.zip");
    let p = Counter { total: 0.into(), done: 0.into(), cancel_after: 1000, cancelled: false.into() };
    let r = compress(&[src], &ar2, Format::Zip, &Default::default(), &p);
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(!ar2.exists(), "取消後應刪除未完成的輸出檔");
}

#[test]
fn zip_slip_is_blocked() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let ar = tmp.path().join("evil.zip");
    let mut z = zip::ZipWriter::new(fs::File::create(&ar).unwrap());
    z.start_file("../evil.txt", zip::write::SimpleFileOptions::default()).unwrap();
    z.write_all(b"pwned").unwrap();
    z.finish().unwrap();

    let out = tmp.path().join("out");
    let r = extract(&ar, &out, &ExtractOptions::default(), &NoProgress).unwrap();
    assert_eq!(r.issues.len(), 1, "不安全的項目應被略過並記錄");
    assert!(!tmp.path().join("evil.txt").exists());
}

#[test]
fn detect_by_magic_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let src = make_tree(tmp.path()).join("a.txt");
    let ar = tmp.path().join("a.txt.xz");
    compress(&[src], &ar, Format::Xz, &Default::default(), &NoProgress).unwrap();
    let renamed = tmp.path().join("noext");
    fs::rename(&ar, &renamed).unwrap();
    assert_eq!(Format::detect(&renamed).unwrap(), Format::Xz);
}

fn archive_of(tmp: &Path, f: Format, opts: &CompressOptions) -> (PathBuf, PathBuf) {
    let src = make_tree(tmp);
    let ar = tmp.join(format!("t.{}", f.extension()));
    compress(std::slice::from_ref(&src), &ar, f, opts, &NoProgress).unwrap();
    (src, ar)
}

#[test]
fn extract_selection_strip_and_flatten() {
    for f in [Format::Zip, Format::SevenZ, Format::TarXz] {
        let tmp = tempfile::tempdir().unwrap();
        let (src, ar) = archive_of(tmp.path(), f, &Default::default());

        // 只解 src/sub，並去掉 src/ 前綴
        let out = tmp.path().join("sel");
        let opts = ExtractOptions { selection: Some(vec!["src/sub".into()]), strip_prefix: "src".into(), ..Default::default() };
        extract(&ar, &out, &opts, &NoProgress).unwrap();
        assert_eq!(fs::read(out.join("sub/b.bin")).unwrap(), fs::read(src.join("sub/b.bin")).unwrap(), "{f:?}");
        assert!(!out.join("a.txt").exists(), "{f:?} 未選取的檔案不應解出");
        assert!(!out.join("src").exists(), "{f:?} 前綴應去掉");

        // 不含路徑
        let out = tmp.path().join("flat");
        let opts = ExtractOptions { flatten: true, ..Default::default() };
        extract(&ar, &out, &opts, &NoProgress).unwrap();
        assert!(out.join("a.txt").is_file() && out.join("b.bin").is_file(), "{f:?}");
        assert!(!out.join("src").exists(), "{f:?}");
    }
}

#[test]
fn overwrite_modes() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, ar) = archive_of(tmp.path(), Format::Zip, &Default::default());
    let out = tmp.path().join("o");
    let target = out.join("src/a.txt");
    fs::create_dir_all(target.parent().unwrap()).unwrap();

    fs::write(&target, "舊內容").unwrap();
    extract(&ar, &out, &ExtractOptions { overwrite: Overwrite::Skip, ..Default::default() }, &NoProgress).unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "舊內容");

    extract(&ar, &out, &ExtractOptions { overwrite: Overwrite::Rename, ..Default::default() }, &NoProgress).unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "舊內容");
    assert_eq!(fs::read(out.join("src/a (1).txt")).unwrap(), fs::read(src.join("a.txt")).unwrap());

    extract(&ar, &out, &ExtractOptions::default(), &NoProgress).unwrap();
    assert_eq!(fs::read(&target).unwrap(), fs::read(src.join("a.txt")).unwrap());
}

#[test]
fn test_detects_corruption() {
    for f in [Format::Zip, Format::SevenZ, Format::TarGz, Format::Xz] {
        let tmp = tempfile::tempdir().unwrap();
        let src = make_tree(tmp.path());
        let input = if f.is_single() { src.join("sub/b.bin") } else { src };
        let ar = tmp.path().join(format!("t.{}", f.extension()));
        compress(&[input], &ar, f, &Default::default(), &NoProgress).unwrap();

        let r = test(&ar, None, &NoProgress).unwrap_or_else(|e| panic!("{f:?}: {e}"));
        assert!(r.files >= 1 && r.bytes >= 200_000, "{f:?}: {r:?}");

        // 破壞中段資料
        let mut bytes = fs::read(&ar).unwrap();
        let mid = bytes.len() / 2;
        for b in &mut bytes[mid..mid + 64] {
            *b ^= 0x5A;
        }
        fs::write(&ar, bytes).unwrap();
        assert!(test(&ar, None, &NoProgress).is_err(), "{f:?} 損壞的檔案應測試失敗");
    }
}

#[test]
fn modified_time_preserved() {
    use std::time::{Duration, SystemTime};
    for f in [Format::Zip, Format::SevenZ, Format::Tar] {
        let tmp = tempfile::tempdir().unwrap();
        let src = make_tree(tmp.path());
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        fs::File::options().write(true).open(src.join("a.txt")).unwrap().set_modified(when).unwrap();
        let ar = tmp.path().join(format!("t.{}", f.extension()));
        compress(&[src], &ar, f, &Default::default(), &NoProgress).unwrap();

        let listed = list(&ar, None).unwrap().into_iter().find(|e| e.name == "src/a.txt").unwrap();
        let diff = |t: SystemTime| t.duration_since(when).unwrap_or_else(|e| e.duration()).as_secs();
        assert!(diff(listed.modified.unwrap()) <= 2, "{f:?} 清單時間");

        let out = tmp.path().join("out");
        x(&ar, &out, None).unwrap();
        let got = fs::metadata(out.join("src/a.txt")).unwrap().modified().unwrap();
        assert!(diff(got) <= 2, "{f:?} 解壓後時間");
    }
}

#[test]
fn sevenz_encrypt_names() {
    let tmp = tempfile::tempdir().unwrap();
    let pw = Some("pw".to_string());

    let (_, plain_hdr) = archive_of(tmp.path(), Format::SevenZ, &CompressOptions { password: pw.clone(), ..Default::default() });
    let entries = list(&plain_hdr, None).unwrap();
    assert!(entries.iter().filter(|e| !e.is_dir).all(|e| e.encrypted), "內容加密的項目應標示加密");

    let hidden = tmp.path().join("hidden.7z");
    let opts = CompressOptions { password: pw, encrypt_names: true, ..Default::default() };
    compress(&[tmp.path().join("src")], &hidden, Format::SevenZ, &opts, &NoProgress).unwrap();
    assert!(matches!(list(&hidden, None), Err(Error::PasswordRequired)));
    assert!(list(&hidden, Some("pw")).unwrap().iter().any(|e| e.name == "src/a.txt"));

    let r = compress(&[tmp.path().join("src")], &tmp.path().join("z.zip"), Format::Zip,
        &CompressOptions { password: Some("p".into()), encrypt_names: true, ..Default::default() }, &NoProgress);
    assert!(matches!(r, Err(Error::Unsupported(_))));
}

#[test]
fn absurd_timestamps_do_not_panic() {
    use std::time::{Duration, SystemTime};
    let tmp = tempfile::tempdir().unwrap();
    let ar = tmp.path().join("weird.tar");
    let mut b = tar::Builder::new(fs::File::create(&ar).unwrap());
    let mut h = tar::Header::new_gnu();
    h.set_size(3);
    h.set_mode(0o644);
    h.set_mtime(u64::MAX / 2); // 遠超過 9999 年
    h.set_cksum();
    b.append_data(&mut h, "x.txt", &b"abc"[..]).unwrap();
    b.finish().unwrap();
    drop(b);

    let entries = list(&ar, None).unwrap();
    assert_eq!(entries.len(), 1);
    if let Some(t) = entries[0].modified {
        let _ = ziptool_core::local_time(t);
    }
    // SystemTime 能表示、但超過 9999 年的時間，轉換時應回傳 None 而不是 panic
    let far = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(600_000_000_000)).unwrap();
    assert_eq!(ziptool_core::local_time(far), None);
    assert!(ziptool_core::local_time(SystemTime::now()).is_some());
    x(&ar, &tmp.path().join("out"), None).unwrap();
}

// ---------- 第一階段：安全與穩定 ----------

fn zip_with(path: &Path, files: &[(&str, &[u8])]) {
    use std::io::Write;
    let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
    for (name, data) in files {
        z.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(data).unwrap();
    }
    z.finish().unwrap();
}

#[cfg(windows)]
#[test]
fn windows_unsafe_names_are_sanitized() {
    let tmp = tempfile::tempdir().unwrap();
    let ar = tmp.path().join("names.zip");
    zip_with(&ar, &[("normal.txt", b"a"), ("normal.txt:hidden", b"ADS"), ("CON.txt", b"c"), ("what?.txt", b"q"), ("dots. ", b"d")]);
    let out = tmp.path().join("out");
    let r = extract(&ar, &out, &ExtractOptions::default(), &NoProgress).unwrap();
    assert!(r.issues.is_empty(), "{:?}", r.issues);
    let mut names: Vec<String> = fs::read_dir(&out).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, ["_CON.txt", "dots_", "normal.txt", "normal.txt_hidden", "what_.txt"]);
    // normal.txt 不應被塞進隱藏的替代資料流
    assert!(fs::read(out.join("normal.txt:hidden")).is_err());
    assert_eq!(fs::read(out.join("normal.txt_hidden")).unwrap(), b"ADS");
}

#[test]
fn tar_symlinks_are_not_created_and_do_not_abort() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let ar = tmp.path().join("link.tar");
    {
        let mut b = tar::Builder::new(fs::File::create(&ar).unwrap());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(&mut h, "lnk", &outside).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_size(5);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, "lnk/evil.txt", &b"pwned"[..]).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_size(2);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, "ok.txt", &b"ok"[..]).unwrap();
        b.finish().unwrap();
    }
    let out = tmp.path().join("out");
    let r = extract(&ar, &out, &ExtractOptions::default(), &NoProgress).unwrap();
    assert!(r.issues.iter().any(|i| i.name == "lnk"), "{:?}", r.issues);
    assert!(fs::read_dir(&outside).unwrap().next().is_none(), "不可寫到目標資料夾之外");
    assert!(fs::symlink_metadata(out.join("lnk")).is_ok_and(|m| !m.file_type().is_symlink()));
    assert_eq!(fs::read(out.join("ok.txt")).unwrap(), b"ok", "其他項目仍要正常解出");
}

/// 目標資料夾裡事先存在、指向外部的連結，也不能被拿來逃出資料夾。
#[test]
fn existing_symlink_in_destination_is_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let out = tmp.path().join("out");
    fs::create_dir(&out).unwrap();
    // 符號連結需要權限；改用不需權限的目錄接合點（junction），效果相同
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_dir(&outside, out.join("lnk")).is_ok()
        || std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(out.join("lnk"))
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&outside, out.join("lnk")).is_ok();
    if !made {
        eprintln!("此環境無法建立符號連結，略過測試");
        return;
    }
    let ar = tmp.path().join("a.zip");
    zip_with(&ar, &[("lnk/evil.txt", b"pwned")]);
    let r = extract(&ar, &out, &ExtractOptions::default(), &NoProgress).unwrap();
    assert_eq!(r.issues.len(), 1);
    assert!(fs::read_dir(&outside).unwrap().next().is_none());
}

#[cfg(windows)]
#[test]
fn mark_of_the_web_is_propagated() {
    let tmp = tempfile::tempdir().unwrap();
    let ar = tmp.path().join("dl.zip");
    zip_with(&ar, &[("doc.txt", b"hello")]);
    let ads = |p: &Path| format!("{}:Zone.Identifier", p.display());

    // 來自網際網路（ZoneId=3）→ 傳遞
    let zone = "[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=https://example.com/dl.zip\r\n";
    fs::write(ads(&ar), zone).unwrap();
    let out = tmp.path().join("web");
    extract(&ar, &out, &ExtractOptions::default(), &NoProgress).unwrap();
    assert_eq!(fs::read_to_string(ads(&out.join("doc.txt"))).unwrap(), zone);

    // 本機內部網路（ZoneId=1）→ 不傳遞
    fs::write(ads(&ar), "[ZoneTransfer]\r\nZoneId=1\r\n").unwrap();
    let out = tmp.path().join("local");
    extract(&ar, &out, &ExtractOptions::default(), &NoProgress).unwrap();
    assert!(fs::read(ads(&out.join("doc.txt"))).is_err());
}

#[test]
fn item_errors_do_not_abort_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, ar) = archive_of(tmp.path(), Format::SevenZ, &Default::default());
    let out = tmp.path().join("out");
    // 讓 src/a.txt 的位置已經是資料夾，寫入必定失敗
    fs::create_dir_all(out.join("src").join("a.txt")).unwrap();
    let r = extract(&ar, &out, &ExtractOptions::default(), &NoProgress).unwrap();
    assert_eq!(r.issues.len(), 1, "{:?}", r.issues);
    assert_eq!(r.issues[0].name, "src/a.txt");
    assert_eq!(fs::read(out.join("src/sub/b.bin")).unwrap(), fs::read(src.join("sub/b.bin")).unwrap());
}

/// 依序回答事先排好的答案。
struct Answers(std::sync::Mutex<Vec<ziptool_core::OverwriteAnswer>>, AtomicU64);
impl Progress for Answers {
    fn ask_overwrite(&self, _: &Path) -> ziptool_core::OverwriteAnswer {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.lock().unwrap().remove(0)
    }
}

#[test]
fn overwrite_ask_answers() {
    use ziptool_core::OverwriteAnswer::*;
    let tmp = tempfile::tempdir().unwrap();
    let ar = tmp.path().join("q.zip");
    zip_with(&ar, &[("1.txt", b"new1"), ("2.txt", b"new2"), ("3.txt", b"new3"), ("4.txt", b"new4")]);
    let out = tmp.path().join("out");
    fs::create_dir(&out).unwrap();
    for i in 1..=4 {
        fs::write(out.join(format!("{i}.txt")), "old").unwrap();
    }
    let ask = ExtractOptions { overwrite: Overwrite::Ask, ..Default::default() };

    // 第 1 個略過、第 2 個起全部取代（之後不再詢問）
    let p = Answers(vec![No, YesAll].into(), 0.into());
    extract(&ar, &out, &ask, &p).unwrap();
    assert_eq!(p.1.load(Ordering::SeqCst), 2);
    let read = |i: u32| fs::read_to_string(out.join(format!("{i}.txt"))).unwrap();
    assert_eq!([read(1), read(2), read(3), read(4)], ["old", "new2", "new3", "new4"]);

    // 全部自動重新命名
    let p = Answers(vec![RenameAll].into(), 0.into());
    extract(&ar, &out, &ask, &p).unwrap();
    assert_eq!(fs::read_to_string(out.join("1 (1).txt")).unwrap(), "new1");
    assert_eq!(fs::read_to_string(out.join("4 (1).txt")).unwrap(), "new4");

    // 取消：中止整個工作
    let p = Answers(vec![Cancel].into(), 0.into());
    assert!(matches!(extract(&ar, &out, &ask, &p), Err(Error::Cancelled)));
}

/// 取消或中途失敗時，不留下寫到一半的檔案，也不破壞原本的同名檔。
#[test]
fn cancel_leaves_no_partial_files() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, ar) = archive_of(tmp.path(), Format::Zip, &Default::default());

    // 解壓縮中途取消
    let out = tmp.path().join("out");
    let p = Counter { total: 0.into(), done: 0.into(), cancel_after: 100_000, cancelled: false.into() };
    let r = extract(&ar, &out, &ExtractOptions::default(), &p);
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    for e in walkdir::WalkDir::new(&out) {
        let name = e.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!name.ends_with(".ziptool-part"), "殘留暫存檔：{name}");
        assert_ne!(name, "b.bin", "寫到一半的檔案不應出現");
    }

    // 壓縮中途取消：原本的同名壓縮檔保持不變
    let existing = tmp.path().join("keep.zip");
    fs::write(&existing, "原本的內容").unwrap();
    let p = Counter { total: 0.into(), done: 0.into(), cancel_after: 1000, cancelled: false.into() };
    let r = compress(&[tmp.path().join("src")], &existing, Format::Zip, &Default::default(), &p);
    assert!(matches!(r, Err(Error::Cancelled)));
    assert_eq!(fs::read_to_string(&existing).unwrap(), "原本的內容");
    assert!(!tmp.path().join("keep.zip.ziptool-part").exists());
}

#[test]
fn sevenz_solid_many_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("many");
    for d in 0..5 {
        let dir = src.join(format!("d{d}"));
        fs::create_dir_all(&dir).unwrap();
        for i in 0..300 {
            fs::write(dir.join(format!("f{i:03}.txt")), format!("設定 {d}-{i} value={} enabled=true\n", i * 7).repeat(i % 5 + 1)).unwrap();
        }
    }
    fs::write(src.join("empty.txt"), "").unwrap();
    fs::create_dir_all(src.join("emptydir")).unwrap();

    let ar7 = tmp.path().join("m.7z");
    let arz = tmp.path().join("m.zip");
    compress(std::slice::from_ref(&src), &ar7, Format::SevenZ, &Default::default(), &NoProgress).unwrap();
    compress(std::slice::from_ref(&src), &arz, Format::Zip, &Default::default(), &NoProgress).unwrap();
    let (s7, sz) = (fs::metadata(&ar7).unwrap().len(), fs::metadata(&arz).unwrap().len());
    assert!(s7 * 4 < sz, "固實 7z（{s7}）應明顯小於 zip（{sz}）");

    // 完整解出
    let out = tmp.path().join("all");
    x(&ar7, &out, None).unwrap();
    assert_same_tree(&src, &out.join("many"));

    // 只解出中間的一個檔案：前面的資料要正確讀過去
    let out = tmp.path().join("one");
    let opts = ExtractOptions { selection: Some(vec!["many/d3/f150.txt".into()]), ..Default::default() };
    let r = extract(&ar7, &out, &opts, &NoProgress).unwrap();
    assert_eq!(r.files, 1);
    assert_eq!(fs::read(out.join("many/d3/f150.txt")).unwrap(), fs::read(src.join("d3/f150.txt")).unwrap());

    let r = test(&ar7, None, &NoProgress).unwrap();
    assert_eq!(r.files, 1501);
}

/// 多執行緒 xz、zstd 的輸出仍是標準格式，且內容正確。
#[test]
fn multithreaded_stream_formats_roundtrip() {
    for f in [Format::TarXz, Format::TarZst, Format::Xz, Format::Zst] {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("big.txt");
        let text: String = (0..300_000).map(|i| format!("{i}\n")).collect();
        fs::write(&src, &text).unwrap();
        let ar = tmp.path().join(format!("big.txt.{}", f.extension()));
        compress(std::slice::from_ref(&src), &ar, f, &CompressOptions { level: Some(9), ..Default::default() }, &NoProgress).unwrap();
        let out = tmp.path().join("out");
        x(&ar, &out, None).unwrap();
        assert_eq!(fs::read(out.join("big.txt")).unwrap(), text.as_bytes(), "{f:?}");
    }
}

/// 模擬 Windows 內建壓縮在繁體中文系統上產生的 zip：檔名以 Big5 儲存，沒有 UTF-8 標記。
#[test]
fn legacy_big5_zip_names() {
    let tmp = tempfile::tempdir().unwrap();
    let ar = tmp.path().join("big5.zip");
    // 先用 ASCII 佔位名稱建立，再把檔名位元組換成 Big5 的「測試」（長度同為 4 位元組）
    zip_with(&ar, &[("AAAA.txt", b"big5 content"), ("dir/BBBB.txt", b"x")]);
    let mut bytes = fs::read(&ar).unwrap();
    let big5_test = [0xB4u8, 0xFA, 0xB8, 0xD5]; // 測試
    let big5_doc = [0xA4u8, 0xE5, 0xA5, 0xF3]; // 文件
    let mut replace = |from: &[u8], to: &[u8]| {
        let mut n = 0;
        let mut i = 0;
        while i + from.len() <= bytes.len() {
            if &bytes[i..i + from.len()] == from {
                bytes[i..i + from.len()].copy_from_slice(to);
                n += 1;
            }
            i += 1;
        }
        assert_eq!(n, 2, "本機標頭與中央目錄各一次");
    };
    replace(b"AAAA", &big5_test);
    replace(b"BBBB", &big5_doc);
    fs::write(&ar, bytes).unwrap();

    ziptool_core::set_zip_codepage(Some(950));
    let names: Vec<String> = list(&ar, None).unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.contains(&"測試.txt".to_string()) && names.contains(&"dir/文件.txt".to_string()), "{names:?}");

    let out = tmp.path().join("out");
    x(&ar, &out, None).unwrap();
    assert_eq!(fs::read(out.join("測試.txt")).unwrap(), b"big5 content");
    assert!(out.join("dir").join("文件.txt").exists());

    // 選取解壓也要用同樣的名稱比對
    let out = tmp.path().join("sel");
    let opts = ExtractOptions { selection: Some(vec!["dir/文件.txt".into()]), ..Default::default() };
    assert_eq!(extract(&ar, &out, &opts, &NoProgress).unwrap().files, 1);
    ziptool_core::set_zip_codepage(None);
}

#[test]
fn errors_are_in_chinese() {
    let tmp = tempfile::tempdir().unwrap();
    let bad = tmp.path().join("bad.zip");
    fs::write(&bad, b"PK\x03\x04 not really a zip").unwrap();
    let msg = list(&bad, None).unwrap_err().to_string();
    assert!(msg.contains("不是有效的 ZIP"), "{msg}");

    let msg = list(&tmp.path().join("nope.zip"), None).unwrap_err().to_string();
    assert!(msg.contains("找不到"), "{msg}");

    // 截斷的 gz：解壓時應說明檔案不完整或損壞
    let src = tmp.path().join("a.txt");
    fs::write(&src, "x".repeat(100_000)).unwrap();
    let gz = tmp.path().join("a.txt.gz");
    compress(&[src], &gz, Format::Gz, &Default::default(), &NoProgress).unwrap();
    let data = fs::read(&gz).unwrap();
    fs::write(&gz, &data[..data.len() / 2]).unwrap();
    let msg = x(&gz, &tmp.path().join("o"), None).unwrap_err().to_string();
    assert!(msg.contains("不完整") || msg.contains("損壞"), "{msg}");
}

/// 簡易模糊測試：對各格式的正常壓縮檔做大量隨機破壞（翻轉位元組、截斷），
/// 確認列出、測試、解壓縮都只會回傳錯誤，而不會 panic。
#[test]
fn corrupted_archives_never_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let src = make_tree(tmp.path());
    let mut seed: u64 = 0x9E3779B97F4A7C15;
    let mut rnd = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n.max(1) as u64) as usize
    };
    let formats = [Format::Zip, Format::SevenZ, Format::TarGz, Format::TarXz, Format::TarZst, Format::TarBz2, Format::Gz];
    let mut panics = Vec::new();
    for f in formats {
        let input = if f.is_single() { src.join("a.txt") } else { src.clone() };
        let good = tmp.path().join(format!("good.{}", f.extension()));
        let opts = if f.supports_password() && rnd(2) == 0 {
            CompressOptions { password: Some("pw".into()), ..Default::default() }
        } else {
            Default::default()
        };
        compress(&[input], &good, f, &opts, &NoProgress).unwrap();
        let original = fs::read(&good).unwrap();
        // 預設 120 次；需要更徹底時可設定 ZIPTOOL_FUZZ_ITERS
        let iters: usize = std::env::var("ZIPTOOL_FUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(120);
        for k in 0..iters {
            let mut data = original.clone();
            match rnd(3) {
                0 => {
                    let n = rnd(data.len()).max(1);
                    data.truncate(n);
                }
                _ => {
                    for _ in 0..=rnd(8) {
                        let i = rnd(data.len());
                        data[i] ^= (rnd(255) + 1) as u8;
                    }
                }
            }
            let bad = tmp.path().join(format!("bad{k}.{}", f.extension()));
            fs::write(&bad, &data).unwrap();
            let out = tmp.path().join(format!("out{k}"));
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = list(&bad, Some("pw"));
                let _ = test(&bad, Some("pw"), &NoProgress);
                let o = ExtractOptions { password: Some("pw".into()), ..Default::default() };
                let _ = extract(&bad, &out, &o, &NoProgress);
            }));
            if r.is_err() {
                panics.push(format!("{f:?} 第 {k} 個"));
            }
            let _ = fs::remove_dir_all(&out);
            let _ = fs::remove_file(&bad);
        }
    }
    assert!(panics.is_empty(), "以下損壞檔案造成 panic：{panics:?}");
}

// ---------- 更新既有壓縮檔 ----------

fn names_of(ar: &Path, pw: Option<&str>) -> Vec<String> {
    let mut v: Vec<String> = list(ar, pw).unwrap().into_iter().filter(|e| !e.is_dir).map(|e| e.name).collect();
    v.sort();
    v
}

#[test]
fn update_add_replace_remove() {
    for f in [Format::Zip, Format::SevenZ, Format::TarGz] {
        let tmp = tempfile::tempdir().unwrap();
        let (_, ar) = archive_of(tmp.path(), f, &Default::default());
        let new = tmp.path().join("new.txt");
        fs::write(&new, "新檔案").unwrap();
        let replace = tmp.path().join("a.txt");
        fs::write(&replace, "取代後的 a").unwrap();

        // 加到根目錄、加到 src/ 底下（取代同名的 a.txt），並刪除 src/sub 資料夾
        update(&ar, std::slice::from_ref(&new), "", &[], &Default::default(), &NoProgress).unwrap_or_else(|e| panic!("{f:?}: {e}"));
        update(&ar, std::slice::from_ref(&replace), "src", &["src/sub".into()], &Default::default(), &NoProgress).unwrap();
        assert_eq!(names_of(&ar, None), ["new.txt", "src/a.txt"], "{f:?}");

        let out = tmp.path().join("out");
        x(&ar, &out, None).unwrap();
        assert_eq!(fs::read_to_string(out.join("src/a.txt")).unwrap(), "取代後的 a", "{f:?}");
        assert_eq!(fs::read_to_string(out.join("new.txt")).unwrap(), "新檔案");
        assert!(!out.join("src/sub").exists());
        assert!(!tmp.path().join(format!("t.{}.ziptool-update", f.extension())).exists(), "暫存資料夾應清除");
    }
}

#[test]
fn update_zip_keeps_encrypted_entries_without_password() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, ar) = archive_of(tmp.path(), Format::Zip, &CompressOptions { password: Some("pw".into()), ..Default::default() });
    let new = tmp.path().join("plain.txt");
    fs::write(&new, "未加密").unwrap();
    // 不提供密碼也能更新：原有項目直接複製
    update(&ar, &[new], "", &[], &Default::default(), &NoProgress).unwrap();
    let entries = list(&ar, None).unwrap();
    assert!(entries.iter().find(|e| e.name == "src/a.txt").unwrap().encrypted);
    assert!(!entries.iter().find(|e| e.name == "plain.txt").unwrap().encrypted);
    let out = tmp.path().join("out");
    x(&ar, &out, Some("pw")).unwrap();
    assert_eq!(fs::read(out.join("src/a.txt")).unwrap(), fs::read(src.join("a.txt")).unwrap());
}

#[test]
fn update_7z_keeps_encrypted_names() {
    let tmp = tempfile::tempdir().unwrap();
    let opts = CompressOptions { password: Some("pw".into()), encrypt_names: true, ..Default::default() };
    let (_, ar) = archive_of(tmp.path(), Format::SevenZ, &opts);
    let new = tmp.path().join("n.txt");
    fs::write(&new, "x").unwrap();
    let upd = CompressOptions { password: Some("pw".into()), ..Default::default() };
    update(&ar, &[new], "", &["src/a.txt".into()], &upd, &NoProgress).unwrap();
    assert!(matches!(list(&ar, None), Err(Error::PasswordRequired)), "更新後仍應加密檔名");
    assert_eq!(names_of(&ar, Some("pw")), ["n.txt", "src/sub/b.bin"]);
}

#[test]
fn update_rejects_single_file_formats() {
    let tmp = tempfile::tempdir().unwrap();
    let src = make_tree(tmp.path()).join("a.txt");
    let gz = tmp.path().join("a.txt.gz");
    compress(std::slice::from_ref(&src), &gz, Format::Gz, &Default::default(), &NoProgress).unwrap();
    assert!(matches!(update(&gz, &[src], "", &[], &Default::default(), &NoProgress), Err(Error::Unsupported(_))));
}

#[test]
fn cancelled_update_leaves_archive_untouched() {
    for f in [Format::Zip, Format::SevenZ] {
        let tmp = tempfile::tempdir().unwrap();
        let (src, ar) = archive_of(tmp.path(), f, &Default::default());
        let before = fs::read(&ar).unwrap();
        let p = Counter { total: 0.into(), done: 0.into(), cancel_after: 1000, cancelled: false.into() };
        let r = update(&ar, &[src.join("sub")], "more", &[], &Default::default(), &p);
        assert!(matches!(r, Err(Error::Cancelled)), "{f:?}: {r:?}");
        assert_eq!(fs::read(&ar).unwrap(), before, "{f:?}：原壓縮檔不可被改動");
        let leftovers: Vec<_> = fs::read_dir(tmp.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.contains("ziptool")).collect();
        assert!(leftovers.is_empty(), "{f:?} 殘留：{leftovers:?}");
    }
}

/// 平行壓縮的 zip：項目順序與逐一壓縮相同，內容與加密都正確。
#[test]
fn parallel_zip_many_files() {
    for pw in [None, Some("pw".to_string())] {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("many");
        let mut expected = vec!["many".to_string()];
        for d in 0..4 {
            let dir = src.join(format!("d{d}"));
            fs::create_dir_all(&dir).unwrap();
            expected.push(format!("many/d{d}"));
            for i in 0..150 {
                fs::write(dir.join(format!("f{i:03}.txt")), format!("{d}-{i} ").repeat(200 + i)).unwrap();
                expected.push(format!("many/d{d}/f{i:03}.txt"));
            }
        }
        let ar = tmp.path().join("p.zip");
        let opts = CompressOptions { password: pw.clone(), ..Default::default() };
        compress(std::slice::from_ref(&src), &ar, Format::Zip, &opts, &NoProgress).unwrap();

        let entries = list(&ar, None).unwrap();
        let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, expected, "項目順序應與檔案順序一致");
        assert_eq!(entries.iter().filter(|e| e.encrypted).count(), if pw.is_some() { 600 } else { 0 });

        let out = tmp.path().join("out");
        x(&ar, &out, pw.as_deref()).unwrap();
        assert_same_tree(&src, &out.join("many"));
    }
}

/// 超過 Windows 傳統 260 字元限制的路徑也能壓縮與解壓縮。
#[test]
fn long_paths_work() {
    let tmp = tempfile::tempdir().unwrap();
    let seg = "很長的資料夾名稱_long_folder_name_0123456789";
    let mut deep = tmp.path().join("src");
    while deep.as_os_str().len() < 400 {
        deep.push(seg);
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("深層檔案.txt"), "deep").unwrap();
    assert!(deep.join("深層檔案.txt").as_os_str().len() > 260);

    for f in [Format::Zip, Format::SevenZ, Format::TarZst] {
        let ar = tmp.path().join(format!("long.{}", f.extension()));
        compress(&[tmp.path().join("src")], &ar, f, &Default::default(), &NoProgress).unwrap_or_else(|e| panic!("{f:?}: {e}"));
        let out = tmp.path().join(format!("out_{}", f.extension().replace('.', "_")));
        let r = extract(&ar, &out, &ExtractOptions::default(), &NoProgress).unwrap();
        assert!(r.issues.is_empty(), "{f:?}: {:?}", r.issues);
        let rel = deep.strip_prefix(tmp.path()).unwrap();
        assert_eq!(fs::read_to_string(out.join(rel).join("深層檔案.txt")).unwrap(), "deep", "{f:?}");
    }
}

/// tar 中的中文檔名要寫在 PAX 擴充標頭（規格上一律是 UTF-8）。只寫在 GNU 標頭時，
/// Windows 內建的 tar.exe 與檔案總管（libarchive）會用系統字碼頁解讀，讀不出檔名。
#[test]
fn tar_non_ascii_names_use_pax_headers() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("中文資料夾");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("中文檔名.txt"), "內容").unwrap();
    let en = tmp.path().join("en");
    fs::create_dir_all(&en).unwrap();
    fs::write(en.join("plain.txt"), "ascii").unwrap();
    let ar = tmp.path().join("t.tar");
    compress(&[src.clone(), en], &ar, Format::Tar, &CompressOptions::default(), &NoProgress).unwrap();

    // 每個中文名稱都有對應的 PAX path 記錄
    let raw = fs::read(&ar).unwrap();
    let has = |rec: &str| raw.windows(rec.len()).any(|w| w == rec.as_bytes());
    assert!(has(" path=中文資料夾/中文檔名.txt\n"), "缺少檔案的 PAX path 記錄");
    assert!(has(" path=中文資料夾"), "缺少資料夾的 PAX path 記錄");
    // 純英文檔名不需要額外的標頭
    assert!(!has(" path=en"));

    // 自己解得開
    let out = tmp.path().join("out");
    x(&ar, &out, None).unwrap();
    assert_eq!(fs::read_to_string(out.join("中文資料夾/中文檔名.txt")).unwrap(), "內容");

    // Windows 內建的 tar.exe 也解得開
    #[cfg(windows)]
    {
        let bsdtar = Path::new(&std::env::var("SystemRoot").unwrap()).join(r"System32\tar.exe");
        if bsdtar.is_file() {
            let out2 = tmp.path().join("bsdtar");
            fs::create_dir(&out2).unwrap();
            let st = std::process::Command::new(&bsdtar).arg("-xf").arg(&ar).arg("-C").arg(&out2).output().unwrap();
            assert!(st.status.success(), "tar.exe 失敗：{}", String::from_utf8_lossy(&st.stderr));
            assert_eq!(fs::read_to_string(out2.join("中文資料夾/中文檔名.txt")).unwrap(), "內容");
        }
    }
}
