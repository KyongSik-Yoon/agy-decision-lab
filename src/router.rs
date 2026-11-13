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

/// 3대 Gemini 4 모델 패밀리(Hydrogen 4 / Argon 4 / Carbon 4) 베이스 설정
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelTargets {
    /// 고속 코딩·실행 패밀리 베이스 (기본: gemini-4-hydrogen -> -low/-medium/-high/-xhigh 자동 조합)
    pub hydrogen_base: String,
    /// 밸런스 개발 패밀리 베이스 (기본: gemini-4-argon -> -low/-medium/-high 자동 조합)
    pub argon_base: String,
    /// 프론티어 추론 패밀리 베이스 (기본: gemini-4-carbon -> -low/-medium/-high 자동 조합)
    pub carbon_base: String,
}

impl ModelTargets {
    pub fn from_env() -> Self {
        let raw_hydrogen = std::env::var("AGY_HYDROGEN_BASE")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                std::env::var("AGY_FLASH_BASE")
                    .ok()
                    .filter(|v| !v.trim().is_empty() && !v.trim().eq_ignore_ascii_case("gemini-3.8-flash"))
            })
            .unwrap_or_else(|| "gemini-4-hydrogen".to_string());
        Self {
            hydrogen_base: strip_effort_suffix(&raw_hydrogen),
            argon_base: strip_effort_suffix(&env_or("AGY_ARGON_BASE", "gemini-4-argon")),
            carbon_base: strip_effort_suffix(&env_or("AGY_CARBON_BASE", "gemini-4-carbon")),
        }
    }

    pub fn supports_xhigh_slug(base: &str) -> bool {
        base.to_ascii_lowercase().contains("hydrogen")
    }

    pub fn variant(&self, family: &str, level: &str) -> String {
        let base = match family {
            "hydrogen" | "flash" => &self.hydrogen_base,
            "argon" => &self.argon_base,
            _ => &self.carbon_base,
        };
        let effective_level = if level == "xhigh" && !Self::supports_xhigh_slug(base) {
            "high"
        } else {
            level
        };
        // gemini-4-hydrogen은 -low/-medium/-high/-xhigh, argon/carbon/claude 등은 -low/-medium/-high 조합
        format!("{base}-{effective_level}")
    }
}

fn strip_effort_suffix(model: &str) -> String {
    model
        .strip_suffix("-xhigh")
        .or_else(|| model.strip_suffix("-low"))
        .or_else(|| model.strip_suffix("-medium"))
        .or_else(|| model.strip_suffix("-high"))
        .unwrap_or(model)
        .to_string()
}

pub fn model_has_baked_effort(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    lower.ends_with("-low")
        || lower.ends_with("-medium")
        || lower.ends_with("-high")
        || lower.ends_with("-xhigh")
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

/// 단일 패스(추가 지연 0ms)로 Hydrogen 4 / Argon 4 / Carbon 4 패밀리와 세부 추론 깊이를 동시 판별합니다.
pub fn build_routing_questions() -> BTreeMap<String, Question> {
    let mut questions = BTreeMap::new();

    questions.insert(
        "task_tier".to_string(),
        Question::Choice {
            instructions: "Which model capability tier best matches this developer request?"
                .to_string(),
            criteria: BTreeMap::from([
                (
                    "hydrogen_quick".to_string(),
                    "Simple Q&A, codebase/file search, log or diff summary, typo/comment/doc edit, formatting, or routine git/shell command.".to_string(),
                ),
                (
                    "hydrogen_exec".to_string(),
                    "Everyday coding, single- or few-file feature implementation, unit test authoring/fixing, clear bug fix, build/CI script update, or verification task.".to_string(),
                ),
                (
                    "argon_feature".to_string(),
                    "Moderate multi-file coordination, cross-component API/SPI wiring, non-trivial query/state logic, or thorough multi-file code review.".to_string(),
                ),
                (
                    "carbon_complex".to_string(),
                    "Large multi-module refactoring, intricate async/state-machine logic, protocol or database schema migration design, or deep security/performance audit.".to_string(),
                ),
                (
                    "carbon_frontier".to_string(),
                    "Complex distributed system architecture, subtle multi-threaded race/deadlock root-cause analysis, or compiler/JVM/kernel internals.".to_string(),
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

    // 1차 매핑: 패밀리(Hydrogen / Argon / Carbon) + 점수 기반 세부 등급(low / medium / high / xhigh) 결정
    let (mut family, mut sub_level) = match task_tier.as_str() {
        "hydrogen_quick" | "flash_quick" => {
            let lvl = if complexity_score < 0.85 {
                "low"
            } else if complexity_score < 1.75 {
                "medium"
            } else {
                "high"
            };
            ("hydrogen", lvl)
        }
        "hydrogen_exec" => {
            let lvl = if complexity_score < 1.10 {
                "medium"
            } else if complexity_score < 2.15 {
                "high"
            } else {
                "xhigh"
            };
            ("hydrogen", lvl)
        }
        "argon_feature" => {
            let lvl = if complexity_score < 1.40 {
                "low"
            } else if complexity_score < 2.40 {
                "medium"
            } else {
                "high"
            };
            ("argon", lvl)
        }
        "carbon_complex" => {
            let lvl = if complexity_score < 1.85 {
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

    // 동일 패밀리 내 하위 티어 간 확률 분산을 보정한 패밀리 통합 확신도 산출
    let hydrogen_prob = tier_probabilities.get("hydrogen_quick").copied().unwrap_or(0.0)
        + tier_probabilities.get("hydrogen_exec").copied().unwrap_or(0.0)
        + tier_probabilities.get("flash_quick").copied().unwrap_or(0.0);
    let argon_prob = tier_probabilities.get("argon_feature").copied().unwrap_or(0.0);
    let carbon_prob = tier_probabilities.get("carbon_complex").copied().unwrap_or(0.0)
        + tier_probabilities.get("carbon_frontier").copied().unwrap_or(0.0);

    let effective_conf = match family {
        "hydrogen" => hydrogen_prob,
        "argon" => argon_prob,
        _ => carbon_prob,
    }
    .max(tier_confidence)
    .clamp(0.0, 1.0);

    // 2차 확신도 게이트: 경계선 질문에서 한 단계 상위 등급·패밀리로 완만하게 승격 (Hydrogen -> Argon -> Carbon)
    let mut escalated = false;
    if effective_conf < threshold {
        match family {
            "hydrogen" => {
                if argon_prob + carbon_prob >= 0.35 {
                    family = "argon";
                    sub_level = if complexity_score < 1.35 {
                        "low"
                    } else {
                        "medium"
                    };
                    escalated = true;
                } else {
                    sub_level = match sub_level {
                        "low" => "medium",
                        "medium" => "high",
                        _ => "xhigh",
                    };
                    escalated = true;
                }
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
        tier_confidence: effective_conf,
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
            hydrogen_base: "gemini-4-hydrogen".to_string(),
            argon_base: "gemini-4-argon".to_string(),
            carbon_base: "gemini-4-carbon".to_string(),
        }
    }

    #[test]
    fn distributes_across_hydrogen_argon_and_carbon_spectrum() {
        let targets = default_targets();

        // 1. Hydrogen exec 고복잡도 -> gemini-4-hydrogen-xhigh (네이티브 xhigh 슬러그)
        let mut a1 = BTreeMap::new();
        a1.insert(
            "task_tier".to_string(),
            Answer::Choice {
                choice: "hydrogen_exec".to_string(),
                probabilities: BTreeMap::from([("hydrogen_exec".to_string(), 0.85)]),
                confidence: 0.85,
            },
        );
        a1.insert(
            "reasoning_effort".to_string(),
            Answer::Score {
                score: 2.35,
                legend: BTreeMap::new(),
                probabilities: BTreeMap::from([("3".to_string(), 0.65)]),
                confidence: 0.65,
            },
        );
        let r1 = resolve_routing_answers(&a1, Some((100, 0)), 200, 0.60, &targets).unwrap();
        assert_eq!(r1.selected_model, "gemini-4-hydrogen-xhigh");
        assert!(r1.cli_effort_flag.is_none());

        // 2. 불확실한 Hydrogen -> Argon 비중이 높으면 Argon으로 완만 승격
        let mut a2 = BTreeMap::new();
        a2.insert(
            "task_tier".to_string(),
            Answer::Choice {
                choice: "hydrogen_quick".to_string(),
                probabilities: BTreeMap::from([
                    ("hydrogen_quick".to_string(), 0.51),
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

        // 3. Hydrogen 내부 확률 분산(quick + exec)은 패밀리 통합 확신도로 인정되어 불필요 승격 방지
        let mut a3 = BTreeMap::new();
        a3.insert(
            "task_tier".to_string(),
            Answer::Choice {
                choice: "hydrogen_exec".to_string(),
                probabilities: BTreeMap::from([
                    ("hydrogen_exec".to_string(), 0.45),
                    ("hydrogen_quick".to_string(), 0.40),
                    ("argon_feature".to_string(), 0.15),
                ]),
                confidence: 0.45,
            },
        );
        a3.insert(
            "reasoning_effort".to_string(),
            Answer::Score {
                score: 1.5,
                legend: BTreeMap::new(),
                probabilities: BTreeMap::from([("2".to_string(), 0.7)]),
                confidence: 0.70,
            },
        );
        let r3 = resolve_routing_answers(&a3, Some((100, 0)), 200, 0.60, &targets).unwrap();
        assert!(!r3.escalated_by_confidence_gate);
        assert!((r3.tier_confidence - 0.85).abs() < 1e-6);
        assert_eq!(r3.selected_model, "gemini-4-hydrogen-high");
    }
}
