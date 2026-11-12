use std::{collections::BTreeMap, time::Duration};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    client::DecisionClient,
    types::{
        Answer, DEFAULT_DECISION_MODEL, DEFAULT_DECISIONS_ENDPOINT, DecisionInput, DecisionRequest,
        InputBlock, MediaResolution, Question,
    },
};

#[derive(Debug, Clone)]
pub struct ImageAttachment {
    pub path: std::path::PathBuf,
    pub mime_type: String,
    pub base64_data: String,
    pub byte_len: usize,
}

/// 3대 모델 패밀리(Flash 3.8 / Argon 4 / Carbon 4) 베이스 설정
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelTargets {
    /// 경량 패밀리 베이스 (기본: gemini-3.8-flash -> -low/-medium/-high 자동 조합)
    pub flash_base: String,
    /// 밸런스 개발 패밀리 베이스 (기본: gemini-4-argon -> -low/-medium/-high 자동 조합)
    pub argon_base: String,
    /// 프론티어 추론 패밀리 베이스 (기본: gemini-4-carbon -> -low/-medium/-high 자동 조합)
    pub carbon_base: String,
}

impl ModelTargets {
    pub fn from_env() -> Self {
        Self {
            flash_base: strip_effort_suffix(&env_or("AGY_FLASH_BASE", "gemini-3.8-flash")),
            argon_base: strip_effort_suffix(&env_or("AGY_ARGON_BASE", "gemini-4-argon")),
            carbon_base: strip_effort_suffix(&env_or("AGY_CARBON_BASE", "gemini-4-carbon")),
        }
    }

    pub fn variant(&self, family: &str, level: &str) -> String {
        let base = match family {
            "flash" => &self.flash_base,
            "argon" => &self.argon_base,
            _ => &self.carbon_base,
        };
        // claude-sonnet-5-5, gemini-4-argon 등 표준 패밀리는 -low/-medium/-high 접미사를 결합
        format!("{base}-{level}")
    }
}

fn strip_effort_suffix(model: &str) -> String {
    model
        .strip_suffix("-low")
        .or_else(|| model.strip_suffix("-medium"))
        .or_else(|| model.strip_suffix("-high"))
        .unwrap_or(model)
        .to_string()
}

pub fn model_has_baked_effort(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    lower.ends_with("-low") || lower.ends_with("-medium") || lower.ends_with("-high")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteDecisionSummary {
    pub selected_model: String,
    pub selected_family: String,
    pub cli_effort_flag: Option<String>,
    pub recommended_effort: String,
    pub task_tier: String,
    pub tier_confidence: f64,
    pub tier_probabilities: BTreeMap<String, f64>,
    pub complexity_score: f64,
    pub complexity_confidence: f64,
    pub escalated_by_confidence_gate: bool,
    pub latency_ms: u128,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

pub fn effort_from_level(level: usize) -> &'static str {
    match level {
        0 => "low",
        1 => "medium",
        2 => "high",
        3 => "xhigh",
        _ => "max",
    }
}

/// 단일 패스(추가 지연 0ms)로 Flash 3.8 / Argon 4 / Carbon 4 패밀리와 세부 추론 깊이를 동시 판별합니다.
pub fn build_routing_questions() -> BTreeMap<String, Question> {
    let mut questions = BTreeMap::new();

    questions.insert(
        "task_tier".to_string(),
        Question::Choice {
            instructions: "Which model capability tier best matches this developer request?"
                .to_string(),
            criteria: BTreeMap::from([
                (
                    "flash_quick".to_string(),
                    "Simple Q&A, casual chat, file/diff summary, typo or comment edit, formatting, boilerplate snippet, or simple shell command.".to_string(),
                ),
                (
                    "argon_feature".to_string(),
                    "Routine feature implementation, unit test authoring, straightforward bug fix with clear symptoms, CLI/API wiring, or localized refactoring.".to_string(),
                ),
                (
                    "carbon_complex".to_string(),
                    "Multi-module refactoring, intricate async/state-machine logic, protocol/schema design, or deep security and performance review.".to_string(),
                ),
                (
                    "carbon_frontier".to_string(),
                    "Complex system architecture design, subtle multi-threaded race/deadlock root-cause analysis, or JVM bytecode/compiler/kernel internals.".to_string(),
                ),
            ]),
        },
    );

    questions.insert(
        "reasoning_effort".to_string(),
        Question::Score {
            instructions:
                "How much multi-step reasoning depth is required to complete this request accurately?"
                    .to_string(),
            criteria: vec![
                "Trivial lookup, greeting, or mechanical text/format edit.".to_string(),
                "Localized single-file code change or direct explanation.".to_string(),
                "Moderate multi-file coordination and edge-case checking.".to_string(),
                "Deep cross-module state, concurrency, or trade-off synthesis.".to_string(),
                "Exhaustive formal invariant, concurrency/deadlock, or bytecode/kernel root-cause verification.".to_string(),
            ],
        },
    );

    questions
}

pub async fn classify_prompt_with_timeout(
    prompt: &str,
    threshold: f64,
    targets: &ModelTargets,
    timeout: Duration,
) -> Result<RouteDecisionSummary> {
    classify_prompt_with_media_timeout(prompt, &[], threshold, targets, timeout).await
}

pub async fn classify_prompt_with_media_timeout(
    prompt: &str,
    images: &[ImageAttachment],
    threshold: f64,
    targets: &ModelTargets,
    timeout: Duration,
) -> Result<RouteDecisionSummary> {
    let _ = crate::runtime_config::load_runtime_env();
    let api_key = std::env::var("GEMINI_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty() && v != "your_ai_studio_api_key_here")
        .ok_or_else(|| anyhow::anyhow!("GEMINI_API_KEY 환경변수가 설정되지 않았습니다"))?;

    let endpoint = env_or("GEMINI_DECISIONS_ENDPOINT", DEFAULT_DECISIONS_ENDPOINT);
    let model = env_or("GEMINI_DECISION_MODEL", DEFAULT_DECISION_MODEL);
    let client = DecisionClient::new(api_key, endpoint, model.clone(), timeout)?;

    let decision_input = if images.is_empty() {
        DecisionInput::Text(prompt.to_string())
    } else {
        let mut blocks = Vec::with_capacity(images.len() + 1);
        for img in images {
            blocks.push(InputBlock::Image {
                data: Some(img.base64_data.clone()),
                uri: None,
                mime_type: img.mime_type.clone(),
                resolution: Some(MediaResolution::Low),
            });
        }
        blocks.push(InputBlock::Text {
            text: prompt.to_string(),
        });
        DecisionInput::Blocks(blocks)
    };

    let req = DecisionRequest::new(decision_input, build_routing_questions()).with_model(model);

    let outcome = client.decide(req).await?;
    resolve_routing_answers(
        &outcome.response.answers,
        outcome
            .response
            .usage
            .as_ref()
            .map(|u| (u.total_input_tokens, u.total_output_tokens)),
        outcome.latency_ms,
        threshold,
        targets,
    )
}

pub fn resolve_routing_answers(
    answers: &BTreeMap<String, Answer>,
    usage_tokens: Option<(u64, u64)>,
    latency_ms: u128,
    threshold: f64,
    targets: &ModelTargets,
) -> Result<RouteDecisionSummary> {
    let (task_tier, tier_probabilities, tier_confidence) = match answers.get("task_tier") {
        Some(Answer::Choice {
            choice,
            probabilities,
            confidence,
        }) => (choice.clone(), probabilities.clone(), *confidence),
        other => bail!("예상치 못한 task_tier 응답 형식: {other:?}"),
    };

    let (complexity_score, complexity_confidence, top_level) = match answers.get("reasoning_effort")
    {
        Some(
            ans @ Answer::Score {
                score, confidence, ..
            },
        ) => (*score, *confidence, ans.top_score_level().unwrap_or(1)),
        other => bail!("예상치 못한 reasoning_effort 응답 형식: {other:?}"),
    };

    let rounded_score_level = complexity_score.round().clamp(0.0, 4.0) as usize;
    let effort_level = rounded_score_level.max(top_level);

    // 1차 매핑: 패밀리(Flash / Argon / Carbon) + 점수 기반 세부 등급(low / medium / high) 결정
    let (mut family, mut sub_level) = match task_tier.as_str() {
        "flash_quick" => {
            let lvl = if complexity_score < 0.75 {
                "low"
            } else if complexity_score < 1.65 {
                "medium"
            } else {
                "high"
            };
            ("flash", lvl)
        }
        "argon_feature" => {
            let lvl = if complexity_score < 1.15 {
                "low"
            } else if complexity_score < 2.25 {
                "medium"
            } else {
                "high"
            };
            ("argon", lvl)
        }
        "carbon_complex" => {
            let lvl = if complexity_score < 1.80 {
                "low"
            } else if complexity_score < 3.10 {
                "medium"
            } else {
                "high"
            };
            ("carbon", lvl)
        }
        _ => {
            // carbon_frontier
            let lvl = if complexity_score < 2.55 {
                "medium"
            } else {
                "high"
            };
            ("carbon", lvl)
        }
    };

    // 2차 확신도 게이트: 경계선 질문에서 한 단계 상위 패밀리로 완만하게 승격 (Flash -> Argon -> Carbon)
    let mut escalated = false;
    if tier_confidence < threshold {
        let carbon_prob = tier_probabilities.get("carbon_complex").copied().unwrap_or(0.0)
            + tier_probabilities.get("carbon_frontier").copied().unwrap_or(0.0);
        match family {
            "flash" => {
                family = "argon";
                sub_level = if complexity_score < 1.2 { "low" } else { "medium" };
                escalated = true;
            }
            "argon" if carbon_prob >= 0.30 => {
                family = "carbon";
                sub_level = "medium";
                escalated = true;
            }
            _ => {}
        }
    }

    let selected_model = targets.variant(family, sub_level);
    let recommended_effort = effort_from_level(effort_level).to_string();
    let cli_effort_flag = (!model_has_baked_effort(&selected_model)).then(|| recommended_effort.clone());
    let (input_tokens, output_tokens) = usage_tokens.unwrap_or((0, 0));

    Ok(RouteDecisionSummary {
        selected_model,
        selected_family: family.to_string(),
        cli_effort_flag,
        recommended_effort,
        task_tier,
        tier_confidence,
        tier_probabilities,
        complexity_score,
        complexity_confidence,
        escalated_by_confidence_gate: escalated,
        latency_ms,
        input_tokens,
        output_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_targets() -> ModelTargets {
        ModelTargets {
            flash_base: "gemini-3.8-flash".to_string(),
            argon_base: "gemini-4-argon".to_string(),
            carbon_base: "gemini-4-carbon".to_string(),
        }
    }

    #[test]
    fn distributes_across_flash_argon_and_carbon_spectrum() {
        let targets = default_targets();

        // 1. Argon feature -> gemini-4-argon-medium
        let mut a1 = BTreeMap::new();
        a1.insert(
            "task_tier".to_string(),
            Answer::Choice {
                choice: "argon_feature".to_string(),
                probabilities: BTreeMap::from([("argon_feature".to_string(), 0.88)]),
                confidence: 0.88,
            },
        );
        a1.insert(
            "reasoning_effort".to_string(),
            Answer::Score {
                score: 1.6,
                legend: BTreeMap::new(),
                probabilities: BTreeMap::from([("2".to_string(), 0.6)]),
                confidence: 0.60,
            },
        );
        let r1 = resolve_routing_answers(&a1, Some((100, 0)), 200, 0.60, &targets).unwrap();
        assert_eq!(r1.selected_model, "gemini-4-argon-medium");
        assert!(r1.cli_effort_flag.is_none());

        // 2. Uncertain Flash -> escalates smoothly to Argon instead of jumping to Carbon
        let mut a2 = BTreeMap::new();
        a2.insert(
            "task_tier".to_string(),
            Answer::Choice {
                choice: "flash_quick".to_string(),
                probabilities: BTreeMap::from([
                    ("flash_quick".to_string(), 0.51),
                    ("argon_feature".to_string(), 0.49),
                ]),
                confidence: 0.51,
            },
        );
        a2.insert(
            "reasoning_effort".to_string(),
            Answer::Score {
                score: 0.9,
                legend: BTreeMap::new(),
                probabilities: BTreeMap::from([("1".to_string(), 0.8)]),
                confidence: 0.80,
            },
        );
        let r2 = resolve_routing_answers(&a2, Some((100, 0)), 200, 0.60, &targets).unwrap();
        assert!(r2.escalated_by_confidence_gate);
        assert_eq!(r2.selected_model, "gemini-4-argon-low");
    }
}
