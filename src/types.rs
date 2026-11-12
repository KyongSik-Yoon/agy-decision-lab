use std::collections::BTreeMap;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

pub const DEFAULT_DECISION_MODEL: &str = "gemini-decision-1";
pub const DEFAULT_DECISIONS_ENDPOINT: &str =
    "https://generativelanguage.googleapis.com/v1beta/decisions";

/// 입력 블록 해상도 옵션 (이미지/비디오 등 멀티모달 토큰 절약용)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MediaResolution {
    Low,
    Medium,
    High,
    UltraHigh,
}

/// 멀티모달/텍스트 공유 입력 블록
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputBlock {
    Text {
        text: String,
    },
    Image {
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
        mime_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        resolution: Option<MediaResolution>,
    },
    Document {
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
        mime_type: String,
    },
    Audio {
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
        mime_type: String,
    },
    Video {
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
        mime_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        resolution: Option<MediaResolution>,
    },
}

/// POST /v1beta/decisions 공유 입력 (단일 문자열 또는 블록 배열)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum DecisionInput {
    Text(String),
    Blocks(Vec<InputBlock>),
}

/// noul(Yes/No) 질문의 선택적 판단 기준 설명
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NoulCriteria {
    pub true_description: String,
    pub false_description: String,
}

/// Gemini Decisions API 공식 질문 타입 (noul / choice / score)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    /// Yes/No 판별 (응답 확률 범위 0.01~0.99 + confidence)
    Noul {
        instructions: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    /// 단일 선택 (2~255개 선택지 맵)
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    /// 루브릭 척도 채점 (0부터 인덱싱되는 정렬 배열, 2~255개)
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
}

/// POST /v1beta/decisions 요청 본문
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRequest {
    pub model: String,
    pub input: DecisionInput,
    pub questions: BTreeMap<String, Question>,
}

impl DecisionRequest {
    pub fn new(input: DecisionInput, questions: BTreeMap<String, Question>) -> Self {
        Self {
            model: DEFAULT_DECISION_MODEL.to_string(),
            input,
            questions,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// 공식 API 제약사항 사전 검증 (질문 1~1,000개, 키 형식, 선택지 2~255개)
    pub fn validate(&self) -> Result<()> {
        let count = self.questions.len();
        ensure!(
            (1..=1000).contains(&count),
            "questions 개수는 1~1,000개 사이여야 합니다 (현재: {count})"
        );

        for (key, question) in &self.questions {
            validate_question_key(key)?;
            match question {
                Question::Noul { instructions, .. } => {
                    ensure!(
                        !instructions.trim().is_empty(),
                        "질문 '{key}'의 instructions가 비어 있습니다"
                    );
                }
                Question::Choice {
                    instructions,
                    criteria,
                } => {
                    ensure!(
                        !instructions.trim().is_empty(),
                        "질문 '{key}'의 instructions가 비어 있습니다"
                    );
                    ensure!(
                        (2..=255).contains(&criteria.len()),
                        "choice 질문 '{key}'의 선택지(criteria)는 2~255개여야 합니다 (현재: {})",
                        criteria.len()
                    );
                }
                Question::Score {
                    instructions,
                    criteria,
                } => {
                    ensure!(
                        !instructions.trim().is_empty(),
                        "질문 '{key}'의 instructions가 비어 있습니다"
                    );
                    ensure!(
                        (2..=255).contains(&criteria.len()),
                        "score 질문 '{key}'의 루브릭 단계(criteria)는 2~255개여야 합니다 (현재: {})",
                        criteria.len()
                    );
                }
            }
        }
        Ok(())
    }
}

pub fn validate_question_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 64 {
        bail!("질문 키 '{key}' 길이는 1~64자여야 합니다");
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("질문 키 '{key}'에는 영숫자, 밑줄(_), 하이픈(-)만 사용할 수 있습니다");
    }
    Ok(())
}

/// 질문별 판별 응답
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Noul {
        /// Yes 확률 (0.01 ~ 0.99)
        noul: f64,
        confidence: f64,
    },
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        /// 확률 가중 기대 점수 (0.0 ~ N-1)
        score: f64,
        legend: BTreeMap<String, String>,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

impl Answer {
    pub fn confidence(&self) -> f64 {
        match self {
            Self::Noul { confidence, .. }
            | Self::Choice { confidence, .. }
            | Self::Score { confidence, .. } => *confidence,
        }
    }

    /// Score 응답에서 가장 높은 확률을 받은 단계 인덱스를 반환합니다.
    pub fn top_score_level(&self) -> Option<usize> {
        match self {
            Self::Score { probabilities, .. } => probabilities
                .iter()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .and_then(|(k, _)| k.parse::<usize>().ok()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModalityTokens {
    pub modality: String,
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageMetadata {
    pub total_input_tokens: u64,
    #[serde(default)]
    pub input_tokens_by_modality: Vec<ModalityTokens>,
    pub total_output_tokens: u64,
    pub total_tokens: u64,
}

/// POST /v1beta/decisions 응답 본문
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionResponse {
    pub answers: BTreeMap<String, Answer>,
    #[serde(default)]
    pub usage: Option<UsageMetadata>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_and_validates_mixed_questions() {
        let mut questions = BTreeMap::new();
        questions.insert(
            "is_defect".to_string(),
            Question::Noul {
                instructions: "Does the diff introduce a security defect?".to_string(),
                criteria: Some(NoulCriteria {
                    true_description: "Hardcoded secret or injection flaw.".to_string(),
                    false_description: "Safe code or test fixture placeholder.".to_string(),
                }),
            },
        );
        questions.insert(
            "route_tier".to_string(),
            Question::Choice {
                instructions: "Which model tier should handle this task?".to_string(),
                criteria: BTreeMap::from([
                    ("fast".to_string(), "Simple edit".to_string()),
                    ("heavy".to_string(), "Deep architecture".to_string()),
                ]),
            },
        );
        questions.insert(
            "risk_level".to_string(),
            Question::Score {
                instructions: "Rate shell command risk.".to_string(),
                criteria: vec![
                    "Read-only".to_string(),
                    "Local write".to_string(),
                    "Destructive".to_string(),
                ],
            },
        );

        let req = DecisionRequest::new(DecisionInput::Text("sample".to_string()), questions);
        req.validate().expect("valid request");

        let json = serde_json::to_value(&req).expect("serialize");
        assert_eq!(json["model"], "gemini-decision-1");
        assert_eq!(json["questions"]["is_defect"]["type"], "noul");
        assert_eq!(json["questions"]["route_tier"]["type"], "choice");
        assert_eq!(json["questions"]["risk_level"]["type"], "score");
    }

    #[test]
    fn deserializes_official_response_shape() {
        let raw = r#"{
          "answers": {
            "is_defect": { "type": "noul", "noul": 0.94, "confidence": 0.94 },
            "next_action": {
              "type": "choice",
              "choice": "human_escalation",
              "probabilities": { "human_escalation": 0.76, "fast_fix": 0.24 },
              "confidence": 0.76
            },
            "severity": {
              "type": "score",
              "score": 1.85,
              "legend": { "0": "Low", "1": "Medium", "2": "High" },
              "probabilities": { "0": 0.02, "1": 0.11, "2": 0.87 },
              "confidence": 0.87
            }
          },
          "usage": {
            "total_input_tokens": 324,
            "input_tokens_by_modality": [{ "modality": "text", "tokens": 324 }],
            "total_output_tokens": 0,
            "total_tokens": 324
          }
        }"#;

        let parsed: DecisionResponse = serde_json::from_str(raw).expect("parse response");
        assert_eq!(parsed.usage.as_ref().unwrap().total_output_tokens, 0);
        assert_eq!(parsed.answers["severity"].top_score_level(), Some(2));
    }
}
