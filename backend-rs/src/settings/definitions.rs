//! Runtime setting definitions — ported from `src/settings/settings.definitions.ts`.
//!
//! 12 settings across 4 categories (terminal, files, security, general).
//! `default_value` is always a string (matches the DB `value` column type).
//! Constraints and full SettingType port deferred to Phase 2 (settings CRUD).

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

pub struct SettingDef {
    pub key: &'static str,
    pub ty: SettingType,
    pub category: Category,
    pub description: &'static str,
    pub default_value: &'static str, // always a string; parsed by SettingsReader
    pub env_key: Option<&'static str>,
}

// ── Authoritative definitions (port of SETTINGS_DEFINITIONS) ──────────────────

pub const SETTINGS_DEFINITIONS: &[SettingDef] = &[
    SettingDef {
        key: "general.maxIdleSubscriptions",
        ty: SettingType::Number,
        category: Category::General,
        description:
            "Maximum idle thread socket subscriptions retained in the browser before cleanup.",
        default_value: "30",
        env_key: None,
    },
    SettingDef {
        key: "general.onlyofficeUrl",
        ty: SettingType::String,
        category: Category::General,
        description:
            "OnlyOffice Document Server base URL. Leave empty to use native viewers and disable PPTX preview.",
        default_value: "",
        env_key: None,
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
    },
    SettingDef {
        key: "terminal.maxSessions",
        ty: SettingType::Number,
        category: Category::Terminal,
        description: "Maximum concurrent terminal sessions retained by the server.",
        default_value: "10",
        env_key: Some("WEBUI_TERMINAL_MAX_SESSIONS"),
    },
    SettingDef {
        key: "terminal.graceMs",
        ty: SettingType::Number,
        category: Category::Terminal,
        description: "Milliseconds to keep a detached terminal alive before cleanup.",
        default_value: "45000",
        env_key: Some("WEBUI_TERMINAL_GRACE_MS"),
    },
    SettingDef {
        key: "terminal.scrollback",
        ty: SettingType::Number,
        category: Category::Terminal,
        description: "Scrollback lines retained by new terminal buffers.",
        default_value: "5000",
        env_key: Some("WEBUI_TERMINAL_SCROLLBACK"),
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
    },
    SettingDef {
        key: "files.uploadMaxBytes",
        ty: SettingType::Number,
        category: Category::Files,
        description: "Maximum file upload size in bytes.",
        default_value: "104857600", // 100 MB
        env_key: Some("WEBUI_UPLOAD_MAX_BYTES"),
    },
    SettingDef {
        key: "files.excludedDirs",
        ty: SettingType::String,
        category: Category::Files,
        description:
            "Comma-separated directory/file names excluded from file tree listings.",
        default_value: "node_modules,.git,.next,dist,__pycache__,.DS_Store",
        env_key: None,
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
    },
];
