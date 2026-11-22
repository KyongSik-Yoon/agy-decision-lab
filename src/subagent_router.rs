use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::router::{ModelTargets, RouteDecisionSummary, classify_prompt_with_timeout, env_or};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentToolKind {
    InvokeSubagent,
    SwarmAdd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentCallSpec {
    pub kind: SubagentToolKind,
    /// invoke_subagent: TypeName (예: research, self), swarm: 워커 name
    pub target_name: String,
    /// invoke_subagent: Role, swarm: action ("add")
    pub role_or_action: String,
    /// workspace 모드 ("inherit" | "share" | "branch")
    pub workspace: String,
    /// swarm fork 여부
    pub has_fork: bool,
    /// 호출자가 원래 넣은 모델 값 (비어 있으면 "")
    pub requested_model: String,
    /// overwrite에 사용할 정확한 인자 키 이름 ("Model" 또는 "model")
    pub model_arg_key: String,
    /// 부모 세션의 현재 활성 모델 표시명 (예: "Gemini 4 Carbon (Medium)")
    pub parent_model: String,
    /// Decision API에 전달할 통합 평가 텍스트
    pub evaluation_prompt: String,
    /// swarm_monitor 전용 경량 헬퍼 여부
    pub is_swarm_monitor: bool,
    /// 라우팅 제외 특수 에이전트 여부 (예: image-generator)
    pub bypass_routing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentRouteOutcome {
    pub kind: SubagentToolKind,
    pub target_name: String,
    pub workspace: String,
    pub requested_model: String,
    pub selected_model: String,
    pub model_arg_key: String,
    pub rewritten: bool,
    pub guard_applied: Option<String>,
    pub reason: String,
    pub route_summary: Option<RouteDecisionSummary>,
}

/// PreToolUse의 toolCall 이름과 인자에서 서브에이전트/스웜 스폰 호출인지 판별하고 명세를 추출합니다.
pub fn extract_subagent_call(
    tool_name: &str,
    args: &Value,
    parent_model: &str,
) -> Option<SubagentCallSpec> {
    if tool_name.eq_ignore_ascii_case("invoke_subagent") {
        let type_name = get_str_ci(args, &["TypeName", "typeName", "type_name"])
            .unwrap_or("self")
            .trim()
            .to_string();
        let role = get_str_ci(args, &["Role", "role"])
            .unwrap_or("")
            .trim()
            .to_string();
        let prompt = get_str_ci(args, &["Prompt", "prompt"])
            .unwrap_or("")
            .trim();
        if prompt.is_empty() {
            return None;
        }
        let workspace = get_str_ci(args, &["Workspace", "workspace"])
            .unwrap_or("inherit")
            .trim()
            .to_string();
        let (requested_model, model_arg_key) = if let Some(v) = args.get("Model").and_then(Value::as_str) {
            (v.trim().to_string(), "Model".to_string())
        } else if let Some(v) = args.get("model").and_then(Value::as_str) {
            (v.trim().to_string(), "model".to_string())
        } else {
            (String::new(), "Model".to_string())
        };

        let lower_type = type_name.to_ascii_lowercase();
        let bypass_routing = matches!(
            lower_type.as_str(),
            "image-generator" | "image_generator" | "browser"
        );

        let evaluation_prompt = format!(
            "[Subagent Archetype: {type_name} | Role: {role} | Workspace: {workspace}]\n{prompt}"
        );

        return Some(SubagentCallSpec {
            kind: SubagentToolKind::InvokeSubagent,
            target_name: type_name,
            role_or_action: role,
            workspace,
            has_fork: false,
            requested_model,
            model_arg_key,
            parent_model: parent_model.trim().to_string(),
            evaluation_prompt,
            is_swarm_monitor: false,
            bypass_routing,
        });
    }

    // swarm 도구는 바이너리 내부에서 "swarm" 또는 generic 스텝("generic")으로 전달될 수 있으므로 시그니처로 교차 검증
    if tool_name.eq_ignore_ascii_case("swarm") || tool_name.eq_ignore_ascii_case("generic") {
        let action = get_str_ci(args, &["action"])?;
        if !action.eq_ignore_ascii_case("add") {
            return None;
        }
        let brief = get_str_ci(args, &["brief"])?.trim();
        if brief.is_empty() {
            return None;
        }
        let worker_name = get_str_ci(args, &["name"])
            .unwrap_or("worker")
            .trim()
            .to_string();
        let workspace = get_str_ci(args, &["workspace"])
            .unwrap_or("inherit")
            .trim()
            .to_string();
        let has_fork = get_str_ci(args, &["fork"])
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let requested_model = get_str_ci(args, &["model"])
            .unwrap_or("")
            .trim()
            .to_string();

        let tasks_snippet = args
            .get("tasks")
            .and_then(Value::as_array)
            .map(|arr| {
                let joined: Vec<&str> = arr.iter().filter_map(Value::as_str).take(6).collect();
                if joined.is_empty() {
                    String::new()
                } else {
                    format!("\n[Sharded Tasks ({} items)]: {}", arr.len(), joined.join(" | "))
                }
            })
            .unwrap_or_default();

        let lower_brief = brief.to_ascii_lowercase();
        let is_swarm_monitor = worker_name.eq_ignore_ascii_case("monitor")
            || lower_brief.contains("swarm_monitor");

        let evaluation_prompt = format!(
            "[Swarm Worker: name={worker_name} | workspace={workspace} | fork={has_fork}]\n{brief}{tasks_snippet}"
        );

        return Some(SubagentCallSpec {
            kind: SubagentToolKind::SwarmAdd,
            target_name: worker_name,
            role_or_action: "add".to_string(),
            workspace,
            has_fork,
            requested_model,
            model_arg_key: "model".to_string(),
            parent_model: parent_model.trim().to_string(),
            evaluation_prompt,
            is_swarm_monitor,
            bypass_routing: false,
        });
    }

    None
}

pub async fn evaluate_subagent_for_hook(
    spec: &SubagentCallSpec,
    timeout: Duration,
) -> Result<SubagentRouteOutcome> {
    let targets = ModelTargets::from_env();

    if spec.bypass_routing {
        return Ok(passthrough_outcome(
            spec,
            "특수 목적 내장 서브에이전트 — 라우팅 생략",
            Some("special_subagent_bypass"),
            None,
        ));
    }

    // 사용자가 명시적 모델 지정을 무조건 우선하도록 환경변수를 켠 경우
    if env_flag("AGY_SUBAGENT_RESPECT_EXPLICIT", false)
        && !spec.requested_model.is_empty()
        && !spec.requested_model.eq_ignore_ascii_case("inherit")
    {
        return Ok(passthrough_outcome(
            spec,
            "명시적 지정 모델 존중(AGY_SUBAGENT_RESPECT_EXPLICIT=1)",
            Some("respect_explicit"),
            None,
        ));
    }

    // 스웜 모니터 워커는 주기적 상태 점검만 수행하므로 API 호출 없이 경량 모델 즉시 할당
    if spec.kind == SubagentToolKind::SwarmAdd && spec.is_swarm_monitor {
        if !spec.requested_model.is_empty() {
            return Ok(passthrough_outcome(
                spec,
                "스웜 모니터 워커 — 지정된 모델 유지",
                Some("swarm_monitor_keep"),
                None,
            ));
        }
        let monitor_model = targets.variant("hydrogen", "low");
        return Ok(SubagentRouteOutcome {
            kind: spec.kind,
            target_name: spec.target_name.clone(),
            workspace: spec.workspace.clone(),
            requested_model: spec.requested_model.clone(),
            selected_model: monitor_model.clone(),
            model_arg_key: spec.model_arg_key.clone(),
            rewritten: true,
            guard_applied: Some("swarm_monitor_fastpath".to_string()),
            reason: format!("swarm_monitor 경량 폴링 워커 -> {monitor_model}"),
            route_summary: None,
        });
    }

    let threshold = std::env::var("AGY_SUBAGENT_CONFIDENCE_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or_else(|| {
            std::env::var("AGY_CONFIDENCE_THRESHOLD")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.68)
        });

    let summary = classify_prompt_with_timeout(
        &spec.evaluation_prompt,
        threshold,
        &targets,
        timeout,
    )
    .await?;

    Ok(resolve_subagent_policy(spec, summary, &targets))
}

/// Decision API 응답 요약과 안전 가드레일(작성자 하한선, 교차 검증 보존, 과도한 강등 방지)을 결합해 최종 모델을 결정합니다.
pub fn resolve_subagent_policy(
    spec: &SubagentCallSpec,
    summary: RouteDecisionSummary,
    targets: &ModelTargets,
) -> SubagentRouteOutcome {
    match spec.kind {
        SubagentToolKind::SwarmAdd => resolve_swarm_add_policy(spec, summary, targets),
        SubagentToolKind::InvokeSubagent => {
            resolve_invoke_subagent_policy(spec, summary, &TierModels::from_env())
        }
    }
}

fn resolve_swarm_add_policy(
    spec: &SubagentCallSpec,
    summary: RouteDecisionSummary,
    targets: &ModelTargets,
) -> SubagentRouteOutcome {
    let preserve_cross_family = env_flag("AGY_SUBAGENT_PRESERVE_CROSS_FAMILY", true);
    let is_author_worker = is_mutating_workspace(&spec.workspace) || spec.has_fork;
    let recommended_level = extract_effort_suffix(&summary.selected_model).unwrap_or("medium");

    // 1. 타 벤더(Claude / GPT 등) 교차 검증 모델이 명시된 경우 패밀리를 깨뜨리지 않고 보존하되,
    //    추론 강도 접미사가 있는 모델(claude-*)은 필요 시 상향만 허용(강등 금지, xhigh 미지원 모델은 high로 클램프)
    if preserve_cross_family
        && !spec.requested_model.is_empty()
        && is_external_vendor_model(&spec.requested_model, targets)
    {
        if let Some((ext_base, cur_eff)) = split_effort_model(&spec.requested_model) {
            let ext_target_level = if recommended_level == "xhigh" && !ModelTargets::supports_xhigh_slug(ext_base) {
                "high"
            } else {
                recommended_level
            };
            if effort_rank(ext_target_level) > effort_rank(cur_eff) && summary.complexity_score >= 2.0 {
                let upgraded = format!("{ext_base}-{ext_target_level}");
                return SubagentRouteOutcome {
                    kind: spec.kind,
                    target_name: spec.target_name.clone(),
                    workspace: spec.workspace.clone(),
                    requested_model: spec.requested_model.clone(),
                    selected_model: upgraded.clone(),
                    model_arg_key: spec.model_arg_key.clone(),
                    rewritten: true,
                    guard_applied: Some("preserve_cross_family_upgrade_effort".to_string()),
                    reason: format!(
                        "타 패밀리 교차 검증 보존 + 고복잡도({:.2}) 추론 강도 상향: {} -> {}",
                        summary.complexity_score, spec.requested_model, upgraded
                    ),
                    route_summary: Some(summary),
                };
            }
        }
        return passthrough_outcome(
            spec,
            "타 패밀리 교차 검증 모델 명시 — 패밀리 및 강도 보존",
            Some("preserve_cross_family"),
            Some(summary),
        );
    }

    let mut final_family = summary.selected_family.clone();
    let mut final_level = recommended_level.to_string();
    let mut guard_applied: Option<String> = None;

    // 2. 작성자 워커 하한선(Author Floor): share / branch / fork 워커는 코드를 직접 수정하므로
    //    3.8-flash 수동 설정 시 Neon 이상, 기본 Hydrogen 4는 최소 medium 이상만 허용
    if is_author_worker && matches!(final_family.as_str(), "hydrogen" | "flash") {
        if targets.hydrogen_base.to_ascii_lowercase().contains("flash") {
            final_family = "neon".to_string();
            final_level = if summary.complexity_score < 1.35 {
                "low".to_string()
            } else {
                "medium".to_string()
            };
            guard_applied = Some("author_floor_no_flash".to_string());
        } else if final_level == "low" {
            final_level = "medium".to_string();
            guard_applied = Some("author_floor_hydrogen_medium".to_string());
        }
    }

    // 3. 명시적 Carbon 지정 시 보수적 강등 방지(High-Tier Guard):
    //    호출자가 이미 carbon 모델을 찍었을 때는 확신도가 충분히 높고(>=0.82) 작성자 워커가 아닐 때만 절감 허용
    let req_lower = spec.requested_model.to_ascii_lowercase();
    if req_lower.contains("carbon") && final_family != "carbon" {
        if is_author_worker || summary.tier_confidence < 0.82 || summary.complexity_score >= 1.65 {
            final_family = "carbon".to_string();
            let req_eff = extract_effort_suffix(&req_lower).unwrap_or("medium");
            if effort_rank(&final_level) < effort_rank(req_eff) && summary.complexity_confidence < 0.78 {
                final_level = req_eff.to_string();
            }
            guard_applied = Some("explicit_carbon_downgrade_guard".to_string());
        }
    }

    let selected_model = targets.variant(&final_family, &final_level);
    let rewritten = !spec.requested_model.eq_ignore_ascii_case(&selected_model);

    let reason = format!(
        "Decision swarm 라우팅(tier={}, score={:.2}, conf={:.0}%{}) -> {}",
        summary.task_tier,
        summary.complexity_score,
        summary.tier_confidence * 100.0,
        guard_applied
            .as_deref()
            .map(|g| format!(", guard={g}"))
            .unwrap_or_default(),
        selected_model
    );

    SubagentRouteOutcome {
        kind: spec.kind,
        target_name: spec.target_name.clone(),
        workspace: spec.workspace.clone(),
        requested_model: spec.requested_model.clone(),
        selected_model,
        model_arg_key: spec.model_arg_key.clone(),
        rewritten,
        guard_applied,
        reason,
        route_summary: Some(summary),
    }
}

fn resolve_invoke_subagent_policy(
    spec: &SubagentCallSpec,
    summary: RouteDecisionSummary,
    tiers: &TierModels,
) -> SubagentRouteOutcome {
    let is_research = spec.target_name.eq_ignore_ascii_case("research");
    let effective_req = if spec.requested_model.is_empty() {
        "inherit"
    } else {
        spec.requested_model.as_str()
    };

    // invoke_subagent 허용 티어: "inherit" | "flash_lite" | "flash" | "pro"
    let aligned = tiers.family_aligned();
    let (mut target_tier, mut guard_applied) = if aligned {
        aligned_invoke_tier(spec, &summary, tiers)
    } else {
        server_invoke_tier(spec, &summary, tiers)
    };

    // 호출자가 명시적으로 "pro"를 지정한 경우, 확실한 단순 research 조회가 아니면 강등 금지
    if effective_req.eq_ignore_ascii_case("pro")
        && matches!(target_tier, "flash" | "flash_lite")
        && !(is_research && summary.tier_confidence >= 0.85 && summary.complexity_score < 1.10)
    {
        target_tier = "pro";
        guard_applied = Some("explicit_pro_downgrade_guard".to_string());
    }

    let rewritten = !effective_req.eq_ignore_ascii_case(target_tier);
    let reason = format!(
        "Decision subagent 라우팅(type={}, tier={}, score={:.2}, conf={:.0}%{}{}) -> {}",
        spec.target_name,
        summary.task_tier,
        summary.complexity_score,
        summary.tier_confidence * 100.0,
        if aligned { ", tiers=aligned" } else { "" },
        guard_applied
            .as_deref()
            .map(|g| format!(", guard={g}"))
            .unwrap_or_default(),
        target_tier
    );

    SubagentRouteOutcome {
        kind: spec.kind,
        target_name: spec.target_name.clone(),
        workspace: spec.workspace.clone(),
        requested_model: effective_req.to_string(),
        selected_model: target_tier.to_string(),
        model_arg_key: spec.model_arg_key.clone(),
        rewritten,
        guard_applied,
        reason,
        route_summary: Some(summary),
    }
}

/// 서버 기본 티어(pro = Hydrogen xhigh 등)일 때의 정책.
/// 고난도 작업을 "pro"로 올릴 때, 부모가 이미 `pro` 티어 모델보다 상위면(예: Neon High 부모 vs Hydrogen xhigh)
/// 올리는 게 아니라 내리는 셈이므로 inherit 를 유지한다(`preserve_parent_above_pro`).
fn server_invoke_tier(
    spec: &SubagentCallSpec,
    summary: &RouteDecisionSummary,
    tiers: &TierModels,
) -> (&'static str, Option<String>) {
    let is_research = spec.target_name.eq_ignore_ascii_case("research");
    let is_mutating_ws = is_mutating_workspace(&spec.workspace);
    let parent_tier = infer_parent_tier(&spec.parent_model);
    let parent_above_pro = model_rank(&spec.parent_model) > model_rank(&tiers.pro);
    let mut guard_applied: Option<String> = None;
    let upgrade_to_pro = |guard: &mut Option<String>| {
        if parent_above_pro {
            *guard = Some("preserve_parent_above_pro".to_string());
            "inherit"
        } else {
            "pro"
        }
    };

    let target_tier = if is_research {
        if matches!(summary.selected_family.as_str(), "hydrogen" | "flash")
            && summary.complexity_score < 0.45
            && summary.tier_confidence >= 0.82
        {
            "flash_lite"
        } else if (matches!(summary.selected_family.as_str(), "hydrogen" | "flash")
            && summary.complexity_score < 2.15)
            || (matches!(summary.selected_family.as_str(), "neon" | "argon")
                && summary.complexity_score < 1.90)
        {
            "flash"
        } else if parent_tier >= 2 {
            // 부모가 이미 Carbon/Opus급이면 고난도 리서치 시 추론 강도까지 물려받는 inherit 우선
            "inherit"
        } else {
            upgrade_to_pro(&mut guard_applied)
        }
    } else {
        // self 또는 커스텀 에이전트 (코드 작성·명령 실행 가능): flash_lite 절대 금지
        match summary.selected_family.as_str() {
            "hydrogen" | "flash" => {
                if !is_mutating_ws
                    && summary.complexity_score < 1.05
                    && summary.tier_confidence >= 0.82
                {
                    "flash"
                } else if parent_tier == 1 {
                    "inherit"
                } else if parent_tier == 0 {
                    guard_applied = Some("exec_subagent_floor".to_string());
                    "pro"
                } else if !is_mutating_ws
                    && summary.tier_confidence >= 0.82
                    && summary.complexity_score < 1.55
                {
                    "pro"
                } else {
                    guard_applied = Some("exec_subagent_floor".to_string());
                    "inherit"
                }
            }
            "neon" | "argon" => {
                if parent_tier == 0 {
                    // 부모가 Flash일 때 기능 구현 서브에이전트는 pro로 승격하여 성능 저하 방지
                    "pro"
                } else if parent_tier == 1 {
                    "inherit"
                } else {
                    // 부모가 Carbon일 때: 확실히 단순한 밸런스 작업일 때만 pro로 완만히 절감, 애매하면 inherit 유지
                    if !is_mutating_ws
                        && summary.tier_confidence >= 0.82
                        && summary.complexity_score < 1.55
                    {
                        "pro"
                    } else {
                        guard_applied = Some("preserve_parent_carbon_effort".to_string());
                        "inherit"
                    }
                }
            }
            _ => {
                // carbon_complex / carbon_frontier
                if parent_tier >= 2 {
                    "inherit"
                } else {
                    upgrade_to_pro(&mut guard_applied)
                }
            }
        }
    };
    (target_tier, guard_applied)
}

/// 티어가 패밀리 순서로 고정된 경우(agy-cli-patch: flash_lite=Hydrogen, flash=Neon·Argon, pro=Carbon)의 정책.
/// 결정 모델이 고른 패밀리를 같은 패밀리 티어로 보내고, 기존 하한·보존 규칙은 유지한다.
fn aligned_invoke_tier(
    spec: &SubagentCallSpec,
    summary: &RouteDecisionSummary,
    tiers: &TierModels,
) -> (&'static str, Option<String>) {
    let is_research = spec.target_name.eq_ignore_ascii_case("research");
    let is_mutating_ws = is_mutating_workspace(&spec.workspace);
    let mut guard_applied: Option<String> = None;

    let mut want = match summary.selected_family.as_str() {
        "carbon" => "pro",
        "neon" | "argon" => "flash",
        _ => "flash_lite",
    };
    // 원하는 추론 강도가 티어 모델보다 두 단계 이상 높으면 한 티어 위로 (예: Hydrogen xhigh 작업 -> Neon medium)
    let (_, desired_effort) = model_rank(&summary.selected_model);
    let (_, tier_effort) = model_rank(tiers.get(want));
    if desired_effort >= tier_effort + 2 {
        want = match want {
            "flash_lite" => "flash",
            _ => "pro",
        };
    }
    // 코드를 쓰는 서브에이전트(self·커스텀)는 flash_lite 금지 — 기존 하한 유지
    if !is_research && want == "flash_lite" {
        want = "flash";
        guard_applied = Some("exec_subagent_floor".to_string());
    }

    let (parent_family, parent_effort) = model_rank(&spec.parent_model);
    let (want_family, want_effort) = model_rank(tiers.get(want));
    if parent_family == want_family
        && (parent_effort == want_effort || (desired_effort > want_effort && parent_effort >= desired_effort))
    {
        // 부모가 곧 그 티어 모델이거나, 결정 모델이 원한 강도를 부모만 채울 수 있으면 부모를 잇는다.
        // 원한 강도가 티어 모델 이하면(더 싸게 충분) 같은 패밀리라도 티어로 내린다.
        return ("inherit", guard_applied);
    }
    if want == "flash"
        && parent_family == 3
        && (is_mutating_ws || summary.tier_confidence < 0.82 || summary.complexity_score >= 1.55)
    {
        // Carbon 부모의 애매하거나 코드를 바꾸는 밸런스 작업은 부모 강도 유지
        return ("inherit", Some("preserve_parent_carbon_effort".to_string()));
    }
    (want, guard_applied)
}

fn passthrough_outcome(
    spec: &SubagentCallSpec,
    msg: &str,
    guard: Option<&str>,
    summary: Option<RouteDecisionSummary>,
) -> SubagentRouteOutcome {
    let cur = if spec.requested_model.is_empty() {
        "inherit".to_string()
    } else {
        spec.requested_model.clone()
    };
    SubagentRouteOutcome {
        kind: spec.kind,
        target_name: spec.target_name.clone(),
        workspace: spec.workspace.clone(),
        requested_model: spec.requested_model.clone(),
        selected_model: cur,
        model_arg_key: spec.model_arg_key.clone(),
        rewritten: false,
        guard_applied: guard.map(ToString::to_string),
        reason: msg.to_string(),
        route_summary: summary,
    }
}

fn get_str_ci<'a>(args: &'a Value, keys: &[&str]) -> Option<&'a str> {
    for k in keys {
        if let Some(v) = args.get(*k).and_then(Value::as_str) {
            return Some(v);
        }
    }
    None
}

fn env_flag(key: &str, default: bool) -> bool {
    let raw = env_or(key, if default { "1" } else { "0" });
    matches!(raw.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

fn is_mutating_workspace(ws: &str) -> bool {
    matches!(ws.trim().to_ascii_lowercase().as_str(), "share" | "branch")
}

/// invoke_subagent 의 티어(flash_lite / flash / pro)가 실제로 실행하는 모델.
/// 서버가 계정별로 정하며(기본값 `server_default`), agy-cli-patch 의 티어 고정 패치
/// (pro-tier·flash-tier·flash-lite-tier)를 적용했다면 `AGY_TIER_MODELS` 로 알려 준다.
/// 예: `AGY_TIER_MODELS=pro=gemini-4-carbon-high,flash=gemini-4-neon-medium,flash_lite=gemini-4-hydrogen-low`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierModels {
    pub flash_lite: String,
    pub flash: String,
    pub pro: String,
}

impl TierModels {
    pub fn server_default() -> Self {
        Self {
            flash_lite: "gemini-3.5-flash-lite".to_string(),
            flash: "gemini-3.8-flash".to_string(),
            pro: "gemini-4-hydrogen-tiered-xhigh".to_string(),
        }
    }

    /// `tier=model` 을 쉼표로 이은 값. 빠진 티어는 서버 기본값을 쓴다.
    pub fn parse(raw: &str) -> Self {
        let mut out = Self::server_default();
        for entry in raw.split(',') {
            let Some((tier, model)) = entry.split_once('=') else {
                continue;
            };
            let model = model.trim();
            if model.is_empty() {
                continue;
            }
            match tier.trim().to_ascii_lowercase().replace('-', "_").as_str() {
                "flash_lite" => out.flash_lite = model.to_string(),
                "flash" => out.flash = model.to_string(),
                "pro" => out.pro = model.to_string(),
                _ => {}
            }
        }
        out
    }

    pub fn from_env() -> Self {
        std::env::var("AGY_TIER_MODELS")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| Self::parse(&v))
            .unwrap_or_else(Self::server_default)
    }

    fn get(&self, tier: &str) -> &str {
        match tier {
            "flash_lite" => &self.flash_lite,
            "flash" => &self.flash,
            _ => &self.pro,
        }
    }

    /// 세 티어가 Hydrogen < Neon·Argon < Carbon 패밀리 순서로 고정돼 있는지
    pub fn family_aligned(&self) -> bool {
        model_rank(&self.flash_lite).0 == 1 && model_rank(&self.flash).0 == 2 && model_rank(&self.pro).0 == 3
    }
}

/// 모델 이름(슬러그·표시 이름)의 (패밀리 서열, 추론 강도 서열).
/// 패밀리: Flash 0 < Hydrogen 1 < Neon·Argon·Sonnet 2 < Carbon·Opus 3 (알 수 없으면 Hydrogen 취급),
/// 강도: low 1 < medium 2 < high 3 < xhigh 4 (표기 없으면 medium 취급).
fn model_rank(name: &str) -> (u8, u8) {
    let l = name.trim().to_ascii_lowercase();
    let family = if l.contains("carbon") || l.contains("opus") {
        3
    } else if l.contains("neon") || l.contains("argon") || l.contains("sonnet") {
        2
    } else if l.contains("flash") {
        0
    } else {
        1
    };
    let effort = if l.contains("xhigh") {
        4
    } else if l.contains("high") {
        3
    } else if l.contains("medium") {
        2
    } else if l.contains("low") {
        1
    } else {
        2
    };
    (family, effort)
}

fn infer_parent_tier(model_name: &str) -> u8 {
    let lower = model_name.trim().to_ascii_lowercase();
    if lower.is_empty() {
        // 알 수 없을 때는 보수적으로 밸런스(1) 취급
        1
    } else if lower.contains("flash")
        || lower.contains("hydrogen-low")
        || lower.contains("hydrogen (low)")
    {
        0
    } else if lower.contains("carbon") || lower.contains("opus") {
        2
    } else {
        1
    }
}

fn is_external_vendor_model(model: &str, targets: &ModelTargets) -> bool {
    let lower = model.trim().to_ascii_lowercase();
    let known_bases = [
        targets.hydrogen_base.to_ascii_lowercase(),
        targets.neon_base.to_ascii_lowercase(),
        targets.carbon_base.to_ascii_lowercase(),
    ];
    if known_bases.iter().any(|b| lower.starts_with(b)) {
        return false;
    }
    !lower.starts_with("gemini-")
}

fn split_effort_model(model: &str) -> Option<(&str, &str)> {
    for eff in ["xhigh", "high", "medium", "low"] {
        let suffix = format!("-{eff}");
        if model.len() > suffix.len() && model.to_ascii_lowercase().ends_with(&suffix) {
            let split_at = model.len() - suffix.len();
            return Some((&model[..split_at], eff));
        }
    }
    None
}

fn extract_effort_suffix(model: &str) -> Option<&'static str> {
    let lower = model.to_ascii_lowercase();
    if lower.ends_with("-xhigh") {
        Some("xhigh")
    } else if lower.ends_with("-high") {
        Some("high")
    } else if lower.ends_with("-medium") {
        Some("medium")
    } else if lower.ends_with("-low") {
        Some("low")
    } else {
        None
    }
}

fn effort_rank(eff: &str) -> u8 {
    match eff {
        "low" => 1,
        "medium" => 2,
        "high" => 3,
        "xhigh" => 4,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn default_targets() -> ModelTargets {
        ModelTargets {
            hydrogen_base: "gemini-4-hydrogen".to_string(),
            neon_base: "gemini-4-neon".to_string(),
            carbon_base: "gemini-4-carbon".to_string(),
        }
    }

    fn mock_summary(
        selected_model: &str,
        family: &str,
        tier: &str,
        score: f64,
        conf: f64,
    ) -> RouteDecisionSummary {
        RouteDecisionSummary {
            selected_model: selected_model.to_string(),
            selected_family: family.to_string(),
            cli_effort_flag: None,
            recommended_effort: extract_effort_suffix(selected_model)
                .unwrap_or("medium")
                .to_string(),
            task_tier: tier.to_string(),
            tier_confidence: conf,
            tier_probabilities: BTreeMap::new(),
            complexity_score: score,
            complexity_confidence: conf,
            escalated_by_confidence_gate: false,
            latency_ms: 210,
            input_tokens: 80,
            output_tokens: 0,
        }
    }

    #[test]
    fn swarm_non_add_actions_are_ignored() {
        let args = serde_json::json!({ "action": "status" });
        assert!(extract_subagent_call("swarm", &args, "Gemini 4 Carbon (Medium)").is_none());
        assert!(extract_subagent_call("generic", &args, "Gemini 4 Carbon (Medium)").is_none());
    }

    #[test]
    fn swarm_author_worker_lifts_hydrogen_low_to_medium() {
        let args = serde_json::json!({
            "action": "add",
            "name": "author",
            "workspace": "share",
            "brief": "Update typo and add small helper in config.rs"
        });
        let spec = extract_subagent_call("swarm", &args, "Gemini 4 Carbon (Medium)").unwrap();
        let summary = mock_summary("gemini-4-hydrogen-low", "hydrogen", "hydrogen_quick", 0.6, 0.90);
        let out = resolve_subagent_policy(&spec, summary, &default_targets());
        assert!(out.rewritten);
        assert_eq!(out.selected_model, "gemini-4-hydrogen-medium");
        assert_eq!(
            out.guard_applied.as_deref(),
            Some("author_floor_hydrogen_medium")
        );
    }

    #[test]
    fn swarm_author_worker_allows_hydrogen_high_and_xhigh() {
        let args = serde_json::json!({
            "action": "add",
            "name": "author",
            "workspace": "share",
            "brief": "Implement retry backoff and unit tests in client.rs"
        });
        let spec = extract_subagent_call("swarm", &args, "Gemini 4 Carbon (Medium)").unwrap();

        let sum_high = mock_summary("gemini-4-hydrogen-high", "hydrogen", "hydrogen_exec", 1.8, 0.90);
        let out_high = resolve_subagent_policy(&spec, sum_high, &default_targets());
        assert_eq!(out_high.selected_model, "gemini-4-hydrogen-high");
        assert!(out_high.guard_applied.is_none());

        let sum_xhigh =
            mock_summary("gemini-4-hydrogen-xhigh", "hydrogen", "hydrogen_exec", 2.4, 0.90);
        let out_xhigh = resolve_subagent_policy(&spec, sum_xhigh, &default_targets());
        assert_eq!(out_xhigh.selected_model, "gemini-4-hydrogen-xhigh");
        assert!(out_xhigh.guard_applied.is_none());
    }

    #[test]
    fn swarm_cross_family_claude_reviewer_is_preserved_and_clamped_from_xhigh_to_high() {
        let args = serde_json::json!({
            "action": "add",
            "name": "reviewer",
            "workspace": "inherit",
            "model": "claude-sonnet-5-5-low",
            "brief": "Audit lock ordering and race conditions across async channels"
        });
        let spec = extract_subagent_call("generic", &args, "Gemini 4 Carbon (Medium)").unwrap();
        let summary = mock_summary(
            "gemini-4-hydrogen-xhigh",
            "hydrogen",
            "hydrogen_exec",
            2.6,
            0.91,
        );
        let out = resolve_subagent_policy(&spec, summary, &default_targets());
        assert!(out.rewritten);
        assert_eq!(out.selected_model, "claude-sonnet-5-5-high");
    }

    #[test]
    fn invoke_subagent_research_scales_down_to_flash_on_carbon_parent() {
        let args = serde_json::json!({
            "TypeName": "research",
            "Role": "Config Finder",
            "Prompt": "Find all files defining DEFAULT_DECISIONS_ENDPOINT and list line numbers."
        });
        let spec =
            extract_subagent_call("invoke_subagent", &args, "Gemini 4 Carbon (High)").unwrap();
        let summary = mock_summary(
            "gemini-4-hydrogen-medium",
            "hydrogen",
            "hydrogen_quick",
            0.85,
            0.88,
        );
        let out = resolve_subagent_policy(&spec, summary, &default_targets());
        assert!(out.rewritten);
        assert_eq!(out.model_arg_key, "Model");
        assert_eq!(out.selected_model, "flash");
    }

    #[test]
    fn invoke_subagent_self_on_carbon_parent_keeps_inherit_when_complex() {
        let args = serde_json::json!({
            "TypeName": "self",
            "Role": "Concurrency Fixer",
            "Workspace": "branch",
            "Prompt": "Diagnose and fix deadlock in multi-worker dispatcher."
        });
        let spec =
            extract_subagent_call("invoke_subagent", &args, "Gemini 4 Carbon (High)").unwrap();
        let summary = mock_summary(
            "gemini-4-carbon-high",
            "carbon",
            "carbon_frontier",
            3.5,
            0.93,
        );
        let out = resolve_subagent_policy(&spec, summary, &default_targets());
        assert!(!out.rewritten);
        assert_eq!(out.selected_model, "inherit");
    }

    #[test]
    fn invoke_subagent_self_on_hydrogen_low_parent_upgrades_to_pro() {
        let args = serde_json::json!({
            "TypeName": "self",
            "Role": "Feature Builder",
            "Prompt": "Implement retry backoff and unit tests for client.rs"
        });
        let spec =
            extract_subagent_call("invoke_subagent", &args, "Gemini 4 Hydrogen (Low)").unwrap();
        let summary = mock_summary("gemini-4-neon-medium", "neon", "neon_balanced", 1.7, 0.85);
        let out = resolve_subagent_policy(&spec, summary, &default_targets());
        assert!(out.rewritten);
        assert_eq!(out.selected_model, "pro");
    }

    #[test]
    fn swarm_author_worker_allows_neon_xhigh() {
        let args = serde_json::json!({
            "action": "add",
            "name": "author",
            "workspace": "branch",
            "brief": "Refactor cross-module API routing and security checks"
        });
        let spec = extract_subagent_call("swarm", &args, "Gemini 4 Neon (High)").unwrap();
        let summary = mock_summary("gemini-4-neon-xhigh", "neon", "neon_balanced", 3.1, 0.91);
        let out = resolve_subagent_policy(&spec, summary, &default_targets());
        assert_eq!(out.selected_model, "gemini-4-neon-xhigh");
        assert!(out.guard_applied.is_none());
    }

    fn aligned_tiers() -> TierModels {
        TierModels::parse("pro=gemini-4-carbon-high, flash=gemini-4-neon-medium, flash-lite=gemini-4-hydrogen-low")
    }

    fn route_invoke(
        type_name: &str,
        workspace: Option<&str>,
        requested: Option<&str>,
        parent: &str,
        summary: RouteDecisionSummary,
        tiers: &TierModels,
    ) -> SubagentRouteOutcome {
        let mut args = serde_json::json!({ "TypeName": type_name, "Role": "Worker", "Prompt": "Do the task." });
        if let Some(ws) = workspace {
            args["Workspace"] = ws.into();
        }
        if let Some(m) = requested {
            args["Model"] = m.into();
        }
        let spec = extract_subagent_call("invoke_subagent", &args, parent).unwrap();
        resolve_invoke_subagent_policy(&spec, summary, tiers)
    }

    #[test]
    fn tier_models_parse_and_alignment() {
        assert!(!TierModels::server_default().family_aligned());
        let t = aligned_tiers();
        assert_eq!(t.flash_lite, "gemini-4-hydrogen-low");
        assert!(t.family_aligned());
        // 빠진 티어는 서버 기본값, 모르는 키는 무시
        let partial = TierModels::parse("pro=gemini-4-carbon-high,ultra=x,flash=");
        assert_eq!(partial.pro, "gemini-4-carbon-high");
        assert_eq!(partial.flash, TierModels::server_default().flash);
        assert!(!partial.family_aligned());
        assert!(model_rank("Gemini 4 Neon (High)") > model_rank("gemini-4-hydrogen-tiered-xhigh"));
        assert!(model_rank("gemini-4-hydrogen-xhigh") == model_rank("gemini-4-hydrogen-tiered-xhigh"));
    }

    #[test]
    fn server_tiers_keep_inherit_when_parent_ranks_above_pro() {
        let server = TierModels::server_default();
        for type_name in ["research", "self"] {
            let summary = mock_summary("gemini-4-carbon-medium", "carbon", "carbon_complex", 3.25, 0.77);
            let out = route_invoke(type_name, None, None, "gemini-4-neon-high", summary, &server);
            assert_eq!(out.selected_model, "inherit", "{type_name}");
            assert_eq!(out.guard_applied.as_deref(), Some("preserve_parent_above_pro"));
        }
        // 부모가 pro(Hydrogen xhigh) 이하면 그대로 pro 로 올린다
        let summary = mock_summary("gemini-4-carbon-medium", "carbon", "carbon_complex", 3.29, 0.75);
        let out = route_invoke("research", None, None, "gemini-4-hydrogen-xhigh", summary, &server);
        assert_eq!(out.selected_model, "pro");
        assert!(out.guard_applied.is_none());
    }

    #[test]
    fn aligned_tiers_route_each_family_to_its_tier() {
        let t = aligned_tiers();
        let s = |m: &str, fam: &str, tier: &str, score: f64| mock_summary(m, fam, tier, score, 0.85);

        // Carbon 작업: Neon 부모에서도 pro(=Carbon)로 올린다
        let out = route_invoke("research", None, None, "gemini-4-neon-high", s("gemini-4-carbon-medium", "carbon", "carbon_complex", 3.25), &t);
        assert_eq!((out.selected_model.as_str(), out.rewritten, out.guard_applied.as_deref()), ("pro", true, None));
        // 부모가 곧 Carbon High 면 inherit
        let out = route_invoke("self", Some("branch"), None, "Gemini 4 Carbon (High)", s("gemini-4-carbon-high", "carbon", "carbon_frontier", 3.5), &t);
        assert_eq!(out.selected_model, "inherit");
        // Neon 작업: Flash 부모 -> flash(=Neon medium)
        let out = route_invoke("self", None, None, "gemini-3.8-flash-low", s("gemini-4-neon-medium", "neon", "neon_balanced", 1.7), &t);
        assert_eq!(out.selected_model, "flash");
        // Neon High 를 원하고 부모가 Neon High 면 부모 유지
        let out = route_invoke("self", None, None, "gemini-4-neon-high", s("gemini-4-neon-high", "neon", "neon_balanced", 2.2), &t);
        assert_eq!(out.selected_model, "inherit");
        // Carbon 부모의 코드 변경 밸런스 작업은 부모 강도 유지
        let out = route_invoke("self", Some("branch"), None, "gemini-4-carbon-high", s("gemini-4-neon-medium", "neon", "neon_balanced", 1.2), &t);
        assert_eq!((out.selected_model.as_str(), out.guard_applied.as_deref()), ("inherit", Some("preserve_parent_carbon_effort")));
        // 간단한 리서치는 Hydrogen 부모라도 더 싼 flash_lite(=Hydrogen low)로
        let out = route_invoke("research", None, None, "gemini-4-hydrogen-high", s("gemini-4-hydrogen-low", "hydrogen", "hydrogen_quick", 0.4), &t);
        assert_eq!(out.selected_model, "flash_lite");
        // Hydrogen xhigh 를 원하는 리서치는 한 티어 위(flash)
        let out = route_invoke("research", None, None, "gemini-3.8-flash-low", s("gemini-4-hydrogen-xhigh", "hydrogen", "hydrogen_exec", 2.4), &t);
        assert_eq!(out.selected_model, "flash");
        // 코드를 쓰는 서브에이전트는 flash_lite 금지
        let out = route_invoke("self", None, None, "gemini-3.8-flash-low", s("gemini-4-hydrogen-medium", "hydrogen", "hydrogen_exec", 0.9), &t);
        assert_eq!((out.selected_model.as_str(), out.guard_applied.as_deref()), ("flash", Some("exec_subagent_floor")));
        // 명시적 pro 는 강등하지 않는다
        let out = route_invoke("self", None, Some("pro"), "gemini-4-neon-high", s("gemini-4-neon-medium", "neon", "neon_balanced", 1.4), &t);
        assert_eq!((out.selected_model.as_str(), out.guard_applied.as_deref()), ("pro", Some("explicit_pro_downgrade_guard")));
    }

}
