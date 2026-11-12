use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;

use crate::types::{
    DEFAULT_DECISION_MODEL, DEFAULT_DECISIONS_ENDPOINT, DecisionRequest, DecisionResponse,
};

#[derive(Debug, Clone)]
pub struct DecisionClient {
    http: Client,
    api_key: String,
    endpoint: String,
    default_model: String,
}

#[derive(Debug, Clone)]
pub struct CallOutcome {
    pub response: DecisionResponse,
    pub latency_ms: u128,
}

#[derive(Debug, Deserialize)]
struct ApiErrorEnvelope {
    error: ApiErrorDetail,
}

#[derive(Debug, Deserialize)]
struct ApiErrorDetail {
    #[serde(default)]
    code: serde_json::Value,
    message: String,
}

impl DecisionClient {
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();
        let api_key = std::env::var("GEMINI_API_KEY")
            .ok()
            .filter(|v| !v.trim().is_empty() && v != "your_ai_studio_api_key_here")
            .context(
                "GEMINI_API_KEY 환경변수가 설정되지 않았습니다 (.env 또는 프로세스 환경변수를 확인하세요)",
            )?;

        let endpoint = std::env::var("GEMINI_DECISIONS_ENDPOINT")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_DECISIONS_ENDPOINT.to_string());

        let default_model = std::env::var("GEMINI_DECISION_MODEL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_DECISION_MODEL.to_string());

        Self::new(api_key, endpoint, default_model, Duration::from_secs(15))
    }

    pub fn new(
        api_key: impl Into<String>,
        endpoint: impl Into<String>,
        default_model: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self> {
        let http = Client::builder()
            .timeout(timeout)
            .build()
            .context("HTTP 클라이언트 초기화 실패")?;

        Ok(Self {
            http,
            api_key: api_key.into(),
            endpoint: endpoint.into(),
            default_model: default_model.into(),
        })
    }

    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub async fn decide(&self, mut request: DecisionRequest) -> Result<CallOutcome> {
        if request.model.trim().is_empty() {
            request.model = self.default_model.clone();
        }
        request.validate()?;

        let started = Instant::now();
        let resp = self
            .http
            .post(&self.endpoint)
            .header("x-goog-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .with_context(|| format!("Decisions API 호출 실패 ({})", self.endpoint))?;

        let status = resp.status();
        let body = resp.text().await.context("응답 본문 수신 실패")?;
        let latency_ms = started.elapsed().as_millis();

        if !status.is_success() {
            if let Ok(err_env) = serde_json::from_str::<ApiErrorEnvelope>(&body) {
                bail!(
                    "Decisions API 오류 (HTTP {status}, code={}): {}",
                    err_env.error.code,
                    err_env.error.message
                );
            }
            bail!("Decisions API 오류 (HTTP {status}): {body}");
        }

        let parsed: DecisionResponse = serde_json::from_str(&body)
            .with_context(|| format!("Decisions API JSON 파싱 실패: {body}"))?;

        Ok(CallOutcome {
            response: parsed,
            latency_ms,
        })
    }
}
