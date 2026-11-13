use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    fs::{self, File},
    hash::{Hash, Hasher},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    client::DecisionClient,
    router::env_or,
    types::{
        Answer, DEFAULT_DECISION_MODEL, DEFAULT_DECISIONS_ENDPOINT, DecisionInput, DecisionRequest,
    },
};

/// Stop 훅 입력에서 추출한 최소 컨텍스트
#[derive(Debug, Clone)]
pub struct StopHookContext {
    pub conversation_id: String,
    pub cwd: String,
    pub transcript_path: String,
    pub termination_reason: String,
    pub continuation_active: bool,
    pub fully_idle: bool,
    pub execution_num: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggeredStopRule {
    pub id: String,
    pub severity: String,
    pub probability: f64,
    pub confidence: f64,
    pub instructions: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopGateEvaluation {
    /// "allow" (정상 종료 허용) | "continue" (위반 사항 수정 후 재종료 요청)
    pub decision: String,
    pub fast_path: bool,
    pub reason: Option<String>,
    pub checked_rules: usize,
    pub triggered_rules: Vec<TriggeredStopRule>,
    pub diff_bytes: usize,
    pub latency_ms: u128,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StopDedupState {
    sessions: BTreeMap<String, BTreeMap<String, u64>>,
}

const MAX_DIFF_CHARS: usize = 16_000;
const MIN_STOP_CONFIDENCE: f64 = 0.68;
const TRANSCRIPT_TAIL_BYTES: u64 = 262_144;
const MAX_DEDUP_SESSIONS: usize = 64;
const MAX_DEDUP_FILES_PER_SESSION: usize = 512;

/// Stop 훅 체크리스트 평가를 수행합니다.
/// - 재진입 턴(`continuation_active` / `execution_num > 0`), 비정상 종료 사유, diff 없음, 이미 검사한 동일 diff는 모두 0ms로 통과시킵니다.
pub async fn evaluate_stop_hook(
    ctx: &StopHookContext,
    rules_path_override: Option<&Path>,
    use_dedup_cache: bool,
    timeout: Duration,
) -> Result<StopGateEvaluation> {
    // 1. 정상 완료(NO_TOOL_CALL)가 아닌 경우 즉시 종료 허용
    if !ctx.termination_reason.is_empty() && ctx.termination_reason != "NO_TOOL_CALL" {
        return Ok(allow_fast("정상 턴 종료(NO_TOOL_CALL) 아님 — 스킵"));
    }

    // 1-2. 백그라운드 태스크/서브에이전트 대기 중인 경우 즉시 종료 허용
    if !ctx.fully_idle {
        return Ok(allow_fast(
            "백그라운드 태스크/에이전트 대기 중(fullyIdle=false) — 대기 종료 허용",
        ));
    }

    // 2. 이미 Stop 훅에 의해 한 번 연장된 턴이면 무한 루프 방지를 위해 즉시 종료 허용
    if ctx.continuation_active || ctx.execution_num > 0 {
        return Ok(allow_fast("이미 재검증(continuation) 수행 완료 — 루프 방지 통과"));
    }

    // 2-2. 이번 턴에 변경/실행 도구를 호출하지 않은 읽기 전용 턴이면 git 실행 없이 즉시 통과
    let transcript_trimmed = ctx.transcript_path.trim();
    if !transcript_trimmed.is_empty() {
        let t_path = Path::new(transcript_trimmed);
        if t_path.is_file() && !transcript_modified_workspace_in_latest_turn(t_path) {
            return Ok(allow_fast("이번 턴에 변경/실행 도구 호출 없음 — 0ms 통과"));
        }
    }

    // 3. Git 작업 트리 변경분(diff) 추출 — 코드 변경이 없는 일반 대화/탐색 턴은 0ms 통과
    let cwd = if ctx.cwd.trim().is_empty() {
        "."
    } else {
        ctx.cwd.trim()
    };
    let Some(diff_text) = collect_workspace_git_diff(Path::new(cwd)) else {
        return Ok(allow_fast("Git 변경분(diff) 없음 — 0ms 통과"));
    };

    // 4. 파일 단위 증분 diff 분리 및 동일 세션 내 이미 검증된 파일 블록 제외
    let use_session_dedup = use_dedup_cache && !ctx.conversation_id.trim().is_empty();
    let (scan_diff, checked_chunks): (String, Vec<(String, u64)>) = if use_session_dedup {
        let checked_map = load_session_checked_map(&ctx.conversation_id);
        filter_unchecked_diff_chunks(&diff_text, &checked_map)
    } else {
        (diff_text.clone(), Vec::new())
    };

    if use_session_dedup && scan_diff.is_empty() {
        return Ok(allow_fast("변경된 모든 파일 diff가 이미 검증됨 — 0ms 통과"));
    }

    let diff_bytes = scan_diff.len();

    // 5. 규칙 로드(외부 파일 미존재 시 내장 규칙 자동 폴백) 및 critical / high 핵심 규칙만 선별해 오탐 최소화
    let rule_set = crate::runtime_config::load_ruleset_with_fallback(rules_path_override)?;
    let (all_questions, rule_meta) = rule_set.build_questions(None);

    let mut high_impact_questions = BTreeMap::new();
    for (id, q) in all_questions {
        if let Some(meta) = rule_meta.get(&id)
            && matches!(meta.severity.to_ascii_lowercase().as_str(), "critical" | "high")
        {
            high_impact_questions.insert(id, q);
        }
    }

    if high_impact_questions.is_empty() {
        return Ok(allow_fast("활성화된 critical/high 규칙 없음"));
    }

    let _ = crate::runtime_config::load_runtime_env();
    let api_key = std::env::var("GEMINI_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty() && v != "your_ai_studio_api_key_here")
        .ok_or_else(|| anyhow::anyhow!("GEMINI_API_KEY 미설정"))?;

    let endpoint = env_or("GEMINI_DECISIONS_ENDPOINT", DEFAULT_DECISIONS_ENDPOINT);
    let model = env_or("GEMINI_DECISION_MODEL", DEFAULT_DECISION_MODEL);
    let client = DecisionClient::new(api_key, endpoint, model.clone(), timeout)?;

    let truncated_diff = truncate_utf8(&scan_diff, MAX_DIFF_CHARS);
    let input_payload = format!("Workspace git diff to audit before turn completion:\n{truncated_diff}");
    let checked_count = high_impact_questions.len();

    let req = DecisionRequest::new(DecisionInput::Text(input_payload), high_impact_questions)
        .with_model(model);
    let outcome = client.decide(req).await?;

    if use_session_dedup && !checked_chunks.is_empty() {
        record_checked_chunks(&ctx.conversation_id, &checked_chunks);
    }

    let mut triggered = Vec::new();
    for (id, ans) in &outcome.response.answers {
        if let Answer::Noul { noul, confidence } = ans
            && let Some(meta) = rule_meta.get(id)
        {
            let base_threshold = meta.threshold.unwrap_or(rule_set.default_threshold);
            // Stop 자동 개입은 오탐 피로도를 낮추기 위해 기본 임계치보다 보수적(최소 0.78) + 확신도 게이트 적용
            let strict_threshold = base_threshold.max(0.78);
            if *noul >= strict_threshold && *confidence >= MIN_STOP_CONFIDENCE {
                triggered.push(TriggeredStopRule {
                    id: id.clone(),
                    severity: meta.severity.clone(),
                    probability: *noul,
                    confidence: *confidence,
                    instructions: meta.instructions.clone(),
                });
            }
        }
    }

    triggered.sort_by(|a, b| b.probability.total_cmp(&a.probability));

    if triggered.is_empty() {
        return Ok(StopGateEvaluation {
            decision: "allow".to_string(),
            fast_path: false,
            reason: None,
            checked_rules: checked_count,
            triggered_rules: Vec::new(),
            diff_bytes,
            latency_ms: outcome.latency_ms,
        });
    }

    let summary_items: Vec<String> = triggered
        .iter()
        .map(|r| {
            format!(
                "{}({}, 확률 {:.0}%/확신도 {:.0}%: {})",
                r.id,
                r.severity,
                r.probability * 100.0,
                r.confidence * 100.0,
                r.instructions
            )
        })
        .collect();

    let reason = format!(
        "[Decision Stop Gate] 작업 트리의 git diff에서 보안·안전 체크리스트 위반 가능성이 감지되었습니다: {}. 실제 코드 문맥을 확인해 문제 소지가 맞다면 수정하고, 의도된 안전한 코드라면 근거를 한 줄로 덧붙인 뒤 완료해 주세요.",
        summary_items.join("; ")
    );

    Ok(StopGateEvaluation {
        decision: "continue".to_string(),
        fast_path: false,
        reason: Some(reason),
        checked_rules: checked_count,
        triggered_rules: triggered,
        diff_bytes,
        latency_ms: outcome.latency_ms,
    })
}

fn allow_fast(note: &str) -> StopGateEvaluation {
    StopGateEvaluation {
        decision: "allow".to_string(),
        fast_path: true,
        reason: Some(note.to_string()),
        checked_rules: 0,
        triggered_rules: Vec::new(),
        diff_bytes: 0,
        latency_ms: 0,
    }
}

/// Git 저장소의 staged + unstaged 코드 변경분을 통합 조회합니다.
pub fn collect_workspace_git_diff(cwd: &Path) -> Option<String> {
    let check = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !check.status.success() {
        return None;
    }

    // HEAD 대비 변경분 조회 (신규 초기화 저장소여서 HEAD가 없으면 일반 git diff 폴백)
    let diff_out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["--no-optional-locks", "diff", "--unified=1", "--no-color", "HEAD"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .or_else(|| {
            Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(["--no-optional-locks", "diff", "--unified=1", "--no-color"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()
        })?;

    let text = String::from_utf8_lossy(&diff_out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn hash_str(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn is_readonly_or_wait_tool(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "read_file"
            | "view_file"
            | "view_file_outline"
            | "view_code_item"
            | "grep_search"
            | "find_by_name"
            | "list_dir"
            | "search_web"
            | "read_url_content"
            | "list_resources"
            | "read_resource"
            | "ask_question"
            | "manage_task"
            | "manage_subagents"
            | "schedule"
            | "exit_plan_mode"
            | "enter_plan_mode"
    )
}

/// 트랜스크립트 끝부분(최대 256KB)을 역순 탐색해 마지막 사용자 입력(`USER_EXPLICIT`) 이후
/// 작업 트리를 변경할 수 있는 도구가 호출되었는지 판정합니다. 판별 불능 시 안전하게 true 반환.
pub fn transcript_modified_workspace_in_latest_turn(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return true;
    };
    let Ok(meta) = file.metadata() else {
        return true;
    };
    let file_len = meta.len();
    let skip_first_partial = file_len > TRANSCRIPT_TAIL_BYTES;
    if skip_first_partial {
        if file
            .seek(SeekFrom::End(-(TRANSCRIPT_TAIL_BYTES as i64)))
            .is_err()
        {
            return true;
        }
    }

    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return true;
    }
    let tail_str = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = tail_str.lines().collect();
    if skip_first_partial && !lines.is_empty() {
        lines.remove(0);
    }

    for line in lines.iter().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };

        if let Some(tool_calls) = val.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tool_calls {
                if let Some(name) = tc.get("name").and_then(|v| v.as_str())
                    && !is_readonly_or_wait_tool(name)
                {
                    return true;
                }
            }
        }

        let step_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let source = val.get("source").and_then(|v| v.as_str()).unwrap_or("");
        if step_type == "USER_INPUT" && source == "USER_EXPLICIT" {
            return false;
        }
    }

    true
}

/// 통합 git diff를 파일 단위로 분할해 `(파일경로, 블록원문, 해시)` 목록으로 반환합니다.
pub fn split_git_diff_by_file(diff_text: &str) -> Vec<(String, String, u64)> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for line in diff_text.split_inclusive('\n') {
        if line.starts_with("diff --git ") && !current.is_empty() {
            chunks.push(build_diff_chunk(std::mem::take(&mut current)));
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        chunks.push(build_diff_chunk(current));
    }
    chunks
}

fn build_diff_chunk(block: String) -> (String, String, u64) {
    let first_line = block.lines().next().unwrap_or("");
    let file_key = extract_diff_file_key(first_line);
    let trimmed = block.trim_end().to_string();
    let h = hash_str(&trimmed);
    (file_key, trimmed, h)
}

fn extract_diff_file_key(header: &str) -> String {
    if let Some(rest) = header.strip_prefix("diff --git ") {
        if let Some((_, b_side)) = rest.rsplit_once(" b/") {
            return b_side.trim().to_string();
        }
        if let Some(last_tok) = rest.split_whitespace().last() {
            return last_tok
                .strip_prefix("b/")
                .unwrap_or(last_tok)
                .trim()
                .to_string();
        }
    }
    header.trim().to_string()
}

pub fn filter_unchecked_diff_chunks(
    diff_text: &str,
    checked_map: &BTreeMap<String, u64>,
) -> (String, Vec<(String, u64)>) {
    let chunks = split_git_diff_by_file(diff_text);
    let mut unseen_blocks = Vec::new();
    let mut newly_checked = Vec::new();

    for (file_path, block, h) in chunks {
        if checked_map.get(&file_path).copied() == Some(h) {
            continue;
        }
        unseen_blocks.push(block);
        newly_checked.push((file_path, h));
    }

    (unseen_blocks.join("\n"), newly_checked)
}

fn dedup_file_path() -> PathBuf {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(runtime).join("agy-decision-stop-dedup.json")
}

fn load_session_checked_map(conv_id: &str) -> BTreeMap<String, u64> {
    let Ok(raw) = fs::read_to_string(dedup_file_path()) else {
        return BTreeMap::new();
    };
    let Ok(state) = serde_json::from_str::<StopDedupState>(&raw) else {
        return BTreeMap::new();
    };
    state.sessions.get(conv_id).cloned().unwrap_or_default()
}

fn record_checked_chunks(conv_id: &str, checked_chunks: &[(String, u64)]) {
    let path = dedup_file_path();
    let mut state = fs::read_to_string(&path)
        .ok()
        .and_then(|r| serde_json::from_str::<StopDedupState>(&r).ok())
        .unwrap_or_default();

    if state.sessions.len() >= MAX_DEDUP_SESSIONS && !state.sessions.contains_key(conv_id) {
        state.sessions.clear();
    }

    let entry = state.sessions.entry(conv_id.to_string()).or_default();
    if entry.len() + checked_chunks.len() > MAX_DEDUP_FILES_PER_SESSION {
        entry.clear();
    }
    for (file_path, h) in checked_chunks {
        entry.insert(file_path.clone(), *h);
    }

    if let Ok(serialized) = serde_json::to_string(&state) {
        let _ = crate::runtime_config::write_private_file(&path, serialized.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn skips_immediately_when_continuation_is_already_active() {
        let ctx = StopHookContext {
            conversation_id: "conv-1".to_string(),
            cwd: "/tmp".to_string(),
            transcript_path: String::new(),
            termination_reason: "NO_TOOL_CALL".to_string(),
            continuation_active: true,
            fully_idle: true,
            execution_num: 1,
        };
        let eval = evaluate_stop_hook(&ctx, None, false, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(eval.decision, "allow");
        assert!(eval.fast_path);
    }

    #[tokio::test]
    async fn skips_immediately_on_non_normal_termination() {
        let ctx = StopHookContext {
            conversation_id: "conv-1".to_string(),
            cwd: "/tmp".to_string(),
            transcript_path: String::new(),
            termination_reason: "USER_CANCELED".to_string(),
            continuation_active: false,
            fully_idle: true,
            execution_num: 0,
        };
        let eval = evaluate_stop_hook(&ctx, None, false, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(eval.decision, "allow");
        assert!(eval.fast_path);
    }

    #[tokio::test]
    async fn skips_immediately_when_not_fully_idle() {
        let ctx = StopHookContext {
            conversation_id: "conv-1".to_string(),
            cwd: "/tmp".to_string(),
            transcript_path: String::new(),
            termination_reason: "NO_TOOL_CALL".to_string(),
            continuation_active: false,
            fully_idle: false,
            execution_num: 0,
        };
        let eval = evaluate_stop_hook(&ctx, None, false, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(eval.decision, "allow");
        assert!(eval.fast_path);
    }

    #[test]
    fn splits_diff_by_file_and_filters_checked_chunks() {
        let sample = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1 +1 @@
-old_a
+new_a
diff --git a/src/b.rs b/src/b.rs
index 333..444 100644
--- a/src/b.rs
+++ b/src/b.rs
@@ -1 +1 @@
-old_b
+new_b";

        let chunks = split_git_diff_by_file(sample);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].0, "src/a.rs");
        assert_eq!(chunks[1].0, "src/b.rs");

        let mut checked = BTreeMap::new();
        checked.insert(chunks[0].0.clone(), chunks[0].2);
        let (unseen, new_list) = filter_unchecked_diff_chunks(sample, &checked);
        assert!(!unseen.contains("src/a.rs"));
        assert!(unseen.contains("src/b.rs"));
        assert_eq!(new_list.len(), 1);
        assert_eq!(new_list[0].0, "src/b.rs");

        checked.insert(chunks[1].0.clone(), chunks[1].2);
        let (unseen_all, new_all) = filter_unchecked_diff_chunks(sample, &checked);
        assert!(unseen_all.is_empty());
        assert!(new_all.is_empty());
    }

    #[test]
    fn detects_readonly_vs_mutating_turn_in_transcript() {
        let mut temp_path = std::env::temp_dir();
        temp_path.push(format!("stop_gate_test_{}.jsonl", std::process::id()));

        let readonly_lines = concat!(
            r#"{"step_index":1,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"check"}"#,
            "\n",
            r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","tool_calls":[{"name":"read_file","args":{}},{"name":"manage_task","args":{}}]}"#,
            "\n"
        );
        fs::write(&temp_path, readonly_lines).unwrap();
        assert!(!transcript_modified_workspace_in_latest_turn(&temp_path));

        let mutating_lines = format!(
            "{}{}\n",
            readonly_lines,
            r#"{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","tool_calls":[{"name":"edit_file","args":{}}]}"#
        );
        fs::write(&temp_path, mutating_lines).unwrap();
        assert!(transcript_modified_workspace_in_latest_turn(&temp_path));

        let _ = fs::remove_file(&temp_path);
    }
}
