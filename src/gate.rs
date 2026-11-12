use std::{collections::BTreeMap, time::Duration};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    client::DecisionClient,
    router::env_or,
    types::{
        Answer, DEFAULT_DECISION_MODEL, DEFAULT_DECISIONS_ENDPOINT, DecisionInput, DecisionRequest,
        NoulCriteria, Question,
    },
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateEvaluation {
    /// "allow" | "ask" | "force_ask" | "deny"
    pub action: String,
    pub fast_path: bool,
    pub reason: String,
    pub risk_score: f64,
    pub risk_confidence: f64,
    pub destructive_probability: f64,
    pub secret_exposure_probability: f64,
    pub latency_ms: u128,
}

/// 셸 메타문자·체이닝·리다이렉션·민감 파일 접근이 없는 순수 조회/로컬 검증 명령인지 0ms로 판별합니다.
pub fn is_zero_ms_safe_command(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    if trimmed.is_empty() || trimmed.len() > 240 {
        return false;
    }

    // 파이프, 체이닝, 리다이렉션, 명령 치환, 백그라운드, 줄바꿈 포함 시 정밀 채점으로 넘김
    let forbidden_chars = [';', '&', '|', '>', '<', '`', '\n', '\r'];
    if trimmed.chars().any(|c| forbidden_chars.contains(&c)) || trimmed.contains("$(") {
        return false;
    }

    // 민감 경로 키워드가 인자에 포함되면 정밀 채점으로 넘김
    let lower = trimmed.to_ascii_lowercase();
    let sensitive_tokens = [
        ".env",
        ".ssh",
        "id_rsa",
        "id_ed25519",
        "shadow",
        "keepass",
        "secret",
        "token",
        "credential",
        "apikey",
        "api_key",
    ];
    if sensitive_tokens.iter().any(|tok| lower.contains(tok)) {
        return false;
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let Some(&first) = parts.first() else {
        return false;
    };
    let bin = first.rsplit('/').next().unwrap_or(first);

    match bin {
        "pwd" | "whoami" | "uname" | "date" | "uptime" | "df" | "free" | "ls" | "ll" | "tree"
        | "wc" | "stat" | "file" | "which" | "type" | "realpath" | "dirname" | "basename" => true,
        "cat" | "head" | "tail" | "bat" | "less" => true,
        "rg" | "grep" | "fd" | "jq" => true,
        "find" => !parts
            .iter()
            .any(|p| matches!(*p, "-delete" | "-exec" | "-execdir" | "-ok" | "-fprint")),
        "git" => match parts.get(1).copied() {
            Some("status" | "diff" | "log" | "show" | "rev-parse" | "ls-files" | "blame") => true,
            Some("branch") => !parts
                .iter()
                .skip(2)
                .any(|p| matches!(*p, "-d" | "-D" | "-m" | "-M" | "--delete")),
            _ => false,
        },
        "cargo" => matches!(
            parts.get(1).copied(),
            Some("check" | "clippy" | "fmt" | "test" | "build" | "metadata" | "tree" | "--version" | "-V")
        ),
        "rustc" | "rustup" => parts.get(1).copied() == Some("--version"),
        _ => false,
    }
}

pub async fn evaluate_command_for_hook(
    command: &str,
    cwd: Option<&str>,
    timeout: Duration,
) -> Result<GateEvaluation> {
    if is_zero_ms_safe_command(command) {
        return Ok(GateEvaluation {
            action: "allow".to_string(),
            fast_path: true,
            reason: "단순 읽기 전용/검증 명령 로컬 패스트패스 허용 (0ms)".to_string(),
            risk_score: 0.0,
            risk_confidence: 1.0,
            destructive_probability: 0.0,
            secret_exposure_probability: 0.0,
            latency_ms: 0,
        });
    }

    let _ = crate::runtime_config::load_runtime_env();
    let api_key = std::env::var("GEMINI_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty() && v != "your_ai_studio_api_key_here")
        .ok_or_else(|| anyhow::anyhow!("GEMINI_API_KEY 미설정"))?;

    let endpoint = env_or("GEMINI_DECISIONS_ENDPOINT", DEFAULT_DECISIONS_ENDPOINT);
    let model = env_or("GEMINI_DECISION_MODEL", DEFAULT_DECISION_MODEL);
    let client = DecisionClient::new(api_key, endpoint, model.clone(), timeout)?;

    let combined_input = match cwd {
        Some(dir) if !dir.trim().is_empty() => {
            format!("Working directory: {dir}\nCandidate shell command:\n{command}")
        }
        _ => format!("Candidate shell command:\n{command}"),
    };

    let mut questions = BTreeMap::new();
    questions.insert(
        "risk_score".to_string(),
        Question::Score {
            instructions:
                "Rate the operational and security blast radius of executing this shell command."
                    .to_string(),
            criteria: vec![
                "Read-only inspection or harmless local query with zero side effects.".to_string(),
                "Scoped reversible local workspace build, test, or file edit.".to_string(),
                "External network mutation, git push, package installation, or service restart."
                    .to_string(),
                "Irreversible data/file deletion, history wipe, privilege escalation, or credential exfiltration."
                    .to_string(),
            ],
        },
    );
    questions.insert(
        "is_destructive".to_string(),
        Question::Noul {
            instructions: "Can this command irreversibly delete, overwrite, or corrupt files, git history, databases, or system state?".to_string(),
            criteria: Some(NoulCriteria {
                true_description: "Uses recursive/force deletion, hard reset, force push, disk/table wipe, or destructive overwrite.".to_string(),
                false_description: "Read-only inspection, normal build/test, or reversible workspace operation.".to_string(),
            }),
        },
    );
    questions.insert(
        "exposes_secrets".to_string(),
        Question::Noul {
            instructions: "Does this command read, print, or transmit secret keys, tokens, .env credentials, or private authentication files over stdout or network?".to_string(),
            criteria: None,
        },
    );

    let req = DecisionRequest::new(DecisionInput::Text(combined_input), questions).with_model(model);
    let outcome = client.decide(req).await?;

    let (risk_score, risk_conf) = match outcome.response.answers.get("risk_score") {
        Some(Answer::Score {
            score, confidence, ..
        }) => (*score, *confidence),
        _ => (3.0, 0.0),
    };
    let destructive_prob = match outcome.response.answers.get("is_destructive") {
        Some(Answer::Noul { noul, .. }) => *noul,
        _ => 0.99,
    };
    let secret_prob = match outcome.response.answers.get("exposes_secrets") {
        Some(Answer::Noul { noul, .. }) => *noul,
        _ => 0.99,
    };

    let (action, reason) = decide_hook_gate_action(
        risk_score,
        risk_conf,
        destructive_prob,
        secret_prob,
        outcome.latency_ms,
    );

    Ok(GateEvaluation {
        action,
        fast_path: false,
        reason,
        risk_score,
        risk_confidence: risk_conf,
        destructive_probability: destructive_prob,
        secret_exposure_probability: secret_prob,
        latency_ms: outcome.latency_ms,
    })
}

pub fn decide_hook_gate_action(
    risk_score: f64,
    risk_conf: f64,
    destructive_prob: f64,
    secret_prob: f64,
    latency_ms: u128,
) -> (String, String) {
    // 극단적 파괴·유출 명령(예: rm -rf /, 시크릿 외부 전송 등)은 즉시 하드 차단(deny)
    if (risk_score >= 2.75 && destructive_prob >= 0.90)
        || (risk_score >= 2.65 && secret_prob >= 0.90)
    {
        return (
            "deny".to_string(),
            format!(
                "[Decision Gate 차단] 고위험 파괴·유출 명령 감지 (risk={risk_score:.2}/3, 파괴확률={:.0}%, 비밀노출={:.0}%, {latency_ms}ms)",
                destructive_prob * 100.0,
                secret_prob * 100.0
            ),
        );
    }

    // 비가역 변경·외부 발신·시크릿 접근 의심 시 always-proceed 상태에서도 사용자 확인창 강제(force_ask)
    if risk_score >= 2.00
        || destructive_prob >= 0.55
        || secret_prob >= 0.50
        || (risk_score >= 1.65 && risk_conf < 0.60)
    {
        return (
            "force_ask".to_string(),
            format!(
                "[Decision Gate 확인 필요] risk={risk_score:.2}/3 (확신도 {:.0}%), 파괴확률={:.0}%, 비밀노출={:.0}% ({latency_ms}ms)",
                risk_conf * 100.0,
                destructive_prob * 100.0,
                secret_prob * 100.0
            ),
        );
    }

    // 일반 로컬 개발·빌드·테스트·가역적 편집 명령은 무소음 자동 허용(allow)하여 always-proceed 및 agy-route 흐름을 방해하지 않음
    (
        "allow".to_string(),
        format!("로컬 안전/표준 작업 허용 (risk={risk_score:.2}/3, {latency_ms}ms)"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_path_allows_clean_readonly_and_rejects_chains_or_secrets() {
        assert!(is_zero_ms_safe_command("git status"));
        assert!(is_zero_ms_safe_command("git diff --stat src/"));
        assert!(is_zero_ms_safe_command("cargo check --offline"));
        assert!(is_zero_ms_safe_command("cargo test --lib"));
        assert!(is_zero_ms_safe_command("rg DecisionClient src/"));

        assert!(!is_zero_ms_safe_command("cat .env"));
        assert!(!is_zero_ms_safe_command("git status && rm -rf target"));
        assert!(!is_zero_ms_safe_command("find . -name '*.tmp' -delete"));
        assert!(!is_zero_ms_safe_command("git push --force origin main"));
    }

    #[test]
    fn gate_thresholds_distinguish_allow_force_ask_and_deny() {
        let (act0, _) = decide_hook_gate_action(0.3, 0.92, 0.03, 0.02, 210);
        assert_eq!(act0, "allow");

        let (act1, _) = decide_hook_gate_action(1.3, 0.85, 0.20, 0.05, 220);
        assert_eq!(act1, "allow");

        let (act2, _) = decide_hook_gate_action(2.1, 0.88, 0.65, 0.05, 250);
        assert_eq!(act2, "force_ask");

        let (act3, _) = decide_hook_gate_action(2.92, 0.95, 0.96, 0.10, 260);
        assert_eq!(act3, "deny");
    }
}
