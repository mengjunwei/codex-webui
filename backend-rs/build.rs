//! 构建脚本：确保 rust-embed 的 `#[folder = "public"]` 目录在编译期存在。
//!
//! rust-embed 的 proc macro 要求 folder 目录存在，否则编译失败。
//! 前端未 build 时这里创建空目录兜底——**release 打包前必须先执行
//! `pnpm --dir web build`**，否则会嵌入空资源（前端不可用）。
//!
//! 产物目录放在 crate 内部（`backend-rs/public`），这样 cross 在 Docker
//! 容器内编译时也能直接访问到（容器只挂载 backend-rs 本身）。

fn main() {
    let public = std::path::Path::new("public");
    if !public.exists() {
        let _ = std::fs::create_dir_all(public);
        println!(
            "cargo:warning=public 不存在，已创建空目录兜底；release 打包前请先执行 pnpm --dir web build"
        );
    }
    // 前端产物变化时触发重新编译（重新嵌入）。
    println!("cargo:rerun-if-changed=public");
}
