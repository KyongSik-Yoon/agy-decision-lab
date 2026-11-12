use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::rules::RuleSet;

/// 외부 규칙 파일이 전혀 없는 환경에서도 단독 동작하도록 기본 규칙 YAML을 바이너리에 내장합니다.
pub const EMBEDDED_DEFAULT_RULES_YAML: &str = include_str!("../rules/default_rules.yaml");

/// 표준 XDG 설정 디렉터리 (`$XDG_CONFIG_HOME/agy-decision-lab` 또는 `~/.config/agy-decision-lab`)
pub fn user_config_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join("agy-decision-lab");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config/agy-decision-lab")
}

/// 표준 사용자 환경설정·시크릿 파일 경로 (`~/.config/agy-decision-lab/config.env`)
pub fn user_config_env_path() -> PathBuf {
    user_config_dir().join("config.env")
}

/// 표준 사용자 커스텀 규칙 파일 경로 (`~/.config/agy-decision-lab/rules/default_rules.yaml`)
pub fn user_rules_path() -> PathBuf {
    user_config_dir().join("rules/default_rules.yaml")
}

/// 런타임 환경변수(`GEMINI_API_KEY` 등)를 표준 경로 우선순위로 로드합니다.
/// 이미 프로세스에 주입된 환경변수는 덮어쓰지 않으며, 실제로 읽은 첫 번째 파일 경로를 반환합니다.
pub fn load_runtime_env() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::with_capacity(4);

    if let Ok(explicit) = std::env::var("AGY_DECISION_ENV_FILE")
        && !explicit.trim().is_empty()
    {
        candidates.push(PathBuf::from(explicit.trim()));
    }

    if let Ok(plugin_data) = std::env::var("PLUGIN_DATA")
        && !plugin_data.trim().is_empty()
    {
        candidates.push(PathBuf::from(plugin_data.trim()).join("config.env"));
    }

    candidates.push(user_config_env_path());

    // 로컬 개발 디렉터리에서 직접 실행할 때만 현재 작업 디렉터리의 `.env`를 보조로 참고 (빌드 머신 절대경로 미포함)
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(".env"));
    }

    let mut first_loaded = None;
    for path in candidates {
        if path.is_file() && dotenvy::from_path(&path).is_ok() && first_loaded.is_none() {
            first_loaded = Some(path);
        }
    }
    first_loaded
}

/// 체크리스트 규칙 세트를 우선순위에 따라 로드하고, 외부 파일이 없으면 내장 YAML로 폴백합니다.
pub fn load_ruleset_with_fallback(explicit_override: Option<&Path>) -> Result<RuleSet> {
    // 1. 호출자가 명시적으로 파일 경로를 지정한 경우에는 실패를 숨기지 않고 그대로 로드
    if let Some(custom) = explicit_override {
        return RuleSet::load_from_path(custom);
    }

    // 2. 표준 후보 경로 탐색
    let mut candidates: Vec<PathBuf> = Vec::with_capacity(4);
    if let Ok(env_path) = std::env::var("AGY_RULES_PATH")
        && !env_path.trim().is_empty()
    {
        candidates.push(PathBuf::from(env_path.trim()));
    }
    if let Ok(plugin_root) = std::env::var("PLUGIN_ROOT")
        && !plugin_root.trim().is_empty()
    {
        candidates.push(PathBuf::from(plugin_root.trim()).join("data/default_rules.yaml"));
    }
    candidates.push(user_rules_path());
    candidates.push(PathBuf::from("rules/default_rules.yaml"));

    for candidate in candidates {
        if candidate.is_file() {
            return RuleSet::load_from_path(&candidate);
        }
    }

    // 3. 외부 파일이 전혀 없어도 바이너리 내장 기본 규칙으로 100% 자립 동작
    RuleSet::from_yaml_str(EMBEDDED_DEFAULT_RULES_YAML)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_default_rules_parse_cleanly() {
        let ruleset = RuleSet::from_yaml_str(EMBEDDED_DEFAULT_RULES_YAML).unwrap();
        assert!(!ruleset.rules.is_empty());
    }

    #[test]
    fn fallback_loader_succeeds_without_external_files() {
        let ruleset = load_ruleset_with_fallback(None).unwrap();
        assert!(!ruleset.rules.is_empty());
    }
}
