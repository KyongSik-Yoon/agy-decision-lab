use std::{collections::BTreeMap, path::PathBuf};

use agy_decision_lab::{
    client::DecisionClient,
    rules::RuleSet,
    types::{
        Answer, DecisionInput, DecisionRequest, InputBlock, MediaResolution, NoulCriteria, Question,
    },
};
use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STD};
use rmcp::{
    ServiceExt, handler::server::wrapper::Parameters, schemars, tool, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ExtraRuleInput {
    /// 영숫자, 밑줄(_), 하이픈(-)으로 구성된 고유 규칙 키 (최대 64자)
    pub id: String,
    /// 긍정문 형태의 Yes/No 판별 질문 (예: "Does the diff modify database schema?")
    pub instructions: String,
    /// 위반 판정 임계 확률 (기본값: 0.72, 유효 범위: 0.01~0.99)
    pub threshold: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScanChecklistArgs {
    /// 검사할 코드 diff, 로그, 설정 텍스트 또는 문서 본문
    pub content: String,
    /// 특정 카테고리만 필터링할 경우 지정 (예: security, safety, reliability, performance, compatibility)
    pub category: Option<String>,
    /// 기본 규칙 파일 경로 재정의 (생략 시 AGY_RULES_PATH 또는 ./rules/default_rules.yaml 사용)
    pub rules_path: Option<String>,
    /// 전역 위반 임계 확률 재정의 (0.01~0.99)
    pub threshold_override: Option<f64>,
    /// 호출 시 즉석에서 함께 검사할 추가 noul 질문 목록
    pub extra_checks: Option<Vec<ExtraRuleInput>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GateCommandArgs {
    /// 실행 전 위험도와 정책 적합도를 평가할 셸 명령 또는 스크립트
    pub command: String,
    /// 현재 작업 디렉터리 또는 실행 목적 등 부가 컨텍스트 (선택)
    pub context: Option<String>,
    /// 자동 실행 허용 최소 확신도 임계치 (기본값: 0.75)
    pub min_confidence: Option<f64>,
    /// 자동 실행 허용 최대 위험 점수 상한 (0.0~3.0 척도, 기본값: 1.20)
    pub max_auto_risk_score: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EvaluateDecisionsArgs {
    /// 평가 대상 텍스트 입력 (local_file_path와 함께 전달 시 보조 설명으로 결합됨)
    pub text_input: Option<String>,
    /// 로컬 이미지 또는 PDF 파일 절대 경로 (지정 시 base64 인라인 블록으로 함께 전송)
    pub local_file_path: Option<String>,
    /// 로컬 파일의 MIME 타입 (예: application/pdf, image/png, image/jpeg)
    pub file_mime_type: Option<String>,
    /// Decisions API questions 맵의 JSON 문자열 (noul / choice / score 혼합 가능, 최대 1,000개)
    pub questions_json: String,
}

#[derive(Debug, Serialize)]
struct RuleFinding {
    id: String,
    category: String,
    severity: String,
    triggered: bool,
    noul_probability: f64,
    confidence: f64,
    threshold: f64,
    instructions: String,
}

#[derive(Clone)]
struct DecisionMcpServer {
    default_rules_path: PathBuf,
}

impl DecisionMcpServer {
    fn new() -> Self {
        let default_rules_path = std::env::var("AGY_RULES_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("rules/default_rules.yaml"));
        Self { default_rules_path }
    }

    fn resolve_rules_path(&self, override_path: Option<&str>) -> PathBuf {
        override_path
            .filter(|p| !p.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.default_rules_path.clone())
    }
}

#[tool_router(server_handler)]
impl DecisionMcpServer {
    #[tool(
        description = "Scan a git diff, source file, or technical document against multiple security, safety, and engineering convention rules in a single Gemini Decision 1 pass (noul probabilities 0.01..0.99 + confidence)."
    )]
    async fn scan_code_checklist(&self, Parameters(args): Parameters<ScanChecklistArgs>) -> String {
        match self.run_scan_checklist(args).await {
            Ok(val) => serde_json::to_string_pretty(&val).unwrap_or_else(|e| e.to_string()),
            Err(err) => json_error(&format!("{err:#}")),
        }
    }

    #[tool(
        description = "Evaluate a shell command or deployment script before execution using Gemini Decision 1 rubric scoring (score 0..3) and confidence gating. Returns ALLOW_AUTO, REQUIRE_HUMAN_CONFIRMATION, or BLOCK."
    )]
    async fn gate_shell_command(&self, Parameters(args): Parameters<GateCommandArgs>) -> String {
        match self.run_gate_command(args).await {
            Ok(val) => serde_json::to_string_pretty(&val).unwrap_or_else(|e| e.to_string()),
            Err(err) => json_error(&format!("{err:#}")),
        }
    }

    #[tool(
        description = "Run custom typed Gemini Decisions API questions (noul, choice, score; up to 1,000 questions in one pass) over text or a local PDF/image file."
    )]
    async fn evaluate_decisions(
        &self,
        Parameters(args): Parameters<EvaluateDecisionsArgs>,
    ) -> String {
        match self.run_evaluate_decisions(args).await {
            Ok(val) => serde_json::to_string_pretty(&val).unwrap_or_else(|e| e.to_string()),
            Err(err) => json_error(&format!("{err:#}")),
        }
    }
}

impl DecisionMcpServer {
    async fn run_scan_checklist(&self, args: ScanChecklistArgs) -> Result<serde_json::Value> {
        let rules_path = self.resolve_rules_path(args.rules_path.as_deref());
        let rule_set = RuleSet::load_from_path(&rules_path)?;
        let (mut questions, rule_meta) = rule_set.build_questions(args.category.as_deref());

        let mut extra_thresholds = BTreeMap::new();
        if let Some(extras) = args.extra_checks {
            for item in extras {
                extra_thresholds.insert(
                    item.id.clone(),
                    (
                        item.instructions.clone(),
                        item.threshold.unwrap_or(rule_set.default_threshold),
                    ),
                );
                questions.insert(
                    item.id,
                    Question::Noul {
                        instructions: item.instructions,
                        criteria: None,
                    },
                );
            }
        }

        let client = DecisionClient::from_env()?;
        let req = DecisionRequest::new(DecisionInput::Text(args.content), questions)
            .with_model(client.default_model());
        let outcome = client.decide(req).await?;

        let mut findings = Vec::new();
        for (id, ans) in &outcome.response.answers {
            if let Answer::Noul { noul, confidence } = ans {
                if let Some(meta) = rule_meta.get(id) {
                    let threshold = args
                        .threshold_override
                        .or(meta.threshold)
                        .unwrap_or(rule_set.default_threshold);
                    findings.push(RuleFinding {
                        id: id.clone(),
                        category: meta.category.clone(),
                        severity: meta.severity.clone(),
                        triggered: *noul >= threshold,
                        noul_probability: *noul,
                        confidence: *confidence,
                        threshold,
                        instructions: meta.instructions.clone(),
                    });
                } else if let Some((inst, default_t)) = extra_thresholds.get(id) {
                    let threshold = args.threshold_override.unwrap_or(*default_t);
                    findings.push(RuleFinding {
                        id: id.clone(),
                        category: "custom".to_string(),
                        severity: "custom".to_string(),
                        triggered: *noul >= threshold,
                        noul_probability: *noul,
                        confidence: *confidence,
                        threshold,
                        instructions: inst.clone(),
                    });
                }
            }
        }

        findings.sort_by(|a, b| b.noul_probability.total_cmp(&a.noul_probability));
        let triggered_count = findings.iter().filter(|f| f.triggered).count();

        Ok(serde_json::json!({
            "status": "ok",
            "summary": {
                "total_rules_checked": findings.len(),
                "triggered_count": triggered_count,
                "latency_ms": outcome.latency_ms,
                "usage": outcome.response.usage,
            },
            "triggered_rules": findings.iter().filter(|f| f.triggered).collect::<Vec<_>>(),
            "all_results": findings,
        }))
    }

    async fn run_gate_command(&self, args: GateCommandArgs) -> Result<serde_json::Value> {
        let min_confidence = args.min_confidence.unwrap_or(0.75);
        let max_auto_risk = args.max_auto_risk_score.unwrap_or(1.20);

        let combined_input = match args.context {
            Some(ctx) if !ctx.trim().is_empty() => {
                format!("Context:\n{ctx}\n\nCandidate shell command:\n{}", args.command)
            }
            _ => format!("Candidate shell command:\n{}", args.command),
        };

        let mut questions = BTreeMap::new();
        questions.insert(
            "risk_score".to_string(),
            Question::Score {
                instructions:
                    "Rate the operational and security blast radius of running this shell command."
                        .to_string(),
                criteria: vec![
                    "Read-only inspection or safe local status query with no side effects."
                        .to_string(),
                    "Scoped local build, test, or reversible workspace file edit.".to_string(),
                    "System package change, network mutation, git push, or service restart."
                        .to_string(),
                    "Irreversible deletion, privilege escalation, credential access, or production mutation."
                        .to_string(),
                ],
            },
        );
        questions.insert(
            "is_destructive".to_string(),
            Question::Noul {
                instructions: "Can this command delete, overwrite, or corrupt data, history, or system state irreversibly?".to_string(),
                criteria: Some(NoulCriteria {
                    true_description: "Deletes files/branches/tables, force-pushes, formats disks, or kills critical daemons.".to_string(),
                    false_description: "Read-only commands, builds, tests, or reversible local edits.".to_string(),
                }),
            },
        );
        questions.insert(
            "exfiltrates_or_exposes_secrets".to_string(),
            Question::Noul {
                instructions: "Does this command read, print, or transmit credentials, SSH keys, tokens, or environment secrets?".to_string(),
                criteria: None,
            },
        );

        let client = DecisionClient::from_env()?;
        let req = DecisionRequest::new(DecisionInput::Text(combined_input), questions)
            .with_model(client.default_model());
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
        let secret_prob = match outcome.response.answers.get("exfiltrates_or_exposes_secrets") {
            Some(Answer::Noul { noul, .. }) => *noul,
            _ => 0.99,
        };

        let gate_decision = if risk_score >= 2.5 || destructive_prob >= 0.80 || secret_prob >= 0.80
        {
            "BLOCK_OR_EXPLICIT_APPROVAL_REQUIRED"
        } else if risk_conf < min_confidence
            || risk_score > max_auto_risk
            || destructive_prob >= 0.35
            || secret_prob >= 0.35
        {
            "REQUIRE_HUMAN_CONFIRMATION"
        } else {
            "ALLOW_AUTO"
        };

        Ok(serde_json::json!({
            "status": "ok",
            "decision": gate_decision,
            "metrics": {
                "risk_score": risk_score,
                "risk_confidence": risk_conf,
                "destructive_probability": destructive_prob,
                "secret_exposure_probability": secret_prob,
                "min_confidence_threshold": min_confidence,
                "max_auto_risk_score": max_auto_risk,
                "latency_ms": outcome.latency_ms,
            },
            "raw_answers": outcome.response.answers,
            "usage": outcome.response.usage,
        }))
    }

    async fn run_evaluate_decisions(
        &self,
        args: EvaluateDecisionsArgs,
    ) -> Result<serde_json::Value> {
        let questions: BTreeMap<String, Question> = serde_json::from_str(&args.questions_json)
            .context("questions_json 파싱 실패: 유효한 Decisions API 질문 맵 JSON이어야 합니다")?;

        let input = if let Some(path_str) = args.local_file_path.as_deref()
            && !path_str.trim().is_empty()
        {
            let bytes = tokio::fs::read(path_str)
                .await
                .with_context(|| format!("로컬 파일 읽기 실패: {path_str}"))?;
            let encoded = BASE64_STD.encode(bytes);
            let mime = args
                .file_mime_type
                .unwrap_or_else(|| guess_mime_type(path_str).to_string());

            let mut blocks = Vec::new();
            if let Some(txt) = args.text_input
                && !txt.trim().is_empty()
            {
                blocks.push(InputBlock::Text {
                    text: format!("{txt}\n"),
                });
            }
            if mime == "application/pdf" {
                blocks.push(InputBlock::Document {
                    data: Some(encoded),
                    uri: None,
                    mime_type: mime,
                });
            } else {
                blocks.push(InputBlock::Image {
                    data: Some(encoded),
                    uri: None,
                    mime_type: mime,
                    resolution: Some(MediaResolution::Medium),
                });
            }
            DecisionInput::Blocks(blocks)
        } else {
            DecisionInput::Text(args.text_input.unwrap_or_default())
        };

        let client = DecisionClient::from_env()?;
        let req = DecisionRequest::new(input, questions).with_model(client.default_model());
        let outcome = client.decide(req).await?;

        Ok(serde_json::json!({
            "status": "ok",
            "latency_ms": outcome.latency_ms,
            "answers": outcome.response.answers,
            "usage": outcome.response.usage,
        }))
    }
}

fn guess_mime_type(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".pdf") {
        "application/pdf"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else {
        "image/jpeg"
    }
}

fn json_error(message: &str) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "status": "error",
        "message": message,
    }))
    .unwrap_or_else(|_| format!("{{\"status\":\"error\",\"message\":\"{message}\"}}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    // stdio MCP 서버는 stdout을 JSON-RPC 전용으로 사용하므로 모든 로그는 반드시 stderr로 기록합니다.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let server = DecisionMcpServer::new();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
