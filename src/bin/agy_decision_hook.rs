use std::{
    collections::hash_map::DefaultHasher,
    fs::{self, OpenOptions},
    hash::{Hash, Hasher},
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    time::Duration,
};

use agy_decision_lab::{
    gate::evaluate_command_for_hook,
    router::{ModelTargets, classify_prompt_with_timeout},
    stop_gate::{StopHookContext, evaluate_stop_hook},
    subagent_router::{SubagentRouteOutcome, evaluate_subagent_for_hook, extract_subagent_call},
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
    /// PreToolUse 서브에이전트·스웜 동적 모델 라우터 활성화
    SubagentOn,
    /// PreToolUse 서브에이전트·스웜 동적 모델 라우터 일시 중지
    SubagentOff,
    /// 최근 서브에이전트·스웜 라우팅 감사 로그 출력
    SubagentLog {
        #[arg(short = 'n', long, default_value_t = 15)]
        limit: usize,
    },
    /// 훅 및 안전 검문소 현재 활성화 상태 확인
    Status,
    /// 샘플 프롬프트로 PreInvocation 라우팅 훅 JSON 출력 시뮬레이션
    Test {
        prompt: String,
        #[arg(long, default_value = "Gemini 4 Hydrogen (Low)")]
        current_model: String,
    },
    /// 샘플 셸 명령으로 PreToolUse 안전 검문소 판정 시뮬레이션
    TestGate {
        command: String,
        #[arg(long)]
        cwd: Option<String>,
    },
    /// 샘플 브리프로 서브에이전트·스웜 동적 모델 라우팅 시뮬레이션
    TestSubagent {
        prompt: String,
        /// 대상 도구 종류: swarm | subagent
        #[arg(long, default_value = "swarm")]
        kind: String,
        /// invoke_subagent일 때 TypeName (research | self) 또는 swarm일 때 워커 이름
        #[arg(long, default_value = "worker")]
        type_name: String,
        /// workspace 모드 (inherit | share | branch)
        #[arg(long, default_value = "inherit")]
        workspace: String,
        /// 호출자가 명시한 모델 값 (비어 있으면 기본 생략/inherit)
        #[arg(long, default_value = "")]
        requested_model: String,
        /// 부모 세션 활성 모델 표시명
        #[arg(long, default_value = "Gemini 4 Carbon (Medium)")]
        parent_model: String,
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
    transcript_path: String,
    #[serde(default)]
    tool_call: Option<ToolCallPayload>,
    #[serde(default)]
    termination_reason: String,
    #[serde(default)]
    continuation_active: bool,
    #[serde(default = "default_fully_idle")]
    fully_idle: bool,
    #[serde(default)]
    execution_num: u64,
}

fn default_fully_idle() -> bool {
    true
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
    let _ = agy_decision_lab::runtime_config::load_runtime_env();
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
    let subagent_flag = subagent_disabled_flag_path();

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
        HookCmd::SubagentOn => {
            if subagent_flag.exists() {
                fs::remove_file(&subagent_flag)?;
            }
            println!("PreToolUse 서브에이전트·스웜 라우터 훅: ENABLED");
        }
        HookCmd::SubagentOff => {
            write_flag(&subagent_flag)?;
            println!("PreToolUse 서브에이전트·스웜 라우터 훅: DISABLED");
        }
        HookCmd::SubagentLog { limit } => {
            let log_path = subagent_audit_log_path();
            match fs::read_to_string(&log_path) {
                Ok(raw) if !raw.trim().is_empty() => {
                    let lines: Vec<&str> = raw.lines().collect();
                    let start = lines.len().saturating_sub(limit);
                    for line in &lines[start..] {
                        println!("{line}");
                    }
                }
                _ => {
                    println!(
                        "기록된 서브에이전트 라우팅 로그가 아직 없습니다 ({})",
                        log_path.display()
                    );
                }
            }
        }
        HookCmd::Status => {
            let subagent_install_mode = detect_plugin_install_mode("decision-subagent");
            let cfg_env = agy_decision_lab::runtime_config::user_config_env_path();
            let has_api_key = std::env::var("GEMINI_API_KEY")
                .ok()
                .filter(|v| !v.trim().is_empty() && v != "your_ai_studio_api_key_here")
                .is_some();
            println!(
                "Config        (XDG 설정·API 키):        {} (GEMINI_API_KEY: {})",
                if cfg_env.is_file() {
                    cfg_env.display().to_string()
                } else {
                    format!("{} (미생성 — 환경변수/CWD 폴백)", cfg_env.display())
                },
                if has_api_key { "configured" } else { "missing" }
            );
            println!(
                "PreToolUse    (명령 안전 검문소):       {} (플러그인: {})",
                if gate_flag.exists() {
                    "disabled (gate-off)"
                } else {
                    "enabled"
                },
                detect_plugin_install_mode("decision")
            );
            println!(
                "PreToolUse    (서브에이전트·스웜 라우터): {} (플러그인: {})",
                if subagent_flag.exists() {
                    "disabled (subagent-off)"
                } else {
                    "enabled"
                },
                subagent_install_mode
            );
            println!(
                "Stop          (완료 체크리스트 검증):   {} (플러그인: {})",
                if stop_flag.exists() {
                    "disabled (stop-off)"
                } else {
                    "enabled"
                },
                detect_plugin_install_mode("decision-stop")
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
            if let Ok(raw) = fs::read_to_string(subagent_audit_log_path()) {
                if let Some(last_sub) = raw.lines().last() {
                    println!("최근 서브에이전트 라우팅: {last_sub}");
                }
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
                transcript_path: String::new(),
                tool_call: None,
                termination_reason: String::new(),
                continuation_active: false,
                fully_idle: true,
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
        HookCmd::TestSubagent {
            prompt,
            kind,
            type_name,
            workspace,
            requested_model,
            parent_model,
        } => {
            let (tool_name, args) = if kind.eq_ignore_ascii_case("subagent")
                || kind.eq_ignore_ascii_case("invoke_subagent")
            {
                let eff_type = if type_name == "worker" {
                    "research".to_string()
                } else {
                    type_name
                };
                let mut obj = serde_json::json!({
                    "TypeName": eff_type,
                    "Role": "Simulated Subagent",
                    "Workspace": workspace,
                    "Prompt": prompt,
                });
                if !requested_model.trim().is_empty() {
                    obj["Model"] = serde_json::Value::String(requested_model);
                }
                ("invoke_subagent", obj)
            } else {
                let mut obj = serde_json::json!({
                    "action": "add",
                    "name": type_name,
                    "workspace": workspace,
                    "brief": prompt,
                });
                if !requested_model.trim().is_empty() {
                    obj["model"] = serde_json::Value::String(requested_model);
                }
                ("swarm", obj)
            };

            let spec = extract_subagent_call(tool_name, &args, &parent_model)
                .ok_or_else(|| anyhow::anyhow!("유효한 서브에이전트/스웜 호출 명세를 생성하지 못했습니다"))?;
            let outcome = evaluate_subagent_for_hook(&spec, Duration::from_millis(3800)).await?;
            let hook_stdout = hook_json_from_subagent_outcome(&outcome);
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "evaluation": outcome,
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
                transcript_path: String::new(),
                termination_reason: "NO_TOOL_CALL".to_string(),
                continuation_active: false,
                fully_idle: true,
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
        transcript_path: envelope.transcript_path.clone(),
        termination_reason: envelope.termination_reason.clone(),
        continuation_active: envelope.continuation_active,
        fully_idle: envelope.fully_idle,
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
    let Some(tool) = &envelope.tool_call else {
        return Ok(serde_json::json!({ "decision": "allow" }));
    };

    // 1. 서브에이전트(invoke_subagent) 및 네이티브 스웜(swarm add) 동적 모델·추론 강도 라우팅 분기
    if let Some(spec) = extract_subagent_call(&tool.name, &tool.args, &envelope.model_name) {
        if subagent_disabled_flag_path().exists()
            || std::env::var("AGY_SUBAGENT_ROUTER_OFF").as_deref() == Ok("1")
        {
            return Ok(serde_json::json!({ "decision": "allow" }));
        }

        return match evaluate_subagent_for_hook(&spec, Duration::from_millis(2800)).await {
            Ok(outcome) => {
                append_subagent_audit_log(&envelope.conversation_id, &envelope.model_name, &outcome);
                Ok(hook_json_from_subagent_outcome(&outcome))
            }
            Err(_) => {
                // 타임아웃·네트워크 오류 시 에이전트 실행을 절대 방해하지 않고 순정 인자 그대로 통과(Fail-Open)
                Ok(serde_json::json!({ "decision": "allow" }))
            }
        };
    }

    // 2. 셸 명령(run_command) 안전 검문소 분기
    if !tool.name.eq_ignore_ascii_case("run_command") {
        return Ok(serde_json::json!({ "decision": "allow" }));
    }

    if gate_disabled_flag_path().exists() {
        // 주의: PreToolUse에서 빈 객체("{}")를 반환하면 agy가 decision=""를 거부(deny)로 처리하므로,
        // always-proceed 흐름을 방해하지 않도록 "allow"를 명시적으로 반환합니다.
        return Ok(serde_json::json!({
            "decision": "allow"
        }));
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

fn hook_json_from_subagent_outcome(outcome: &SubagentRouteOutcome) -> serde_json::Value {
    if outcome.rewritten {
        let mut overwrite = serde_json::Map::new();
        overwrite.insert(
            outcome.model_arg_key.clone(),
            serde_json::Value::String(outcome.selected_model.clone()),
        );
        serde_json::json!({
            "decision": "allow",
            "reason": outcome.reason,
            "overwrite": overwrite,
        })
    } else {
        serde_json::json!({
            "decision": "allow",
        })
    }
}

fn append_subagent_audit_log(
    conversation_id: &str,
    parent_model: &str,
    outcome: &SubagentRouteOutcome,
) {
    let path = subagent_audit_log_path();
    if let Some(parent) = path.parent() {
        let _ = agy_decision_lab::runtime_config::ensure_private_dir(parent);
    }
    if let Ok(meta) = fs::symlink_metadata(&path)
        && meta.file_type().is_symlink()
    {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let record = serde_json::json!({
        "epoch_sec": ts,
        "conversation_id": conversation_id,
        "parent_model": parent_model,
        "kind": outcome.kind,
        "target": outcome.target_name,
        "workspace": outcome.workspace,
        "requested_model": outcome.requested_model,
        "selected_model": outcome.selected_model,
        "rewritten": outcome.rewritten,
        "guard": outcome.guard_applied,
        "tier": outcome.route_summary.as_ref().map(|s| s.task_tier.as_str()),
        "score": outcome.route_summary.as_ref().map(|s| s.complexity_score),
        "confidence": outcome.route_summary.as_ref().map(|s| s.tier_confidence),
        "latency_ms": outcome.route_summary.as_ref().map(|s| s.latency_ms),
    });
    if let Ok(line) = serde_json::to_string(&record) {
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        if let Ok(mut file) = opts.open(&path) {
            let _ = writeln!(file, "{line}");
        }
    }
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
        "hydrogen" | "flash" => 0,
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
    if lower.contains("flash")
        || lower.contains("hydrogen-low")
        || lower.contains("hydrogen (low)")
    {
        0
    } else if lower.contains("carbon") || lower.contains("opus") {
        2
    } else {
        // Hydrogen Medium/High/XHigh, Argon, Sonnet 등 코딩 주력 모델
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
    } else if base.contains("hydrogen") {
        15
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
    if agy_decision_lab::runtime_config::ensure_private_dir(&cdir).is_err() {
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

    let final_path = cdir.join(format!("decision-route-{sid}.tsv"));
    let _ = agy_decision_lab::runtime_config::write_private_file(&final_path, line.as_bytes());
}

fn dedup_cache_path() -> PathBuf {
    runtime_dir().join("agy-decision-hook-cache.json")
}

fn last_route_state_path() -> PathBuf {
    runtime_dir().join("agy-decision-last-route.json")
}

fn save_last_route_state(val: &serde_json::Value) {
    if let Ok(raw) = serde_json::to_string(val) {
        let _ = agy_decision_lab::runtime_config::write_private_file(
            &last_route_state_path(),
            raw.as_bytes(),
        );
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
        let _ = agy_decision_lab::runtime_config::write_private_file(&path, raw.as_bytes());
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

fn subagent_disabled_flag_path() -> PathBuf {
    state_dir().join("subagent-disabled")
}

fn subagent_audit_log_path() -> PathBuf {
    state_dir().join("subagent-routes.jsonl")
}

fn detect_plugin_install_mode(plugin_name: &str) -> &'static str {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let home_path = PathBuf::from(home);

    let candidates = [
        home_path
            .join(".gemini/antigravity-cli/plugins")
            .join(plugin_name),
        home_path.join(".gemini/antigravity/plugins").join(plugin_name),
        home_path.join(".gemini/config/plugins").join(plugin_name),
    ];

    for p in &candidates {
        if let Ok(meta) = fs::symlink_metadata(p) {
            if meta.file_type().is_symlink() {
                return "legacy-symlink";
            }
            if meta.is_dir() {
                return "managed-package";
            }
        }
    }
    "not-installed"
}
