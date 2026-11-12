use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    io::{self, IsTerminal, Read},
    path::PathBuf,
    time::Duration,
};

use agy_decision_lab::{
    gate::evaluate_command_for_hook,
    router::{ModelTargets, classify_prompt_with_timeout},
    stop_gate::{StopHookContext, evaluate_stop_hook},
};
use anyhow::Result;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

/// 순정 agy TUI용 PreToolUse 안전 검문소 및 Stop 체크리스트 통합 훅 핸들러
#[derive(Debug, Parser)]
#[command(name = "agy-decision-hook", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<HookCmd>,
}

#[derive(Debug, Subcommand)]
enum HookCmd {
    /// PreInvocation 라우팅 훅 활성화
    On,
    /// PreInvocation 라우팅 훅 일시 중지
    Off,
    /// PreToolUse 위험 명령 안전 검문소 활성화
    GateOn,
    /// PreToolUse 위험 명령 안전 검문소 일시 중지
    GateOff,
    /// Stop 완료 체크리스트 검증 훅 활성화
    StopOn,
    /// Stop 완료 체크리스트 검증 훅 일시 중지
    StopOff,
    /// 훅 및 안전 검문소 현재 활성화 상태 확인
    Status,
    /// 샘플 프롬프트로 PreInvocation 라우팅 훅 JSON 출력 시뮬레이션
    Test {
        prompt: String,
        #[arg(long, default_value = "Gemini 3.8 Flash (Low)")]
        current_model: String,
    },
    /// 샘플 셸 명령으로 PreToolUse 안전 검문소 판정 시뮬레이션
    TestGate {
        command: String,
        #[arg(long)]
        cwd: Option<String>,
    },
    /// 현재 작업 트리의 git diff로 Stop 체크리스트 검증 시뮬레이션
    TestStop {
        #[arg(long)]
        cwd: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenericHookEnvelope {
    #[serde(default)]
    hook_event_name: String,
    #[serde(default)]
    conversation_id: String,
    #[serde(default)]
    model_name: String,
    #[serde(default)]
    last_user_input: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    tool_call: Option<ToolCallPayload>,
    #[serde(default)]
    termination_reason: String,
    #[serde(default)]
    continuation_active: bool,
    #[serde(default)]
    execution_num: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallPayload {
    #[serde(default)]
    name: String,
    #[serde(default)]
    args: serde_json::Value,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DedupCache {
    last_key_hash: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();

    if let Some(sub) = cli.command {
        return handle_cli_subcommand(sub).await;
    }

    match handle_hook_stdin().await {
        Ok(json_out) => println!("{json_out}"),
        Err(_) => println!("{{}}"),
    }
    Ok(())
}

async fn handle_cli_subcommand(cmd: HookCmd) -> Result<()> {
    let route_flag = route_disabled_flag_path();
    let gate_flag = gate_disabled_flag_path();
    let stop_flag = stop_disabled_flag_path();

    match cmd {
        HookCmd::On => {
            if route_flag.exists() {
                fs::remove_file(&route_flag)?;
            }
            println!("PreInvocation 라우팅 훅: ENABLED");
        }
        HookCmd::Off => {
            write_flag(&route_flag)?;
            println!("PreInvocation 라우팅 훅: DISABLED");
        }
        HookCmd::GateOn => {
            if gate_flag.exists() {
                fs::remove_file(&gate_flag)?;
            }
            println!("PreToolUse 안전 검문소 훅: ENABLED");
        }
        HookCmd::GateOff => {
            write_flag(&gate_flag)?;
            println!("PreToolUse 안전 검문소 훅: DISABLED");
        }
        HookCmd::StopOn => {
            if stop_flag.exists() {
                fs::remove_file(&stop_flag)?;
            }
            println!("Stop 완료 체크리스트 검증 훅: ENABLED");
        }
        HookCmd::StopOff => {
            write_flag(&stop_flag)?;
            println!("Stop 완료 체크리스트 검증 훅: DISABLED");
        }
        HookCmd::Status => {
            println!(
                "PreToolUse    (명령 안전 검문소):       {}",
                if gate_flag.exists() {
                    "disabled (gate-off)"
                } else {
                    "enabled"
                }
            );
            println!(
                "Stop          (완료 체크리스트 검증):   {}",
                if stop_flag.exists() {
                    "disabled (stop-off)"
                } else {
                    "enabled (decision-stop 플러그인 활성 시 동작)"
                }
            );
            println!(
                "PreInvocation (레거시 자문 훅):         {} (플러그인 기본 구성에서는 비등록)",
                if route_flag.exists() {
                    "disabled"
                } else {
                    "standby"
                }
            );
            if let Ok(last) = fs::read_to_string(last_route_state_path()) {
                println!("최근 라우팅 기록: {last}");
            }
        }
        HookCmd::Test {
            prompt,
            current_model,
        } => {
            let env = GenericHookEnvelope {
                hook_event_name: "PreInvocation".to_string(),
                conversation_id: "test-session".to_string(),
                model_name: current_model,
                last_user_input: prompt,
                cwd: String::new(),
                tool_call: None,
                termination_reason: String::new(),
                continuation_active: false,
                execution_num: 0,
            };
            let out = evaluate_pre_invocation(&env, false).await?;
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        HookCmd::TestGate { command, cwd } => {
            let eval =
                evaluate_command_for_hook(&command, cwd.as_deref(), Duration::from_millis(4000))
                    .await?;
            let hook_stdout = hook_json_from_gate_action(&eval.action, &eval.reason);
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "evaluation": eval,
                    "hook_stdout": hook_stdout,
                }))?
            );
        }
        HookCmd::TestStop { cwd } => {
            let resolved_cwd = cwd.unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| ".".to_string())
            });
            let ctx = StopHookContext {
                conversation_id: String::new(),
                cwd: resolved_cwd,
                termination_reason: "NO_TOOL_CALL".to_string(),
                continuation_active: false,
                execution_num: 0,
            };
            let eval = evaluate_stop_hook(&ctx, None, false, Duration::from_millis(4800)).await?;
            let hook_stdout = if eval.decision == "continue" {
                serde_json::json!({
                    "decision": "continue",
                    "reason": eval.reason.clone().unwrap_or_default(),
                })
            } else {
                serde_json::json!({
                    "decision": "allow",
                })
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "evaluation": eval,
                    "hook_stdout": hook_stdout,
                }))?
            );
        }
    }
    Ok(())
}

async fn handle_hook_stdin() -> Result<String> {
    if io::stdin().is_terminal() {
        return Ok("{}".to_string());
    }

    let mut raw = String::new();
    io::stdin().read_to_string(&mut raw)?;
    if raw.trim().is_empty() {
        return Ok("{}".to_string());
    }

    let is_decision_hook = raw.contains("\"PreToolUse\"") || raw.contains("\"Stop\"");
    let envelope: GenericHookEnvelope = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(_) => {
            return Ok(if is_decision_hook {
                r#"{"decision":"allow","reason":"훅 입력 파싱 예외 — 기본 정책(allow)으로 통과"}"#
                    .to_string()
            } else {
                "{}".to_string()
            });
        }
    };

    let out = match envelope.hook_event_name.as_str() {
        "PreToolUse" => match evaluate_pre_tool_use(&envelope).await {
            Ok(val) => val,
            Err(err) => serde_json::json!({
                "decision": "allow",
                "reason": format!("Decision 게이트 타임아웃/예외 폴백 ({err}) — 기본 실행 정책(allow)으로 진행")
            }),
        },
        "Stop" => evaluate_stop_envelope(&envelope).await,
        "PreInvocation" => {
            if std::env::var("AGY_ROUTE_ACTIVE").as_deref() == Ok("1")
                || route_disabled_flag_path().exists()
            {
                serde_json::json!({})
            } else {
                evaluate_pre_invocation(&envelope, true)
                    .await
                    .unwrap_or_else(|_| serde_json::json!({}))
            }
        }
        _ => serde_json::json!({}),
    };

    Ok(serde_json::to_string(&out)?)
}

async fn evaluate_stop_envelope(envelope: &GenericHookEnvelope) -> serde_json::Value {
    if stop_disabled_flag_path().exists() {
        return serde_json::json!({ "decision": "allow" });
    }

    let ctx = StopHookContext {
        conversation_id: envelope.conversation_id.clone(),
        cwd: envelope.cwd.clone(),
        termination_reason: envelope.termination_reason.clone(),
        continuation_active: envelope.continuation_active,
        execution_num: envelope.execution_num,
    };

    match evaluate_stop_hook(&ctx, None, true, Duration::from_millis(4800)).await {
        Ok(eval) if eval.decision == "continue" => serde_json::json!({
            "decision": "continue",
            "reason": eval.reason.unwrap_or_else(|| "체크리스트 위반 항목을 점검해 주세요.".to_string()),
        }),
        _ => serde_json::json!({ "decision": "allow" }),
    }
}

fn stop_disabled_flag_path() -> PathBuf {
    state_dir().join("stop-disabled")
}

async fn evaluate_pre_tool_use(envelope: &GenericHookEnvelope) -> Result<serde_json::Value> {
    if gate_disabled_flag_path().exists() {
        // 주의: PreToolUse에서 빈 객체("{}")를 반환하면 agy가 decision=""를 거부(deny)로 처리하므로,
        // always-proceed 및 agy-route 흐름을 방해하지 않도록 "allow"를 명시적으로 반환합니다.
        return Ok(serde_json::json!({
            "decision": "allow"
        }));
    }

    let Some(tool) = &envelope.tool_call else {
        return Ok(serde_json::json!({ "decision": "allow" }));
    };

    if !tool.name.eq_ignore_ascii_case("run_command") {
        return Ok(serde_json::json!({ "decision": "allow" }));
    }

    let cmd_str = tool
        .args
        .get("CommandLine")
        .or_else(|| tool.args.get("commandLine"))
        .or_else(|| tool.args.get("command"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    if cmd_str.is_empty() {
        return Ok(serde_json::json!({ "decision": "allow" }));
    }

    let cwd_opt = (!envelope.cwd.trim().is_empty()).then_some(envelope.cwd.as_str());
    let eval = evaluate_command_for_hook(cmd_str, cwd_opt, Duration::from_millis(3800)).await?;
    Ok(hook_json_from_gate_action(&eval.action, &eval.reason))
}

fn hook_json_from_gate_action(action: &str, reason: &str) -> serde_json::Value {
    match action {
        "deny" => serde_json::json!({
            "decision": "deny",
            "reason": reason,
        }),
        "force_ask" => serde_json::json!({
            "decision": "force_ask",
            "reason": reason,
        }),
        "ask" => serde_json::json!({
            "decision": "ask",
            "reason": reason,
        }),
        _ => serde_json::json!({
            "decision": "allow",
            "reason": reason,
        }),
    }
}

async fn evaluate_pre_invocation(
    payload: &GenericHookEnvelope,
    use_dedup_cache: bool,
) -> Result<serde_json::Value> {
    let prompt = payload.last_user_input.trim();
    if prompt.is_empty() || prompt.starts_with('/') {
        return Ok(serde_json::json!({}));
    }

    let hash_val = compute_turn_hash(&payload.conversation_id, prompt);
    if use_dedup_cache && is_duplicate_turn(hash_val) {
        return Ok(serde_json::json!({}));
    }

    let threshold = std::env::var("AGY_CONFIDENCE_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.65);
    let targets = ModelTargets::from_env();

    let summary =
        classify_prompt_with_timeout(prompt, threshold, &targets, Duration::from_millis(3500))
            .await?;

    if use_dedup_cache {
        save_turn_hash(hash_val);
    }

    // 상태바(statusline)나 status 명령에서 참조할 수 있도록 최신 라우팅 요약 기록
    save_last_route_state(&serde_json::json!({
        "conversation_id": payload.conversation_id,
        "tier": summary.task_tier,
        "recommended_model": summary.selected_model,
        "recommended_effort": summary.recommended_effort,
        "score": summary.complexity_score,
        "confidence": summary.tier_confidence,
        "active_model": payload.model_name,
        "latency_ms": summary.latency_ms,
    }));
    save_statusline_route_cache(
        &payload.conversation_id,
        &summary.selected_model,
        &summary.recommended_effort,
        summary.complexity_score,
        summary.tier_confidence,
    );

    let current_tier = infer_active_model_tier(&payload.model_name);
    let recommended_tier = match summary.selected_family.as_str() {
        "flash" => 0,
        "argon" => 1,
        _ => 2,
    };

    if recommended_tier > current_tier && summary.selected_family == "carbon" {
        let sys = format!(
            "[Decision Router Advisory: tier={}, score={:.2}/4.0, recommended_model={}, active_model='{}'] This prompt requires Carbon-grade multi-module or architectural reasoning. Read primary source files completely, verify edge cases step-by-step, and briefly mention at the end if switching via `/model {}` would help for follow-up turns.",
            summary.task_tier,
            summary.complexity_score,
            summary.selected_model,
            payload.model_name,
            summary.selected_model
        );
        return Ok(serde_json::json!({
            "injectSteps": [
                {
                    "systemMessage": {
                        "systemMessage": sys
                    }
                }
            ]
        }));
    }

    // 체급이 이미 맞거나 단순 질문인 경우 불필요한 스텝 주입 없이 빈 객체("{}")만 반환해 패닉·토큰 낭비를 원천 차단합니다.
    Ok(serde_json::json!({}))
}

fn infer_active_model_tier(model_name: &str) -> u8 {
    let lower = model_name.to_ascii_lowercase();
    if lower.contains("flash") {
        0
    } else if lower.contains("carbon") || lower.contains("opus") {
        2
    } else {
        // Argon, Sonnet 등 밸런스 주력 모델
        1
    }
}

fn compute_turn_hash(conv_id: &str, prompt: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    conv_id.hash(&mut hasher);
    prompt.hash(&mut hasher);
    hasher.finish()
}

fn runtime_dir() -> PathBuf {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(runtime)
}

fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = fs::metadata("/proc/self") {
            return meta.uid();
        }
    }
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .and_then(|p| p.strip_prefix("/run/user/")?.parse::<u32>().ok())
        .unwrap_or(1000)
}

fn statusline_cache_dir() -> PathBuf {
    PathBuf::from(format!("/tmp/agy-statusline-{}", current_uid()))
}

fn sanitize_cache_id(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn split_model_for_statusline(
    selected_model: &str,
    fallback_effort: &str,
) -> (String, String, String, u32) {
    let normalized = selected_model.trim().to_ascii_lowercase();
    let (base, effort) = ["low", "medium", "high", "xhigh", "max"]
        .iter()
        .find_map(|eff| {
            let suffix = format!("-{eff}");
            normalized
                .strip_suffix(&suffix)
                .map(|b| (b.to_string(), (*eff).to_string()))
        })
        .unwrap_or_else(|| {
            let eff = fallback_effort.trim().to_ascii_lowercase();
            let eff = if eff.is_empty() {
                "medium".to_string()
            } else {
                eff
            };
            (normalized.clone(), eff)
        });

    let short_base = base
        .strip_prefix("gemini-3.8-")
        .or_else(|| base.strip_prefix("gemini-4-"))
        .or_else(|| base.strip_prefix("gemini-"))
        .or_else(|| base.strip_prefix("claude-"))
        .unwrap_or(&base);
    let short_label = format!("{short_base}-{effort}");

    let fam_rank: u32 = if base.contains("flash") || base.contains("haiku") {
        10
    } else if base.contains("carbon") || base.contains("opus") || base.contains("pro") {
        30
    } else {
        20
    };
    let eff_rank: u32 = match effort.as_str() {
        "low" => 1,
        "medium" => 2,
        "high" => 3,
        "xhigh" => 4,
        "max" => 5,
        _ => 2,
    };

    (base, effort, short_label, fam_rank + eff_rank)
}

fn save_statusline_route_cache(
    conversation_id: &str,
    selected_model: &str,
    recommended_effort: &str,
    complexity_score: f64,
    tier_confidence: f64,
) {
    let trimmed = conversation_id.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return;
    }
    let sid = sanitize_cache_id(trimmed);
    let cdir = statusline_cache_dir();
    if fs::create_dir_all(&cdir).is_err() {
        return;
    }

    let (base, effort, short_label, rank) =
        split_model_for_statusline(selected_model, recommended_effort);
    let epoch_sec = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let conf_pct = (tier_confidence.clamp(0.0, 1.0) * 100.0).round() as u32;
    let line = format!(
        "{base}\t{effort}\t{short_label}\t{rank}\t{complexity_score:.2}\t{conf_pct}\t{epoch_sec}\n"
    );

    let tmp_path = cdir.join(format!(".decision-route-{sid}.tsv.{}", std::process::id()));
    let final_path = cdir.join(format!("decision-route-{sid}.tsv"));
    if fs::write(&tmp_path, line).is_ok() {
        let _ = fs::rename(&tmp_path, &final_path);
    }
}

fn dedup_cache_path() -> PathBuf {
    runtime_dir().join("agy-decision-hook-cache.json")
}

fn last_route_state_path() -> PathBuf {
    runtime_dir().join("agy-decision-last-route.json")
}

fn save_last_route_state(val: &serde_json::Value) {
    if let Ok(raw) = serde_json::to_string(val) {
        let _ = fs::write(last_route_state_path(), raw);
    }
}

fn is_duplicate_turn(hash_val: u64) -> bool {
    let path = dedup_cache_path();
    let Ok(raw) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(cache) = serde_json::from_str::<DedupCache>(&raw) else {
        return false;
    };
    cache.last_key_hash == hash_val
}

fn save_turn_hash(hash_val: u64) {
    let path = dedup_cache_path();
    if let Ok(raw) = serde_json::to_string(&DedupCache {
        last_key_hash: hash_val,
    }) {
        let _ = fs::write(path, raw);
    }
}

fn write_flag(path: &PathBuf) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, b"disabled\n")?;
    Ok(())
}

fn state_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local/state/agy-decision-lab")
}

fn route_disabled_flag_path() -> PathBuf {
    state_dir().join("hook-disabled")
}

fn gate_disabled_flag_path() -> PathBuf {
    state_dir().join("gate-disabled")
}
