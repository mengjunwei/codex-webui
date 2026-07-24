# Rust 依赖全面升级设计

## 范围

升级 `backend-rs` 中除 Git 依赖 `wezterm-term` 外的全部 crates.io 依赖，允许跨主版本升级。保留 Git URL、revision 和配置不变。

## 执行策略

1. 查询并更新直接依赖的最新版本约束。
2. 重新解析并生成 `Cargo.lock`，确保间接 crates.io 依赖同步更新。
3. 运行格式检查、默认 feature 编译/测试及 `memberlist-backend` feature 编译。
4. 根据编译错误修复升级造成的 API 兼容问题，不进行无关重构。
5. 检查 Git 依赖仍指向原有 revision，并汇总无法升级到最新版本的依赖及原因。

## 验收标准

- `wezterm-term` 的 Git revision 未改变。
- `cargo fmt --check` 通过。
- `cargo check` 通过。
- `cargo test` 通过，或明确报告环境导致的失败。
- `cargo check --features memberlist-backend` 通过。
