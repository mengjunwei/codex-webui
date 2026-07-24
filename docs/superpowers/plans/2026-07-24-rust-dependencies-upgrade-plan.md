# Rust 依赖全面升级实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 `backend-rs` 中除 `wezterm-term` 外的所有 crates.io 依赖升级到当前可用最新版本，允许跨主版本升级，并通过编译、测试和 feature 验证。

**Architecture:** 先记录当前依赖基线，再使用 Cargo 的升级能力更新直接依赖声明与锁文件；随后按编译错误逐项适配 API，不改变业务架构。Git 依赖 `wezterm-term` 的 URL、revision `fff02ca501c3b457f99b467a86061d2b150c51f2` 和 feature 配置保持不变。

**Tech Stack:** Rust edition 2024、Cargo、axum、SeaORM、Tokio、OpenTelemetry、Redis、memberlist。

## Global Constraints

- 仅处理 `backend-rs` Rust 项目。
- 除 Git 依赖 `wezterm-term` 外，所有直接和间接 crates.io 依赖都纳入升级。
- 允许跨主版本升级，并同步修复必要的 API 兼容问题。
- 不更新 `wezterm-term` 的 Git URL、revision 或依赖来源。
- 不修改与本次升级无关的业务逻辑和已有未跟踪策略引擎文档。
- 数据库、前端和策略引擎功能不做无关重构。
- 每次修改后优先运行最小相关验证，最终运行完整验证。
- 测试失败、工具链限制或某依赖无法升级时必须保留实际输出并在结论中说明。

---

## 文件结构与职责

| 文件 | 操作 | 职责 |
|------|------|------|
| `backend-rs/Cargo.toml` | 修改 | 更新直接 crates.io 依赖版本与必要 feature 配置；保留 `wezterm-term` Git 配置 |
| `backend-rs/Cargo.lock` | 生成 | 记录升级后的直接和间接依赖解析结果 |
| `backend-rs/src/**/*.rs` | 按需修改 | 仅修复依赖跨主版本升级引起的编译/API 适配 |
| `docs/superpowers/specs/2026-07-24-rust-dependencies-upgrade-design.md` | 已新增 | 记录升级范围、策略和验收标准；不与代码混改 |

---

### Task 1: 建立依赖升级基线

**Files:**
- Read: `backend-rs/Cargo.toml`
- Read: `backend-rs/Cargo.lock`
- Modify: `docs/superpowers/plans/2026-07-24-rust-dependencies-upgrade-plan.md`

**Interfaces:**
- Produces: 当前直接依赖版本、Git 依赖 revision、Rust/Cargo 工具链和基线验证结果，供后续比较。

- [ ] **Step 1: 记录工具链与依赖树基线**

运行：

```bash
cd backend-rs
rustc --version
cargo --version
cargo tree --locked > ../dependency-tree-before.txt
cargo metadata --locked --format-version 1 > ../cargo-metadata-before.json
```

预期：命令成功；若现有锁文件或环境导致失败，记录完整错误，不修改依赖。

- [ ] **Step 2: 记录基线编译与测试结果**

运行：

```bash
cargo fmt --check
cargo check --locked
cargo test --locked
cargo check --locked --features memberlist-backend
```

预期：每条命令给出明确 PASS 或失败原因。基线已有失败不归因于升级。

- [ ] **Step 3: 保存 Git 依赖保护值**

确认 `backend-rs/Cargo.toml` 中仍为：

```toml
wezterm-term = { git = "https://github.com/wez/wezterm", rev = "fff02ca501c3b457f99b467a86061d2b150c51f2" }
```

并从 `Cargo.lock` 记录 `wezterm-term` 的 `source` 与 revision，后续逐字比较。

- [ ] **Step 4: 提交基线记录（仅在用户要求提交时执行）**

不得把临时的 `dependency-tree-before.txt` 或 `cargo-metadata-before.json` 纳入提交。若需要提交，仅提交计划/设计文档变更：

```bash
git add docs/superpowers/specs/2026-07-24-rust-dependencies-upgrade-design.md docs/superpowers/plans/2026-07-24-rust-dependencies-upgrade-plan.md
git commit -m "docs: 记录 Rust 依赖升级计划"
```

---

### Task 2: 升级 crates.io 直接依赖并刷新锁文件

**Files:**
- Modify: `backend-rs/Cargo.toml`
- Modify: `backend-rs/Cargo.lock`

**Interfaces:**
- Consumes: Task 1 的基线和 Git revision 保护值。
- Produces: 所有可升级的直接 crates.io 依赖声明与新的锁文件；`wezterm-term` 来源不变。

- [ ] **Step 1: 检查升级工具是否可用**

运行：

```bash
cargo upgrade --version
```

若命令不存在，先运行：

```bash
cargo install cargo-edit --locked
```

若网络、权限或 Rust 版本不允许安装，改用手工核对 `cargo search`/`cargo update`，并记录限制。

- [ ] **Step 2: 预览所有直接依赖的最新版本**

运行：

```bash
cd backend-rs
cargo upgrade --dry-run --incompatible allow
```

检查输出中不包含 `wezterm-term`，且不把 Git revision 依赖转换为 crates.io 依赖。

- [ ] **Step 3: 更新直接依赖声明**

运行：

```bash
cargo upgrade --incompatible allow
```

如果当前 cargo-edit 不支持该参数，使用其等价的允许不兼容版本选项；不得使用会改写 Git 依赖来源的命令。

- [ ] **Step 4: 刷新锁文件中的 crates.io 依赖**

运行：

```bash
cargo update
```

若某些包仍被旧版本约束锁定，使用 `cargo update -p <package> --precise <version>` 仅处理已确认的 crates.io 包，不操作 `wezterm-term`。

- [ ] **Step 5: 验证 Git 依赖未变化**

运行：

```bash
git diff -- backend-rs/Cargo.toml backend-rs/Cargo.lock
```

确认 `wezterm-term` 仍使用原始 Git URL 和 revision；若发生变化，立即恢复该依赖配置并重新生成锁文件。

- [ ] **Step 6: 检查锁文件完整性**

运行：

```bash
cargo check --locked
cargo tree --locked --duplicates
```

预期：锁文件可复现，重复版本仅在上游约束确实需要时保留。

- [ ] **Step 7: 提交纯依赖升级（若此时无编译适配修改）**

```bash
git add backend-rs/Cargo.toml backend-rs/Cargo.lock
git commit -m "chore(deps): 升级 Rust crates.io 依赖"
```

如果马上需要 API 适配，则将依赖和适配代码作为同一逻辑提交，不提交半成品。

---

### Task 3: 修复默认 feature 的编译兼容问题

**Files:**
- Modify: `backend-rs/src/**/*.rs`（仅限编译错误涉及文件）
- Test: 受影响模块现有测试

**Interfaces:**
- Consumes: Task 2 更新后的 Cargo 解析结果。
- Produces: 默认 feature 下可编译的源码，保留现有公共行为和错误处理语义。

- [ ] **Step 1: 首次运行默认 feature 编译并收集完整错误**

运行：

```bash
cd backend-rs
cargo check
```

按错误顺序处理，不根据猜测批量修改；优先修复根因依赖（例如 trait、类型、feature 或模块路径变化）。

- [ ] **Step 2: 针对每个错误编写或扩展最小回归测试**

测试应放在受影响模块现有测试位置，覆盖升级后使用的 API 语义。例如序列化、HTTP 响应、数据库查询或错误转换；测试断言应验证业务结果，而不是依赖内部实现细节。

- [ ] **Step 3: 修改最小兼容代码**

遵循现有命名、错误处理和中文注释风格；不得通过关闭安全校验、删除功能或固定旧版本来绕过错误。若需要改变 feature，必须说明该 feature 对应的功能影响。

- [ ] **Step 4: 逐模块验证并重新编译**

运行：

```bash
cargo test <受影响模块或测试名>
cargo check
```

预期：新回归测试和已有相关测试通过，`cargo check` 不再报告该批错误。

- [ ] **Step 5: 提交兼容适配**

```bash
git add backend-rs/src backend-rs/Cargo.toml backend-rs/Cargo.lock
git commit -m "fix(compat): 适配升级后的 Rust 依赖 API"
```

---

### Task 4: 验证 memberlist feature 与完整测试套件

**Files:**
- Modify: 无；若发现 feature 专属 API 问题，修改对应 `backend-rs/src/**/*.rs`
- Test: `backend-rs` 全量现有测试

**Interfaces:**
- Consumes: 默认 feature 编译通过的依赖与源码。
- Produces: 默认配置和 `memberlist-backend` 配置均通过的构建与测试证据。

- [ ] **Step 1: 验证可选 feature 编译**

运行：

```bash
cd backend-rs
cargo check --features memberlist-backend
```

预期：通过；若失败，修复 memberlist/nodecraft 新版本 API 或 feature 变更，并对相关逻辑增加最小测试。

- [ ] **Step 2: 运行格式检查**

```bash
cargo fmt --check
```

若失败，仅运行 `cargo fmt` 修正格式，再检查一次。

- [ ] **Step 3: 运行完整测试**

```bash
cargo test
```

记录所有失败测试的名称、输出和是否由环境依赖导致；不得将失败描述为通过。

- [ ] **Step 4: 验证锁定构建可复现**

```bash
cargo check --locked
cargo test --locked
cargo check --locked --features memberlist-backend
```

预期：不修改 `Cargo.lock`；如果 Cargo 要求更新锁文件，先检查是否有遗漏的依赖声明或 Git revision 变化。

---

### Task 5: 做最终差异审查与升级报告

**Files:**
- Read: `backend-rs/Cargo.toml`
- Read: `backend-rs/Cargo.lock`
- Modify: `docs/superpowers/plans/2026-07-24-rust-dependencies-upgrade-plan.md`（勾选完成项并记录结果）

**Interfaces:**
- Consumes: Task 2-4 的变更和验证输出。
- Produces: 可审查的最终 diff、Git 依赖保护结论、验证结果与已知限制。

- [ ] **Step 1: 审查最终 diff 范围**

```bash
git diff --stat
git diff -- backend-rs/Cargo.toml
```

确认没有无关业务改动、调试输出、临时文件或策略文档覆盖。

- [ ] **Step 2: 审查 Git 依赖来源**

```bash
git diff -- backend-rs/Cargo.lock | findstr /i "wezterm-term github.com/wez/wezterm fff02ca501c3b457f99b467a86061d2b150c51f2"
```

在 Windows Git Bash 中若 `findstr` 不可用，使用：

```bash
git diff -- backend-rs/Cargo.lock | grep -i -E "wezterm-term|github.com/wez/wezterm|fff02ca501c3b457f99b467a86061d2b150c51f2"
```

预期：Git revision 与升级前一致。

- [ ] **Step 3: 检查依赖树中是否仍存在明显过期直接依赖**

```bash
cargo tree --locked > ../dependency-tree-after.txt
cargo metadata --locked --format-version 1 > ../cargo-metadata-after.json
```

对比升级前后文件，确认直接依赖已更新；临时文件不提交。

- [ ] **Step 4: 更新计划完成状态并汇总例外**

在计划末尾记录：

- 实际升级的直接依赖；
- 无法升级的依赖及 Cargo/Rust/上游约束原因；
- Git 依赖 revision 保持不变的证据；
- `cargo fmt --check`、`cargo check`、`cargo test`、`cargo check --features memberlist-backend` 的实际结果；
- 如果测试因外部数据库、Redis、网络或工具链失败，明确标注为环境限制。

- [ ] **Step 5: 最终状态确认**

```bash
git status --short
git diff --check
```

预期：无空白错误；仅保留依赖升级、必要兼容修复和已确认的设计/计划文档变更。
