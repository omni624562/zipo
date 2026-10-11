fn main() {
    // 把圖示嵌入 Windows 執行檔
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/ziptool.rc", embed_resource::NONE).manifest_optional().unwrap();
    }
}
