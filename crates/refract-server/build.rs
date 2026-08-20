//! 构建脚本：让 cargo 在前端产物变化时重新编译。

use std::path::Path;

/// `rust-embed` 只在编译期读一次 dist，产物更新必须触发重编。
fn main() {
    let dist = Path::new("../../apps/admin/dist");

    println!("cargo:rerun-if-changed=../../apps/admin/dist");

    if !dist.join("index.html").exists() {
        // 不自动跑 pnpm：构建脚本静默调用包管理器会让编译时间和网络行为
        // 变得不可预测，CI 里也常常没有 node。缺产物时给出可操作的提示即可，
        // `statics.rs` 对空 dist 有降级路径，后端仍可独立编译与测试。
        println!(
            "cargo:warning=apps/admin/dist/index.html not found — the embedded UI will be empty. \
             Build it with: pnpm --filter @refract/admin build"
        );
    }
}
