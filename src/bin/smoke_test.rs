use std::{collections::BTreeMap, path::PathBuf};

use agy_decision_lab::{
    client::DecisionClient,
    rules::RuleSet,
    types::{Answer, DecisionInput, DecisionRequest, NoulCriteria, Question},
};
use anyhow::{Result, ensure};
use clap::Parser;

/// Gemini Decisions API 단독 연결·지연 시간·스키마 검증 도구
#[derive(Debug, Parser)]
#[command(name = "smoke-test", version, about)]
struct Cli {
    /// 네트워크 호출 없이 로컬 스키마 직렬화 및 기본 YAML 규칙 파일만 오프라인 검증
    #[arg(long)]
    offline: bool,

    /// 검증할 규칙 YAML 경로
    #[arg(long, default_value = "rules/default_rules.yaml")]
    rules_path: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = agy_decision_lab::runtime_config::load_runtime_env();
    let cli = Cli::parse();

    let rule_set = RuleSet::load_from_path(&cli.rules_path)?;
    let (rule_questions, _) = rule_set.build_questions(None);
    println!(
        "[1/3] 규칙 YAML 검증 완료: {}개 규칙 로드됨 ({})",
        rule_questions.len(),
        cli.rules_path.display()
    );

    let sample_diff = r#"
diff --git a/src/db.rs b/src/db.rs
+ const ADMIN_API_TOKEN: &str = "sk-live-9876543210abcdef";
+ let query = format!("DELETE FROM users WHERE id = {}", raw_user_input);
"#;

    let mut questions = BTreeMap::new();
    questions.insert(
        "contains_secret".to_string(),
        Question::Noul {
            instructions: "Does this diff introduce a hardcoded secret or API token?".to_string(),
            criteria: Some(NoulCriteria {
                true_description: "A hardcoded credential or token literal is added.".to_string(),
                false_description: "Secrets come from environment variables or config.".to_string(),
            }),
        },
    );
    questions.insert(
        "review_queue".to_string(),
        Question::Choice {
            instructions: "Which review queue should inspect this diff first?".to_string(),
            criteria: BTreeMap::from([
                (
                    "security_review".to_string(),
                    "Auth, secrets, SQL/command injection, or permission changes.".to_string(),
                ),
                (
                    "style_only".to_string(),
                    "Formatting, comments, or naming cleanup.".to_string(),
                ),
            ]),
        },
    );
    questions.insert(
        "risk_level".to_string(),
        Question::Score {
            instructions: "Rate the production risk of merging this diff.".to_string(),
            criteria: vec![
                "Safe cosmetic change".to_string(),
                "Moderate functional bug risk".to_string(),
                "Severe security or data loss risk".to_string(),
            ],
        },
    );

    let req = DecisionRequest::new(DecisionInput::Text(sample_diff.to_string()), questions);
    req.validate()?;
    println!("[2/3] POST /v1beta/decisions 요청 스키마(noul + choice + score) 검증 완료");

    if cli.offline {
        println!("[3/3] --offline 모드: 실제 네트워크 호출을 건너뛰고 로컬 검증을 마쳤습니다.");
        return Ok(());
    }

    let client = DecisionClient::from_env()?;
    let outcome = client.decide(req).await?;

    if let Some(usage) = &outcome.response.usage {
        ensure!(
            usage.total_output_tokens == 0,
            "출력 토큰이 0이 아닙니다: {}",
            usage.total_output_tokens
        );
    }

    if let Some(Answer::Noul { noul, .. }) = outcome.response.answers.get("contains_secret") {
        ensure!(
            (0.01..=0.99).contains(noul),
            "noul 확률값이 공식 범위(0.01~0.99)를 벗어났습니다: {noul}"
        );
    }

    println!(
        "[3/3] 실시간 API 호출 성공 (왕복 지연 시간: {} ms)",
        outcome.latency_ms
    );
    println!("{}", serde_json::to_string_pretty(&outcome.response)?);
    Ok(())
}
