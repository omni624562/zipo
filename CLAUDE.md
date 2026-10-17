# Zipo 開發說明（給 Claude Code）

以 7-Zip 為藍本、用 Rust 撰寫的壓縮工具。公開於 github.com/omni624562/zipo（本機資料夾與 crate 名稱仍是 ziptool）。

| crate | 執行檔 | 內容 |
|---|---|---|
| `crates/ziptool-core`（套件名 `ziptool-core`） | — | 各格式的壓縮、解壓縮、更新、篩選 |
| `crates/ziptool-cli`（套件名 `ziptool`） | `zipo` | 命令列，語法與 7-Zip 的 `7z.exe` 相同 |
| `crates/ziptool-gui`（套件名 `ziptool-gui`） | `zipo-gui` | egui 圖形介面（Windows 為主） |

## 協作規則

- 一律使用台灣繁體中文與台灣用語（程式註解、提交訊息、回覆皆同），不可用中國用語或簡體字；找不到合適的中文時可用英文原詞。
- 只改被要求的部分；發現可改善之處列出來讓使用者決定，不自行動手。一個任務一個 commit。
- 修正前先寫能重現問題的測試；修好後做突變檢查（暫時拿掉修正，確認測試會失敗）。
- 推送標籤（發布）、送 winget PR 等對外動作要先問使用者。

## 介面文字

- 使用者看得到的文字都要有中英兩種：`L!("中文", "English")`、`F!("中文 {x}", "English {x}", ...)`（`ziptool_core::i18n`）。
- 命令列依 `ZIPO_LANG=zh|en` 或系統語言；CLI 的端對端測試比對中文訊息，執行時設 `ZIPO_LANG=zh`。

## 建置與測試

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --release -- -D warnings
```

- Linux 需先安裝 GUI 相依套件：`sudo apt-get install -y libxkbcommon-dev libgtk-3-dev`。
- 很多 GUI 與系統整合功能只在 Windows 編譯（`#[cfg(windows)]`：右鍵選單、剪貼簿、資源回收筒、工作列進度），Linux 上的測試涵蓋不到；以 CI 的 Windows 結果為準。
- CI（`.github/workflows/ci.yml`）在每次推送執行：Windows x64、32 位元（i686）、Arm64（windows-11-arm）、Linux、macOS 的測試與 clippy、release 設定的 clippy、RustSec 安全檢查、winget 上架檔案檢查，以及 GUI 截圖。
- CI 用最新 stable Rust，新版 clippy 規則可能比本機嚴格。

## GUI 的驗證方式

- **單元測試**：用 `egui::Context` 直接跑畫面。要畫文字的測試先呼叫 `theme::setup(&ctx)`；修飾鍵用 `egui::Event::ModifiersChanged` 送出（`RawInput` 沒有 modifiers 欄位）；`ctx.enable_accesskit()` 後可從 `platform_output.accesskit_update` 找元件位置；每次執行後 `out.textures_delta.clear()`。
- **截圖**：debug 版的截圖工具（`crates/ziptool-gui/src/devshot.rs`），以環境變數控制：`ZIPTOOL_SHOT=輸出.png`、`ZIPTOOL_SHOT_OPEN=路徑`、`ZIPTOOL_SHOT_SELECT=選取前幾列`、`ZIPTOOL_SHOT_DIALOG=add|extract|rename|menu|…`、`ZIPTOOL_SHOT_DARK=1`、`ZIPTOOL_SHOT_LANG=en|zh`。
- **CI 截圖**：CI 的 screenshot 工作會截取資料夾、壓縮檔、加入（深色）、解壓縮（英文）四個畫面；改了畫面時下載來檢查：`gh run download <run-id> -n screenshots`。需要其他畫面時在 ci.yml 增加截圖項目。
- 不要用滑鼠自動化操作使用者的桌面；會寫登錄檔的功能只在測試機碼驗證。

## 只能在實體 Windows 桌面進行的檢查

CI 無法涵蓋，只有改到相關功能時才需要，請使用者在自己的電腦上執行：

- 剪貼簿：`ZIPO_CLIP_FILES="路徑1|路徑2" cargo test -p ziptool-gui real_clipboard -- --ignored`（會改寫真正的剪貼簿）
- 資源回收筒：`ZIPO_RECYCLE_FILE=路徑 cargo test -p ziptool-gui real_recycle -- --ignored`
- 工作列進度、檔案總管右鍵選單：手動確認
- 壓縮速度的比較：CI 機器效能不穩定，要在固定的機器上交替執行多次比較

## 記憶體與效能注意事項

- 執行緒數依記憶體預算決定（`util::reserve_threads`：實體記憶體的一半與目前可用記憶體取小者；32 位元最多 1.5 GB）。高等級 LZMA 每個執行緒將近 1 GB，只依核心數開執行緒會配置失敗而當掉。
- 7z 固實區塊 1 GB（`sevenz.rs` 的 `SOLID_BLOCK_BYTES`）；每個區塊結束都要等最慢的工作區塊，區塊太小會讓核心閒置。

## 發布流程

1. 三個 `Cargo.toml`（含彼此相依的版本）與 `crates/ziptool-gui/assets/ziptool.rc`（FILEVERSION、PRODUCTVERSION、FileVersion、ProductVersion）改版本號；新增 `docs/releases/v<版本>.md`（格式照前一版）。提交、推送，等 CI 全部通過。
2. `git tag v<版本> <完整 SHA>`，`git push origin v<版本>`。
3. `.github/workflows/release.yml` 檢查版本號與說明檔、確認該 commit 的 CI 已通過、建置、打包並測試，再建立 Release（約 10 分鐘）。等待時輪詢 `gh run view <id> --json status` 到 completed（`gh run watch` 曾提早結束而誤判失敗）。
4. 下載附件，確認 SHA-256 與 `gh release view --json assets` 的 digest 一致，解開後確認版本。
5. 更新 `packaging/winget/manifests/o/omni624562/Zipo/`：`git mv` 版本資料夾、改版本號與 `InstallerSha256`，提交後由 CI 檢查。送到 microsoft/winget-pkgs 要先問使用者。

**附件格式不可更動**（線上更新 `crates/ziptool-gui/src/update.rs` 只認得這種）：標籤 `v<主>.<次>.<修>`；附件 `zipo-<版本>-windows-x64.zip`，內含 `zipo-<版本>-windows-x64/` 資料夾，裡面有 `zipo-gui.exe`（必要）與 `zipo.exe`。發布檔一律由 GitHub Actions 建置，不要在本機建置後上傳。
