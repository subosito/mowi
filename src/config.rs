//! `extensions.mowi` from `extension.config`, plus CLI / env overlays.
//!
//! Precedence: CLI > env > `extensions.mowi` > built-in defaults.

use serde_json::Value;

use crate::theme::ThemeName;

/// Built-in splash tagline when `welcome_message` is absent or blank.
pub const DEFAULT_WELCOME_TAGLINE: &str = "mow with interface";

/// Built-in composer glyph (always stored with a trailing space).
pub const DEFAULT_PROMPT: &str = "❯ ";
/// Composer glyph when colour is off and no override is set.
pub const DEFAULT_PROMPT_PLAIN: &str = "> ";

/// Permission gate: ask before power tools, or run them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Ask,
    Auto,
}

impl PermissionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
        }
    }
}

/// Decoded `extensions.mowi` section. Absent fields stay `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MowiConfig {
    pub permission_mode: Option<PermissionMode>,
    pub theme: Option<ThemeName>,
    pub welcome: Option<bool>,
    pub welcome_message: Option<String>,
    pub prompt: Option<String>,
}

/// CLI and env overlays. `Some` means that layer set a value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserSources {
    pub permission_mode: Option<PermissionMode>,
    pub theme: Option<ThemeName>,
}

/// Values after CLI > env > pack > defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConfig {
    pub permission_mode: PermissionMode,
    pub theme: ThemeName,
    pub welcome: bool,
    pub welcome_message: String,
    pub prompt: Option<String>,
}

impl ResolvedConfig {
    /// Composer prefix, always ending in a single space.
    pub fn prompt_prefix(&self, colored: bool) -> String {
        normalize_prompt(self.prompt.as_deref(), colored)
    }
}

impl Default for ResolvedConfig {
    fn default() -> Self {
        Self {
            permission_mode: PermissionMode::Ask,
            theme: ThemeName::CatppuccinMocha,
            welcome: true,
            welcome_message: String::new(),
            prompt: None,
        }
    }
}

/// Parse `ask` / `auto`. Unknown tokens are absent, not an error.
pub fn parse_permission_mode(raw: &str) -> Option<PermissionMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "ask" => Some(PermissionMode::Ask),
        "auto" => Some(PermissionMode::Auto),
        _ => None,
    }
}

/// Parse a full theme identifier. Unknown names are absent.
pub fn parse_theme_name(raw: &str) -> Option<ThemeName> {
    raw.trim().parse().ok()
}

/// Read `$MOW_THEME`. Empty / unset is `Ok(None)`; a known name is `Ok(Some)`.
/// An unknown name is `Err` with the same listing `--theme` prints.
pub fn env_theme() -> Result<Option<ThemeName>, String> {
    match std::env::var("MOW_THEME") {
        Err(_) => Ok(None),
        Ok(raw) if raw.trim().is_empty() => Ok(None),
        Ok(raw) => raw.trim().parse().map(Some),
    }
}

/// Read `$MOW_PERMISSION_MODE`. Empty / unset is `Ok(None)`.
pub fn env_permission_mode() -> Result<Option<PermissionMode>, String> {
    match std::env::var("MOW_PERMISSION_MODE") {
        Err(_) => Ok(None),
        Ok(raw) if raw.trim().is_empty() => Ok(None),
        Ok(raw) => parse_permission_mode(&raw)
            .map(Some)
            .ok_or_else(|| format!("unknown permission mode {:?}; use ask or auto", raw.trim())),
    }
}

/// Decode an `extension.config` result for `name: "mowi"`.
///
/// Accepts the section object itself, or `{config:{…}}` / `{mowi:{…}}`
/// wrappers. Unknown fields are ignored.
pub fn decode_mowi_config(value: &Value) -> MowiConfig {
    let section = config_section(value);
    MowiConfig {
        permission_mode: section
            .get("permission_mode")
            .and_then(Value::as_str)
            .and_then(parse_permission_mode),
        theme: section
            .get("theme")
            .and_then(Value::as_str)
            .and_then(parse_theme_name),
        welcome: section.get("welcome").and_then(as_bool),
        welcome_message: section
            .get("welcome_message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty()),
        prompt: section
            .get("prompt")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty()),
    }
}

fn config_section(value: &Value) -> &Value {
    if let Some(config) = value.get("config").filter(|v| v.is_object()) {
        return config;
    }
    if let Some(mowi) = value.get("mowi").filter(|v| v.is_object()) {
        return mowi;
    }
    value
}

fn as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        Value::Number(n) => n.as_i64().map(|n| n != 0),
        _ => None,
    }
}

/// Splash tagline from a configured message; blank keeps the built-in line.
pub fn welcome_tagline(message: &str) -> &str {
    first_nonempty_line(message).unwrap_or(DEFAULT_WELCOME_TAGLINE)
}

fn first_nonempty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

fn normalize_prompt(prompt: Option<&str>, colored: bool) -> String {
    if let Some(raw) = prompt {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return format!("{} ", trimmed.trim_end());
        }
    }
    if colored {
        DEFAULT_PROMPT.to_string()
    } else {
        DEFAULT_PROMPT_PLAIN.to_string()
    }
}

/// Merge user overlays onto a pack section. Missing user/pack fields
/// fall through to built-in defaults (`ask`, mocha, welcome on).
pub fn resolve_config(user: &UserSources, pack: &MowiConfig) -> ResolvedConfig {
    ResolvedConfig {
        permission_mode: user
            .permission_mode
            .or(pack.permission_mode)
            .unwrap_or(PermissionMode::Ask),
        theme: user
            .theme
            .or(pack.theme)
            .unwrap_or(ThemeName::CatppuccinMocha),
        welcome: pack.welcome.unwrap_or(true),
        welcome_message: pack.welcome_message.clone().unwrap_or_default(),
        prompt: pack.prompt.clone(),
    }
}

/// CLI `--ask` / `--auto` beat env. Both absent leaves `None`.
/// `--auto` wins when both flags are set (same as the previous UI default).
pub fn cli_permission_mode(ask: bool, auto: bool) -> Option<PermissionMode> {
    if auto {
        Some(PermissionMode::Auto)
    } else if ask {
        Some(PermissionMode::Ask)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(json: &str) -> MowiConfig {
        decode_mowi_config(&serde_json::from_str(json).unwrap())
    }

    #[test]
    fn decode_canonical_section() {
        let c = pack(
            r#"{
                "permission_mode": "auto",
                "theme": "gruvbox-dark",
                "welcome": false,
                "welcome_message": "hello pack",
                "prompt": "❯"
            }"#,
        );
        assert_eq!(c.permission_mode, Some(PermissionMode::Auto));
        assert_eq!(c.theme, Some(ThemeName::GruvboxDark));
        assert_eq!(c.welcome, Some(false));
        assert_eq!(c.welcome_message.as_deref(), Some("hello pack"));
        assert_eq!(c.prompt.as_deref(), Some("❯"));
    }

    #[test]
    fn decode_wrapped_config_and_mowi_objects() {
        let via_config = pack(r#"{"name":"mowi","config":{"theme":"monokai"}}"#);
        assert_eq!(via_config.theme, Some(ThemeName::Monokai));

        let via_name = pack(r#"{"mowi":{"permission_mode":"ask"}}"#);
        assert_eq!(via_name.permission_mode, Some(PermissionMode::Ask));
    }

    #[test]
    fn decode_ignores_unknown_and_invalid_fields() {
        let c = pack(
            r#"{
                "keys": {"send": "enter"},
                "theme": {"name": "monokai"},
                "permission_mode": "always",
                "extra": true
            }"#,
        );
        assert_eq!(c, MowiConfig::default());
    }

    #[test]
    fn decode_blank_and_invalid_fields_are_absent() {
        let c = pack(
            r#"{
                "permission_mode": "ASK",
                "theme": "solarized",
                "welcome": "yes",
                "welcome_message": "   ",
                "prompt": "  "
            }"#,
        );
        assert_eq!(c.permission_mode, Some(PermissionMode::Ask));
        assert_eq!(c.theme, None);
        assert_eq!(c.welcome, Some(true));
        assert_eq!(c.welcome_message, None);
        assert_eq!(c.prompt, None);
    }

    #[test]
    fn decode_empty_object_is_defaults() {
        assert_eq!(
            decode_mowi_config(&serde_json::json!({})),
            MowiConfig::default()
        );
        assert_eq!(
            decode_mowi_config(&serde_json::json!({"name": "mowi"})),
            MowiConfig::default()
        );
    }

    #[test]
    fn resolve_cli_beats_env_and_pack() {
        let pack = MowiConfig {
            permission_mode: Some(PermissionMode::Auto),
            theme: Some(ThemeName::Monokai),
            welcome: Some(false),
            welcome_message: Some("from pack".into()),
            prompt: Some("$".into()),
        };
        let user = UserSources {
            permission_mode: Some(PermissionMode::Ask),
            theme: Some(ThemeName::GruvboxDark),
        };
        let got = resolve_config(&user, &pack);
        assert_eq!(got.permission_mode, PermissionMode::Ask);
        assert_eq!(got.theme, ThemeName::GruvboxDark);
        assert!(!got.welcome);
        assert_eq!(welcome_tagline(&got.welcome_message), "from pack");
        assert_eq!(got.prompt_prefix(true), "$ ");
    }

    #[test]
    fn resolve_pack_beats_builtins() {
        let pack = MowiConfig {
            permission_mode: Some(PermissionMode::Auto),
            theme: Some(ThemeName::CatppuccinLatte),
            welcome: Some(true),
            welcome_message: None,
            prompt: None,
        };
        let got = resolve_config(&UserSources::default(), &pack);
        assert_eq!(got.permission_mode, PermissionMode::Auto);
        assert_eq!(got.theme, ThemeName::CatppuccinLatte);
        assert!(got.welcome);
        assert_eq!(
            welcome_tagline(&got.welcome_message),
            DEFAULT_WELCOME_TAGLINE
        );
        assert_eq!(got.prompt_prefix(true), DEFAULT_PROMPT);
        assert_eq!(got.prompt_prefix(false), DEFAULT_PROMPT_PLAIN);
    }

    #[test]
    fn resolve_absent_layers_use_builtins() {
        let got = resolve_config(&UserSources::default(), &MowiConfig::default());
        assert_eq!(got, ResolvedConfig::default());
        assert_eq!(got.permission_mode, PermissionMode::Ask);
        assert_eq!(got.theme, ThemeName::CatppuccinMocha);
        assert!(got.welcome);
    }

    #[test]
    fn cli_ask_auto_distinguish_explicit_from_absent() {
        assert_eq!(cli_permission_mode(false, false), None);
        assert_eq!(cli_permission_mode(true, false), Some(PermissionMode::Ask));
        assert_eq!(cli_permission_mode(false, true), Some(PermissionMode::Auto));
        assert_eq!(cli_permission_mode(true, true), Some(PermissionMode::Auto));
    }

    #[test]
    fn user_theme_beats_pack_when_env_or_cli_set() {
        let pack = MowiConfig {
            theme: Some(ThemeName::Monokai),
            ..MowiConfig::default()
        };
        let from_env = UserSources {
            theme: Some(ThemeName::GruvboxDark),
            ..UserSources::default()
        };
        assert_eq!(
            resolve_config(&from_env, &pack).theme,
            ThemeName::GruvboxDark
        );
        assert_eq!(
            resolve_config(&UserSources::default(), &pack).theme,
            ThemeName::Monokai
        );
    }

    #[test]
    fn prompt_prefix_trims_and_keeps_one_space() {
        let resolved = ResolvedConfig {
            prompt: Some("❯  ".into()),
            ..ResolvedConfig::default()
        };
        assert_eq!(resolved.prompt_prefix(true), "❯ ");
        let empty = ResolvedConfig {
            prompt: Some("   ".into()),
            ..ResolvedConfig::default()
        };
        assert_eq!(empty.prompt_prefix(true), DEFAULT_PROMPT);
    }

    #[test]
    fn welcome_tagline_uses_first_nonempty_line() {
        let resolved = ResolvedConfig {
            welcome_message: "  \nhello pack\nmore".into(),
            ..ResolvedConfig::default()
        };
        assert_eq!(welcome_tagline(&resolved.welcome_message), "hello pack");
    }
}
