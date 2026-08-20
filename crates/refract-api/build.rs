//! 构建脚本：保证 rust-embed 的内嵌目录存在。
//!
//! rust-embed 对不存在的目录会**编译失败**，而后端必须能在前端尚未构建时
//! 独立编译 —— CI 里前后端是分开跑的，本地也常常只想 `cargo test` 一下。
//! 建一个空目录是最小代价的解法：真实构建产物存在时它什么也不做。

use std::path::Path;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let dist = Path::new(&manifest).join("../../apps/admin/dist");

    if !dist.exists() {
        std::fs::create_dir_all(&dist).expect("failed to create apps/admin/dist placeholder");
    }

    // 前端产物变化时要重新编译，否则改了界面却还嵌着旧文件。
    println!("cargo:rerun-if-changed=../../apps/admin/dist");
}
