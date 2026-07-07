//! 运行时设置定义 —— 移植自 `src/settings/settings.definitions.ts`。
//!
//! 共 12 项设置，横跨 4 个分类（terminal、files、security、general）。
//! `default_value` 是一个原始字符串，读取时按类型解释。
//! `constraints`（min/max/integer）现已建模 + 持久化 + 强制校验，与 TS 保持一致。

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SettingType {
    String,
    Number,
    Boolean,
    Json,
}

impl SettingType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Json => "json",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Category {
    Terminal,
    Files,
    Security,
    General,
}

impl Category {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Files => "files",
            Self::Security => "security",
            Self::General => "general",
        }
    }
}

/// 设置项的约束（与 TS 的 `SettingConstraints` 对齐）。
#[derive(Clone, Copy, Debug, Default)]
pub struct SettingConstraints {
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// 标记必须为整数的 number 类型设置。
    pub integer: bool,
}

impl SettingConstraints {
    /// 编码为 JSON 字符串以便存入数据库（与 TS 的 `encodeJson(def.constraints)` 对齐）。
    pub fn to_json(self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        if let Some(min) = self.min {
            m.insert("min".into(), num_value(min));
        }
        if let Some(max) = self.max {
            m.insert("max".into(), num_value(max));
        }
        if self.integer {
            m.insert("integer".into(), serde_json::Value::Bool(true));
        }
        serde_json::Value::Object(m)
    }
}

/// 构造一个 `serde_json::Number`，整数优先使用 i64。
fn num_value(n: f64) -> serde_json::Value {
    if n.fract() == 0.0 && n.is_finite() {
        serde_json::Value::Number(serde_json::Number::from(n as i64))
    } else {
        serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)
    }
}

/// 用于整数范围约束的 const 辅助函数。
const fn int_range(min: f64, max: f64) -> SettingConstraints {
    SettingConstraints {
        min: Some(min),
        max: Some(max),
        integer: true,
    }
}

/// 无约束（用于没有 min/max/integer 的设置项）。
const NO_CONSTRAINTS: SettingConstraints = SettingConstraints {
    min: None,
    max: None,
    integer: false,
};

pub struct SettingDef {
    pub key: &'static str,
    pub ty: SettingType,
    pub category: Category,
    pub description: &'static str,
    pub default_value: &'static str, // 原始字符串，读取时按类型解释
    pub env_key: Option<&'static str>,
    pub constraints: SettingConstraints,
}

// ── 权威定义（移植自 SETTINGS_DEFINITIONS）──────────────────────────

pub const SETTINGS_DEFINITIONS: &[SettingDef] = &[
    SettingDef {
        key: "general.maxIdleSubscriptions",
        ty: SettingType::Number,
        category: Category::General,
        description:
            "Maximum idle thread socket subscriptions retained in the browser before cleanup.",
        default_value: "30",
        env_key: None,
        constraints: int_range(5.0, 200.0),
    },
    SettingDef {
        key: "general.onlyofficeUrl",
        ty: SettingType::String,
        category: Category::General,
        description:
            "OnlyOffice Document Server base URL. Leave empty to use native viewers and disable PPTX preview.",
        default_value: "",
        env_key: None,
        constraints: NO_CONSTRAINTS,
    },
    SettingDef {
        key: "general.onlyofficeJwtSecret",
        ty: SettingType::String,
        category: Category::General,
        description:
            "JWT secret for signing OnlyOffice editor config and verifying save callbacks. \
             Must match the Document Server browser/outbox secret for edit mode.",
        default_value: "",
        env_key: None,
        constraints: NO_CONSTRAINTS,
    },
    SettingDef {
        key: "general.onlyofficeSaveMaxBytes",
        ty: SettingType::Number,
        category: Category::General,
        description:
            "Maximum file size in bytes accepted from OnlyOffice save callback. \
             Increase for large Office documents.",
        default_value: "104857600", // 100 MB
        env_key: None,
        constraints: int_range(1_048_576.0, 1_073_741_824.0),
    },
    SettingDef {
        key: "general.publicBaseUrl",
        ty: SettingType::String,
        category: Category::General,
        description:
            "Public base URL of this WebUI instance (e.g. https://codex.example.com). \
             Used to build document URLs reachable by OnlyOffice. \
             Auto-detected from request headers when empty.",
        default_value: "",
        env_key: None,
        constraints: NO_CONSTRAINTS,
    },
    SettingDef {
        key: "terminal.maxSessions",
        ty: SettingType::Number,
        category: Category::Terminal,
        description: "Maximum concurrent terminal sessions retained by the server.",
        default_value: "10",
        env_key: Some("WEBUI_TERMINAL_MAX_SESSIONS"),
        constraints: int_range(1.0, 50.0),
    },
    SettingDef {
        key: "terminal.graceMs",
        ty: SettingType::Number,
        category: Category::Terminal,
        description: "Milliseconds to keep a detached terminal alive before cleanup.",
        default_value: "45000",
        env_key: Some("WEBUI_TERMINAL_GRACE_MS"),
        constraints: int_range(10_000.0, 300_000.0),
    },
    SettingDef {
        key: "terminal.scrollback",
        ty: SettingType::Number,
        category: Category::Terminal,
        description: "Scrollback lines retained by new terminal buffers.",
        default_value: "5000",
        env_key: Some("WEBUI_TERMINAL_SCROLLBACK"),
        constraints: int_range(100.0, 50_000.0),
    },
    SettingDef {
        key: "terminal.defaultCwd",
        ty: SettingType::String,
        category: Category::Terminal,
        description:
            "Default working directory for new terminals. \
             Must be an existing directory within workspace roots. \
             Empty to use thread cwd or home.",
        default_value: "",
        env_key: Some("DEFAULT_TERMINAL_CWD"),
        constraints: NO_CONSTRAINTS,
    },
    SettingDef {
        key: "files.uploadMaxBytes",
        ty: SettingType::Number,
        category: Category::Files,
        description: "Maximum file upload size in bytes.",
        default_value: "104857600", // 100 MB
        env_key: Some("WEBUI_UPLOAD_MAX_BYTES"),
        constraints: int_range(1.0, 10_737_418_240.0),
    },
    SettingDef {
        key: "files.excludedDirs",
        ty: SettingType::String,
        category: Category::Files,
        description:
            "Comma-separated directory/file names excluded from file tree listings.",
        default_value: "node_modules,.git,.next,dist,__pycache__,.DS_Store",
        env_key: None,
        constraints: NO_CONSTRAINTS,
    },
    SettingDef {
        key: "security.workspaceRoots",
        ty: SettingType::String,
        category: Category::Security,
        description:
            "Comma-separated list of allowed workspace root directories. \
             Home directory is always included.",
        default_value: "",
        env_key: Some("WORKSPACE_ROOTS"),
        constraints: NO_CONSTRAINTS,
    },
];
