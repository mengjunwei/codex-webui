use crate::error::{AppError, ErrorCode};
use crate::services::extensions::config_merge;
use axum::http::StatusCode;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// 本地状态文件名：id → 本地扩展条目(name+hash) 映射，用于集群扩展同步对齐。
const STATE_FILE: &str = ".cluster-extensions.json";

/// 本地扩展条目：记录 name / hash / kind / market / version。
///
/// 设计：name + kind + market + version 用于删除分支定位落盘目录 —— **不依赖 PG 查询**，
/// 避免发起节点物理删 PG 行后、副本收到事件时 PG 已无该行 → `name_of(id)` 返回 None
/// → 目录成孤儿。hash 用于同步循环判断是否需要更新(与 PG content_hash 比对)。
///
/// - skill 目录:`skills/{name}/`
/// - plugin 目录:`plugins/cache/{market}/{name}/{version}/`(删除时清整个 `{name}/`)
///
/// `kind` 恒为 "skill" / "plugin"(未知类型不登记);`market`/`version` 仅 plugin 有值。
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub struct LocalExtEntry {
    /// 扩展名(skill 目录名 / plugin 名),删除时定位目录用。
    pub name: String,
    /// 内容指纹,同步对齐用。
    pub hash: String,
    /// 扩展类型:"skill" / "plugin"(MCP 留后续)。删除/落盘分支按此分发根目录。
    pub kind: String,
    /// plugin 的市场名(skill 为 None);plugin 删除时拼 `plugins/cache/{market}/` 用。
    pub market: Option<String>,
    /// plugin 的版本号(skill 为 None);plugin 落盘时拼 version 子目录用。
    pub version: Option<String>,
}

/// skills 目录：`<codex_home>/skills`，存放每个扩展落盘后的文件树。
pub fn skills_dir(codex_home: &Path) -> PathBuf {
    codex_home.join("skills")
}

/// plugin 缓存根目录：`<codex_home>/plugins/cache`，所有 marketplace plugin 落盘于此。
pub fn plugins_cache_dir(codex_home: &Path) -> PathBuf {
    codex_home.join("plugins").join("cache")
}

/// plugin 落盘目标目录：`<codex_home>/plugins/cache/<market>/<name>/<version>`。
/// 按 market/name/version 三级隔离,支持同 name 多 market、多版本并存。
///
/// 段校验:逐段拒绝空 / 绝对路径 / 含 `..` / 含反斜杠,防穿越。
/// 关口自带校验——Task 5 upload 等用户输入会灌入 market/name/version,
/// 而 `write_file_safe` 只校验 `rel_path` 不校验 `root`,若任一段为 `..` 等,
/// 算出的 root 会逸出 `plugins/cache`。
pub fn plugin_dest(
    codex_home: &Path,
    market: &str,
    name: &str,
    version: &str,
) -> Result<PathBuf, AppError> {
    for seg in [market, name, version] {
        if seg.is_empty()
            || seg.starts_with('/')
            || seg.starts_with('\\')
            || seg.contains("..")
            || seg.contains('\\')
        {
            return Err(AppError::internal(format!("invalid plugin segment: {seg}")));
        }
    }
    Ok(plugins_cache_dir(codex_home)
        .join(market)
        .join(name)
        .join(version))
}

/// plugin 落盘后写启用段到 config.toml：`[plugins."<name>@<market>"] enabled = "true"`。
/// 复用 config_merge 的段合并,保留其余配置原样。
pub async fn enable_plugin_config(
    codex_home: &Path,
    name: &str,
    market: &str,
) -> Result<(), AppError> {
    let cfg = codex_home.join("config.toml");
    let section = format!("plugins.\"{name}@{market}\"");
    // codex 的 plugins.enabled 是 boolean,必须写 `enabled = true`(无引号);
    // ensure_section_kv 会写字符串 "true" 致 codex 报 "invalid type: string, expected a boolean" 拒绝整个 config。
    config_merge::ensure_section_bool(&cfg, &section, "enabled", true).await
}

/// 删除 plugin 时移除启用段 `[plugins."<name>@<market>"]`。
/// 段不存在视为成功(容错),其余配置保留。
pub async fn disable_plugin_config(
    codex_home: &Path,
    name: &str,
    market: &str,
) -> Result<(), AppError> {
    let cfg = codex_home.join("config.toml");
    let section = format!("plugins.\"{name}@{market}\"");
    config_merge::remove_section(&cfg, &section).await
}

/// MCP 启用:把 content_toml(段内容,无段头)合并进 config.toml `[mcp_servers.<name>]` 段。
/// 复用 config_merge::merge_full_section:逐 key clone(支持 env 等嵌套值),保留其余配置。
pub async fn enable_mcp_config(
    codex_home: &Path,
    name: &str,
    content_toml: &str,
) -> Result<(), AppError> {
    let cfg = codex_home.join("config.toml");
    config_merge::merge_full_section(&cfg, "mcp_servers", name, content_toml).await
}

/// MCP 卸载:移除 config.toml `[mcp_servers.<name>]` 段。
/// 段不存在视为成功(容错),其余配置保留。
pub async fn disable_mcp_config(codex_home: &Path, name: &str) -> Result<(), AppError> {
    let cfg = codex_home.join("config.toml");
    let section = format!("mcp_servers.{name}");
    config_merge::remove_section(&cfg, &section).await
}

/// 安全拼路径：拒绝空 / 绝对 / 含 `..` / 含反斜杠的相对路径；
/// 归一化（反斜杠→正斜杠、小写）后校验 candidate 必以 root 开头，防穿越。
///
/// `pub(crate)`:ext-fetch(internal_rpc) 对 skill 根的 m.name 复核也复用本函数,
/// 与 plugin 分支的 plugin_dest 段校验同口径。
pub(crate) async fn safe_join_local(root: &Path, rel: &str) -> Result<PathBuf, AppError> {
    if rel.is_empty()
        || rel.starts_with('/')
        || rel.starts_with('\\')
        || rel.contains("..")
        || rel.contains('\\')
    {
        return Err(AppError::internal(format!("invalid rel_path: {rel}")));
    }
    let candidate = root.join(rel);
    let c = candidate
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();
    let r = root.to_string_lossy().replace('\\', "/").to_lowercase();
    if !c.starts_with(&r) {
        return Err(AppError::internal(format!("path escapes root: {rel}")));
    }
    Ok(candidate)
}

/// 写文件（自动建父目录）。
pub async fn write_file_safe(root: &Path, rel: &str, content: &[u8]) -> Result<(), AppError> {
    let path = safe_join_local(root, rel).await?;
    if let Some(p) = path.parent() {
        tokio::fs::create_dir_all(p)
            .await
            .map_err(|e| AppError::internal(format!("mkdir: {e}")))?;
    }
    tokio::fs::write(&path, content)
        .await
        .map_err(|e| AppError::internal(format!("write {}: {e}", path.display())))?;
    Ok(())
}

/// 解压 zip 字节流到 dest 目录。
///
/// - 自动剥单一顶层目录:所有文件 entry 共享同一 `topdir/` 前缀时剥掉,扁平化,
///   避免落盘成 `skills/{name}/{topdir}/...` 多一层(与前端"选目录打包"语义对齐)。
/// - 防 zip-slip:每个 entry 的 rel_path 经 `write_file_safe` → `safe_join_local` 校验,
///   含 `..` / 绝对路径 / 反斜杠一律拒绝(与原 files 数组分支同口径)。
/// - zip bomb 安全阀:解压前按 `entry.size()`(zip 元数据)累计未压缩总字节 + 文件数,
///   超上限即报错,防止恶意小包解出超大目录撑爆磁盘。
/// - 返回落盘后的文件指纹(`scan_dir`),供调用方算 `aggregate_hash`。
///
/// 调用方须自行保证 `dest` 路径合法(name 段无穿越),本函数只校验 zip 内 entry 路径。
pub async fn unzip_to_dest(
    zip_bytes: &[u8],
    dest: &Path,
) -> Result<Vec<crate::services::extensions::fingerprint::FileFingerprint>, AppError> {
    let cursor = std::io::Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| AppError::internal(format!("zip parse: {e}")))?;

    // 第一遍:收集所有文件 entry 名(跳过目录 entry),用于检测单一顶层目录。
    let mut names: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        if let Ok(e) = archive.by_index(i) {
            if !e.is_dir() {
                names.push(e.name().to_string());
            }
        }
    }
    let strip = detect_common_topdir(&names);

    // 清 dest(若存在)+ 建目录,准备解压。同名重传走全量替换语义。
    if dest.exists() {
        let _ = tokio::fs::remove_dir_all(dest).await;
    }
    tokio::fs::create_dir_all(dest)
        .await
        .map_err(|e| AppError::internal(format!("mkdir {}: {e}", dest.display())))?;

    // zip bomb 安全阀:解压后总字节 / 文件数上限(与原 skill 上传 50MB 总量 + 200 文件同量级)。
    const MAX_UNCOMPRESSED_TOTAL: u64 = 50 * 1024 * 1024; // 50 MB
    const MAX_ENTRY_COUNT: usize = 200;

    let mut total_uncompressed: u64 = 0;
    let mut file_count: usize = 0;

    // 第二遍:逐文件解压落盘。entry 在内层块结束时 drop,避免跨 await 借用 archive。
    for i in 0..archive.len() {
        let rel;
        let buf: Vec<u8>;
        {
            let mut entry = match archive.by_index(i) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.is_dir() {
                continue;
            }
            rel = apply_strip(entry.name(), strip.as_deref());
            // zip bomb 双层防护:
            //   1) 元数据级预检:entry.size() 是 zip 声明的未压缩大小,解压前按它累计,
            //      超限立即报错(快速失败,不为明显超大 entry 预分配/读取)。
            //   2) 实际字节级:声明值可被恶意伪造(声明 1 实际 GB 的 zip bomb),故 read_to_end
            //      后按真实解出字节 v.len() 再补判一次——实际 > 声明时补回差额重判,堵住绕过。
            let declared = entry.size();
            total_uncompressed = total_uncompressed.saturating_add(declared);
            if total_uncompressed > MAX_UNCOMPRESSED_TOTAL {
                return Err(AppError::internal(format!(
                    "解压后总字节数(声明) {total_uncompressed} 超过上限 {MAX_UNCOMPRESSED_TOTAL}"
                )));
            }
            file_count += 1;
            if file_count > MAX_ENTRY_COUNT {
                return Err(AppError::internal(format!(
                    "zip 内文件数 {file_count} 超过上限 {MAX_ENTRY_COUNT}"
                )));
            }
            // 预分配按声明值(已过预检 → declared ≤ 剩余配额 ≤ MAX),防声明超大值意外撑内存。
            let mut v = Vec::with_capacity(declared.min(MAX_UNCOMPRESSED_TOTAL) as usize);
            entry
                .read_to_end(&mut v)
                .map_err(|e| AppError::internal(format!("zip read: {e}")))?;
            // 实际字节二次校验:实际解出 > 声明(声明被伪造)时,补回差额并重判总配额。
            let real = v.len() as u64;
            if real > declared {
                total_uncompressed = total_uncompressed.saturating_add(real - declared);
                if total_uncompressed > MAX_UNCOMPRESSED_TOTAL {
                    return Err(AppError::internal(format!(
                        "解压后总字节数(实际) {total_uncompressed} 超过上限 {MAX_UNCOMPRESSED_TOTAL}"
                    )));
                }
            }
            buf = v;
        }
        write_file_safe(dest, &rel, &buf).await?;
    }

    crate::services::extensions::fingerprint::scan_dir(dest).await
}

/// 校验解压后的目录是否为合法 skill:根目录必须有 `SKILL.md`,且是文件、且非空,
/// 且以 YAML frontmatter 起始(codex skill 加载器要求)。
///
/// codex skill 的入口标准是目录根存在 `SKILL.md`(描述 skill 元信息,类似 README),
/// 且首部须有 `---` 分隔的 YAML frontmatter(否则 codex 运行时报
/// "missing YAML frontmatter delimited by ---")。缺失/是目录/空/无 frontmatter 均
/// 视为非法 skill 包,上传时应在落盘正式目录前用此函数拦截。
///
/// 校验失败返回 400 业务错误(中文消息),调用方 `?` 上抛即可;若解压在临时目录中转,
/// 由 `tempfile::TempDir` drop 自动清理,不留残渣、不入库。
pub async fn validate_skill(dest: &Path) -> Result<(), AppError> {
    let skill_md = dest.join("SKILL.md");
    // metadata 取不到(文件不存在/无权限)→ 视为缺 SKILL.md。
    let meta = tokio::fs::metadata(&skill_md).await.map_err(|_| {
        AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "不是合法 skill:缺少 SKILL.md".into(),
            None,
        )
    })?;
    if !meta.is_file() {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "不是合法 skill:SKILL.md 不是文件".into(),
            None,
        ));
    }
    if meta.len() == 0 {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "不是合法 skill:SKILL.md 为空".into(),
            None,
        ));
    }
    // 校验 YAML frontmatter(codex 要求 SKILL.md 以 --- 起始并闭合)。
    let content = tokio::fs::read_to_string(&skill_md)
        .await
        .map_err(|e| AppError::internal(format!("read SKILL.md: {e}")))?;
    if !has_yaml_frontmatter(&content) {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "不是合法 skill:SKILL.md 缺少 YAML frontmatter(须以 `---` 起始并有闭合 `---`)"
                .into(),
            None,
        ));
    }
    Ok(())
}

/// SKILL.md 是否以合法 YAML frontmatter 起始(codex skill 加载要求):
/// 首行(去 BOM/空白后)为 `---`,且后续 64 行内存在第二个 `---` 闭合。
fn has_yaml_frontmatter(content: &str) -> bool {
    let mut lines = content.lines();
    let first = lines
        .next()
        .map(|l| l.trim_start_matches('\u{feff}').trim());
    if first != Some("---") {
        return false;
    }
    lines.take(64).any(|l| l.trim() == "---")
}

/// 检测所有文件是否共享单一顶层目录;返回该顶层(含尾 `/`)或 None。
///
/// 仅当所有 entry 都以同一 `topdir/` 开头(即都含 `/`)时才剥 —— 避免误剥无公共顶层
/// 的扁平包(那种情况下首段是文件名而非目录名,剥了会丢前缀)。
fn detect_common_topdir(names: &[String]) -> Option<String> {
    let tops: Vec<&str> = names.iter().filter_map(|n| n.split('/').next()).collect();
    if tops.is_empty() {
        return None;
    }
    let first = tops[0];
    if tops.iter().all(|t| *t == first) && names.iter().all(|n| n.contains('/')) {
        Some(format!("{first}/"))
    } else {
        None
    }
}

/// 剥掉顶层前缀(若有);并归一化反斜杠为正斜杠(跨平台稳定,与 scan_dir 口径一致)。
fn apply_strip(name: &str, strip: Option<&str>) -> String {
    let n = name.replace('\\', "/");
    match strip {
        Some(p) if n.starts_with(p) => n[p.len()..].to_string(),
        _ => n,
    }
}

/// 删除 root/{name} 整个目录（skill 卸载）。目录不存在视为成功。
pub async fn remove_dir_safe(root: &Path, name: &str) -> Result<(), AppError> {
    let dir = safe_join_local(root, name).await?;
    if dir.exists() {
        tokio::fs::remove_dir_all(&dir)
            .await
            .map_err(|e| AppError::internal(format!("remove {}: {e}", dir.display())))?;
    }
    Ok(())
}

/// 读取本地状态文件；不存在或解析失败(含旧格式 id→hash)时返回空 map(容错)。
pub async fn load_local_state(codex_home: &Path) -> HashMap<String, LocalExtEntry> {
    let p = codex_home.join(STATE_FILE);
    match tokio::fs::read(&p).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

/// 写入本地状态文件(原子覆盖:先写 .tmp 再 rename)。
///
/// 原子写:进程崩溃 / 断电 / 磁盘满中途若直接覆盖写,文件可能截断或损坏,而
/// `load_local_state` 对解析失败静默返回空 map(节点误以为"无任何扩展" → 全量重下 +
/// stale 旧条目丢失 → 孤儿目录不再被清理)。M2 把每轮 save 次数从 1 放大到 N 次,
/// 损坏暴露窗口同比放大,故用 temp + rename 收口。tmp 与目标同目录 → 同文件系统,
/// rename 在 Linux/Windows 均为原子替换。tmp 残留(rename 失败时)无害:下次同名覆盖,
/// load 只读正式文件。
pub async fn save_local_state(
    codex_home: &Path,
    map: &HashMap<String, LocalExtEntry>,
) -> Result<(), AppError> {
    let bytes = serde_json::to_vec(map).map_err(|e| AppError::internal(format!("json: {e}")))?;
    let final_path = codex_home.join(STATE_FILE);
    let tmp_path = codex_home.join(format!("{STATE_FILE}.tmp"));
    tokio::fs::write(&tmp_path, &bytes)
        .await
        .map_err(|e| AppError::internal(format!("write state tmp: {e}")))?;
    tokio::fs::rename(&tmp_path, &final_path)
        .await
        .map_err(|e| AppError::internal(format!("rename state: {e}")))?;
    Ok(())
}

/// 在 ext_state_lock 保护下执行 local_state 的读-改-写(防并发丢失更新)。
///
/// 闭包 f 收到 load 出的 HashMap 可变引用,做 insert/remove 等修改;返回后 save 回盘。
/// 锁由调用方从 AppState.ext_state_lock 传入。锁粒度精确到单次 load→改→save(几 ms),
/// 不阻塞 run_round 的同步主体(DB/RPC/文件落盘)。
///
/// 所有 local_state 修改(run_round 的 stale 清理/同步登记、upload/delete handler)都应走
/// 本函数,与其它写者互斥 → 不丢更新。返回闭包结果(如 remove 取出的 entry),save 失败返 Err。
pub async fn with_local_state<R, F>(
    lock: &tokio::sync::Mutex<()>,
    codex_home: &Path,
    f: F,
) -> Result<R, AppError>
where
    F: FnOnce(&mut HashMap<String, LocalExtEntry>) -> R,
{
    let _guard = lock.lock().await;
    let mut map = load_local_state(codex_home).await;
    let result = f(&mut map);
    save_local_state(codex_home, &map).await?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn write_file_safe_creates_nested_and_content() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file_safe(root, "my-skill/scripts/run.sh", b"hi").await.unwrap();
        let got = tokio::fs::read(root.join("my-skill/scripts/run.sh")).await.unwrap();
        assert_eq!(got, b"hi");
    }

    #[tokio::test]
    async fn write_file_safe_rejects_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let r = write_file_safe(tmp.path(), "../escape.sh", b"x").await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn local_state_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = HashMap::new();
        m.insert(
            "ext_1".into(),
            LocalExtEntry {
                name: "skill-1".into(),
                hash: "deadbeef".into(),
                kind: "skill".into(),
                market: None,
                version: None,
            },
        );
        // plugin 条目:覆盖 market/version 序列化往返。
        m.insert(
            "ext_2".into(),
            LocalExtEntry {
                name: "foo".into(),
                hash: "cafef00d".into(),
                kind: "plugin".into(),
                market: Some("openai-api-curated".into()),
                version: Some("1.2.3".into()),
            },
        );
        save_local_state(tmp.path(), &m).await.unwrap();
        let loaded = load_local_state(tmp.path()).await;
        let e = loaded.get("ext_1").expect("ext_1 应存在");
        assert_eq!(e.name, "skill-1");
        assert_eq!(e.hash, "deadbeef");
        assert_eq!(e.kind, "skill");
        let p = loaded.get("ext_2").expect("ext_2 应存在");
        assert_eq!(p.kind, "plugin");
        assert_eq!(p.market.as_deref(), Some("openai-api-curated"));
        assert_eq!(p.version.as_deref(), Some("1.2.3"));
    }

    /// plugin_dest 路径拼接(用 temp_dir 避免硬编码绝对路径) + 段穿越拒绝校验。
    #[test]
    fn plugin_dest_path_and_traversal_rejected() {
        let base = std::env::temp_dir();
        let got = plugin_dest(&base, "mkt", "foo", "1.2.3").unwrap();
        assert!(got.ends_with("plugins/cache/mkt/foo/1.2.3"));
        // 段含 .. 被拒
        assert!(plugin_dest(&base, "mkt", "../pwned", "1.2.3").is_err());
        assert!(plugin_dest(&base, "mkt", "foo", "/abs").is_err());
    }

    /// validate_skill:有非空 SKILL.md + 合法 YAML frontmatter → Ok。
    #[tokio::test]
    async fn validate_skill_ok_when_skill_md_present_nonempty() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join("SKILL.md"),
            "---\nname: my-skill\ndescription: test\n---\n# my skill\n",
        )
        .await
        .unwrap();
        validate_skill(tmp.path()).await.unwrap();
    }

    /// validate_skill:缺 SKILL.md / SKILL.md 为空 → Err(400 业务错误)。
    #[tokio::test]
    async fn validate_skill_errors_when_missing_or_empty() {
        // 缺 SKILL.md
        let tmp = tempfile::tempdir().unwrap();
        let err = validate_skill(tmp.path()).await.unwrap_err();
        assert!(matches!(err, AppError::Business { .. }));
        // 空文件
        tokio::fs::write(tmp.path().join("SKILL.md"), "")
            .await
            .unwrap();
        let err = validate_skill(tmp.path()).await.unwrap_err();
        assert!(matches!(err, AppError::Business { .. }));
    }

    /// validate_skill:SKILL.md 缺 YAML frontmatter → Err(对齐 codex 加载器要求)。
    #[tokio::test]
    async fn validate_skill_errors_when_missing_frontmatter() {
        // 纯 markdown 正文,无 --- frontmatter(ui-skill 类问题)
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("SKILL.md"), "# UI 模拟\n正文内容\n")
            .await
            .unwrap();
        let err = validate_skill(tmp.path()).await.unwrap_err();
        assert!(matches!(err, AppError::Business { .. }));
        // 有起始 --- 但无闭合 --- 同样非法
        tokio::fs::write(tmp.path().join("SKILL.md"), "---\nname: x\n无闭合\n")
            .await
            .unwrap();
        let err = validate_skill(tmp.path()).await.unwrap_err();
        assert!(matches!(err, AppError::Business { .. }));
    }

    /// enable 写入 [plugins."<name>@<market>"] 段;disable 后该段被移除。
    #[tokio::test]
    async fn plugin_config_enable_disable_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        enable_plugin_config(tmp.path(), "foo", "mkt")
            .await
            .unwrap();
        let s = tokio::fs::read_to_string(tmp.path().join("config.toml"))
            .await
            .unwrap();
        assert!(s.contains("[plugins.\"foo@mkt\"]"));
        disable_plugin_config(tmp.path(), "foo", "mkt")
            .await
            .unwrap();
        let s2 = tokio::fs::read_to_string(tmp.path().join("config.toml"))
            .await
            .unwrap();
        assert!(!s2.contains("foo@mkt"));
    }

    /// 验证 C1 候选:MCP name 含 "." 时 enable/disable 是否对称(是否删错段)。
    /// enable 写 [mcp_servers."my.server"];disable 应精确移除该段,
    /// 且不误伤可能存在的 [mcp_servers.my] 段。
    #[tokio::test]
    async fn mcp_config_dotted_name_enable_disable_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        // 预置一个独立的 [mcp_servers.my] 段(C1 候选说 disable 会误删它)。
        tokio::fs::write(
            tmp.path().join("config.toml"),
            "[mcp_servers.my]\ncommand = \"keep\"\n",
        )
        .await
        .unwrap();
        // enable 含点 name
        enable_mcp_config(tmp.path(), "my.server", "command = \"node\"\n")
            .await
            .unwrap();
        let after_enable = tokio::fs::read_to_string(tmp.path().join("config.toml"))
            .await
            .unwrap();
        // disable 含点 name
        disable_mcp_config(tmp.path(), "my.server").await.unwrap();
        let after_disable = tokio::fs::read_to_string(tmp.path().join("config.toml"))
            .await
            .unwrap();
        eprintln!("=== after_enable ===\n{after_enable}");
        eprintln!("=== after_disable ===\n{after_disable}");
        // 目标段 my.server 必须被移除
        assert!(
            !after_disable.contains("my.server"),
            "disable 后 my.server 段仍残留(卸载失效)"
        );
        // 预置的 my 段必须保留(若被误删 = C1 真 bug)
        assert!(
            after_disable.contains("keep"),
            "disable 误删了无关的 [mcp_servers.my] 段(C1 确认)"
        );
    }

    /// MCP 启用:enable 后 config.toml 含 [mcp_servers.mysrv] 及 command 字段;
    /// disable 后该段被移除,文件中不再出现 mysrv。验证 Task 2 的两个封装。
    #[tokio::test]
    async fn mcp_config_enable_disable_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        enable_mcp_config(tmp.path(), "mysrv", "command = \"node\"\nargs = [\"s.js\"]\n")
            .await
            .unwrap();
        let s = tokio::fs::read_to_string(tmp.path().join("config.toml"))
            .await
            .unwrap();
        assert!(s.contains("command"));
        disable_mcp_config(tmp.path(), "mysrv")
            .await
            .unwrap();
        let s2 = tokio::fs::read_to_string(tmp.path().join("config.toml"))
            .await
            .unwrap();
        assert!(!s2.contains("mysrv"));
    }

    /// with_local_state 并发写测试:多 task 同时 insert 不同 key,
    /// 若锁粒度不足或保存覆盖,最终会出现丢 key。验证 M2 不丢更新。
    #[tokio::test]
    async fn with_local_state_no_lost_update() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let home = tmp.path().to_path_buf();
        const N: usize = 8;
        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let lock = lock.clone();
            let home_t = home.clone();
            let h = tokio::spawn(async move {
                let id = format!("ext_{i}");
                let name = format!("name_{i}");
                with_local_state(&lock, &home_t, |m| {
                    m.insert(
                        id,
                        LocalExtEntry {
                            name,
                            hash: format!("hash_{i}"),
                            kind: "skill".into(),
                            market: None,
                            version: None,
                        },
                    );
                })
                .await
                .unwrap();
            });
            handles.push(h);
        }
        for h in handles {
            h.await.unwrap();
        }
        let loaded = load_local_state(&home).await;
        assert_eq!(loaded.len(), N, "并发写入后 key 数量不符(丢失更新?)");
        for i in 0..N {
            let id = format!("ext_{i}");
            assert!(
                loaded.contains_key(&id),
                "key {id} 在并发写入后应存在"
            );
        }
    }

    /// save_local_state 原子写:save 后正式文件存在且可解析;.tmp 不残留。
    /// (无法在单测里中途 kill 进程验证半截,这里验证正常路径的 temp+rename 收尾干净。)
    #[tokio::test]
    async fn save_local_state_atomic_no_tmp_residue() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = HashMap::new();
        m.insert(
            "ext_x".into(),
            LocalExtEntry {
                name: "s".into(),
                hash: "h".into(),
                kind: "skill".into(),
                market: None,
                version: None,
            },
        );
        save_local_state(tmp.path(), &m).await.unwrap();
        // 正式文件存在 + 可解析。
        assert!(tmp.path().join(STATE_FILE).exists());
        let loaded = load_local_state(tmp.path()).await;
        assert!(loaded.contains_key("ext_x"));
        // .tmp 不应残留(rename 成功后已移走)。
        assert!(
            !tmp.path().join(format!("{STATE_FILE}.tmp")).exists(),
            "save 后 .tmp 不应残留"
        );
    }
}
