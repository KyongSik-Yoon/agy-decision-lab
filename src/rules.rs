use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::types::{NoulCriteria, Question, validate_question_key};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleSet {
    #[serde(default = "default_threshold")]
    pub default_threshold: f64,
    pub rules: Vec<ChecklistRule>,
}

fn default_threshold() -> f64 {
    0.70
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChecklistRule {
    pub id: String,
    pub category: String,
    pub severity: String,
    /// 긍정문 형태의 질문 (예: "Does the diff contain hardcoded secrets?")
    pub instructions: String,
    #[serde(default)]
    pub true_description: Option<String>,
    #[serde(default)]
    pub false_description: Option<String>,
    #[serde(default)]
    pub threshold: Option<f64>,
}

impl RuleSet {
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path_ref = path.as_ref();
        let raw = std::fs::read_to_string(path_ref)
            .with_context(|| format!("규칙 파일을 읽을 수 없습니다: {}", path_ref.display()))?;
        Self::from_yaml_str(&raw)
    }

    pub fn from_yaml_str(raw: &str) -> Result<Self> {
        let set: Self = serde_yaml_ng::from_str(raw).context("규칙 YAML 파싱 실패")?;
        set.validate()?;
        Ok(set)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.rules.is_empty(), "최소 1개 이상의 규칙이 필요합니다");
        ensure!(
            self.rules.len() <= 1000,
            "단일 요청 규칙 수는 최대 1,000개 이하여야 합니다"
        );
        for rule in &self.rules {
            validate_question_key(&rule.id)?;
            ensure!(
                !rule.instructions.trim().is_empty(),
                "규칙 '{}'의 instructions가 비어 있습니다",
                rule.id
            );
        }
        Ok(())
    }

    /// 카테고리 필터(선택)에 맞는 규칙들을 Decisions API noul 질문 맵으로 변환합니다.
    pub fn build_questions(
        &self,
        category_filter: Option<&str>,
    ) -> (BTreeMap<String, Question>, BTreeMap<String, ChecklistRule>) {
        let mut questions = BTreeMap::new();
        let mut selected = BTreeMap::new();

        for rule in &self.rules {
            if let Some(cat) = category_filter
                && !rule.category.eq_ignore_ascii_case(cat)
            {
                continue;
            }

            let criteria = match (&rule.true_description, &rule.false_description) {
                (Some(t), Some(f)) => Some(NoulCriteria {
                    true_description: t.clone(),
                    false_description: f.clone(),
                }),
                _ => None,
            };

            questions.insert(
                rule.id.clone(),
                Question::Noul {
                    instructions: rule.instructions.clone(),
                    criteria,
                },
            );
            selected.insert(rule.id.clone(), rule.clone());
        }

        (questions, selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rule_yaml_and_builds_noul_questions() {
        let yaml = r#"
default_threshold: 0.72
rules:
  - id: hardcoded_secret
    category: security
    severity: critical
    instructions: "Does the input contain hardcoded credentials, API keys, or private tokens?"
    true_description: "Real-looking secret literals, tokens, or passwords appear in code."
    false_description: "Environment variable lookups or placeholder examples."
"#;
        let set = RuleSet::from_yaml_str(yaml).expect("valid yaml");
        let (questions, meta) = set.build_questions(Some("security"));
        assert_eq!(questions.len(), 1);
        assert!(meta.contains_key("hardcoded_secret"));
    }
}
