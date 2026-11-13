use std::{
    collections::HashSet,
    io::{self, IsTerminal},
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use agy_decision_lab::{
    render::{
        LiveProgressKind, StreamingMarkdownPrinter, ThemeFlavor, clear_live_progress_line,
        print_completed_tool_summary_line, render_live_progress_line, render_turn_summary_card,
        render_user_prompt_bubble, resolve_theme_flavor,
    },
    repl_ui::{
        ClipboardPayload, CommonSlashAction, DEFAULT_IMAGE_ANALYSIS_PROMPT, ReadLineTurnOutcome,
        SharedReplState, activate_resumed_conversation_state, apply_effort_override,
        attach_clipboard_image_interactive, build_prompt_with_images, build_reedline_editor,
        capture_clipboard_payload, clear_terminal_screen, collect_turn_images,
        consume_quotes_and_expand_prompt, current_terminal_width, dispatch_common_slash_command,
        erase_prompt_line_in_place, extract_file_path_from_info, extract_inline_image_attachments,
        extract_tool_target_label, format_active_tool_label, format_cumulative_tool_summary,
        load_lab_dotenv, maybe_exec_native_subcommand, print_common_status,
        print_conversation_resume_preview, print_repl_banner, read_repl_line, read_single_prompt,
        reset_terminal_mouse_and_modes, resolve_conversation_id_prefix,
        resolve_most_recent_conversation_id, resolve_targets, select_conversation_interactive,
    },
    router::{
        ImageAttachment, ModelTargets, RouteDecisionSummary, classify_prompt_with_media_timeout,
        env_or,
    },
};
use anyhow::{Context as _, Result, bail};
use clap::Parser;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};

const ROUTE_HISTORY_FILE: &str = "repl_history.txt";

/// Gemini Decision 1 기반 Antigravity CLI(agy) 10단계 스펙트럼 동적 라우터 (Hydrogen 4 / Argon 4 / Carbon 4)
#[derive(Debug, Parser)]
#[command(name = "agy-route", version, about)]
struct Cli {
    /// 멀티턴 동적 라우팅 REPL 모드로 실행
    #[arg(short = 'r', long = "repl")]
    repl: bool,

    /// 모델 패밀리 구성 프리셋 (spectrum | no-carbon | claude)
    #[arg(long)]
    preset: Option<String>,

    /// 터미널 색상 테마 (auto: OSC 11 배경 밝기 자동 감지 | light | dark)
    #[arg(long, env = "AGY_THEME", default_value = "auto")]
    theme: String,

    /// REPL 키 바인딩 모드 (emacs | vi)
    #[arg(long, env = "AGY_EDIT_MODE", default_value = "emacs")]
    edit_mode: String,

    /// 시작 시 모델 자동 라우팅 모드로 진입 (기본값은 gemini-4-carbon-medium 고정)
    #[arg(long)]
    auto: bool,

    /// 시작 시 고정할 특정 모델명 지정 (기본값: gemini-4-carbon-medium)
    #[arg(short = 'm', long = "model", env = "AGY_MODEL")]
    model: Option<String>,

    /// 마크다운 스타일 렌더링 끄기 (원본 텍스트 그대로 출력)
    #[arg(long)]
    raw_output: bool,

    /// 가장 최근 agy 대화를 이어서 실행 (-c / --continue 전달)
    #[arg(short = 'c', long = "continue")]
    continue_recent: bool,

    /// 특정 conversationId 세션을 지정해 이어서 실행 (전체 ID 또는 앞자리 프리픽스)
    #[arg(long = "conversation")]
    conversation_id: Option<String>,

    /// 실행 시 클립보드의 스크린샷 이미지를 즉시 첨부
    #[arg(long = "paste")]
    paste_clipboard: bool,

    /// 라우팅 판별 결과(JSON)만 출력하고 agy는 실행하지 않음
    #[arg(long)]
    dry_run: bool,

    /// 단일 출력 모드(-p / --print)로 1회 실행 후 종료
    #[arg(short = 'p', long = "print")]
    print_mode: bool,

    /// 확신도(confidence) 임계치
    #[arg(long, env = "AGY_CONFIDENCE_THRESHOLD", default_value_t = 0.60)]
    threshold: f64,

    /// Decisions API 호출 실패 시 폴백하지 않고 즉시 종료
    #[arg(long)]
    strict: bool,

    /// 자식 agy 프로세스에도 GEMINI_API_KEY 환경변수를 그대로 유지
    #[arg(long)]
    keep_api_key_env: bool,

    /// 실행할 첫 프롬프트 문자열 또는 agy 서브커맨드 (생략하고 터미널에서 실행하면 자동으로 멀티턴 REPL 모드로 진입)
    prompt: Option<String>,

    /// agy CLI에 추가로 전달할 인자 (`--` 뒤에 지정)
    #[arg(last = true)]
    agy_args: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct StreamEventEnvelope {
    #[serde(default)]
    event: String,
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    step_update: Option<StepUpdatePayload>,
    #[serde(default)]
    result: Option<TurnResultPayload>,
}

#[derive(Debug, Deserialize)]
struct StepUpdatePayload {
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    step_index: u64,
    #[serde(default)]
    state: String,
    #[serde(default)]
    step_type: String,
    #[serde(default)]
    text_delta: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    duration_seconds: Option<f64>,
    #[serde(default)]
    tool_info: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct TurnResultPayload {
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    duration_seconds: Option<f64>,
    #[serde(default)]
    usage: Option<TurnUsagePayload>,
}

#[derive(Debug, Deserialize)]
struct TurnUsagePayload {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

struct ResolvedModelChoice {
    model: String,
    cli_effort: Option<String>,
    summary: Option<RouteDecisionSummary>,
}

#[tokio::main]
async fn main() -> Result<()> {
    load_lab_dotenv();
    let agy_bin = env_or("AGY_BIN", "agy");
    maybe_exec_native_subcommand(&agy_bin)?;

    let mut cli = Cli::parse();
    if let Some(prefix) = cli.conversation_id.as_deref()
        && let Some(full_id) = resolve_conversation_id_prefix(prefix)
    {
        cli.conversation_id = Some(full_id);
    }

    let targets = resolve_targets(cli.preset.as_deref());

    let stdin_is_tty = io::stdin().is_terminal();
    if cli.repl || (stdin_is_tty && !cli.dry_run && !cli.print_mode) {
        return run_multiturn_repl(&cli, &agy_bin, &targets).await;
    }

    let (flavor, _) = if cli.dry_run {
        (ThemeFlavor::Dark, None)
    } else {
        resolve_theme_flavor(&cli.theme)
    };

    let mut images = Vec::new();
    if cli.paste_clipboard
        && let Ok(ClipboardPayload::Image(img)) = capture_clipboard_payload(1)
    {
        images.push(img);
    }

    let raw_prompt = read_single_prompt(cli.prompt.clone(), !images.is_empty(), "agy-route")?;
    for inline_img in extract_inline_image_attachments(&raw_prompt) {
        if !images.iter().any(|existing| existing.path == inline_img.path) {
            images.push(inline_img);
        }
    }

    let decision = decide_or_fallback(&raw_prompt, &images, &cli, &targets, flavor, None, None).await?;

    if cli.dry_run {
        if let Some(summary) = decision.summary {
            println!("{}", serde_json::to_string_pretty(&summary)?);
            return Ok(());
        }
        bail!("--dry-run 모드에서는 유효한 Decisions API 응답이 필요합니다.");
    }

    let final_prompt = build_prompt_with_images(&raw_prompt, &images);

    let mut cmd = build_sync_agy_command(
        &agy_bin,
        &decision.model,
        decision.cli_effort.as_deref(),
        cli.keep_api_key_env,
    );
    if let Some(conv_id) = &cli.conversation_id {
        cmd.arg("--conversation").arg(conv_id);
    } else if cli.continue_recent {
        cmd.arg("--continue");
    }

    if cli.print_mode {
        cmd.stdin(Stdio::null());
        cmd.arg("--print").arg(&final_prompt);
    } else {
        cmd.arg("-i").arg(&final_prompt);
    }
    cmd.args(&cli.agy_args);

    let exec_err = cmd.exec();
    bail!("'{}' 프로세스 실행 교체(exec) 실패: {}", agy_bin, exec_err)
}

async fn decide_or_fallback(
    prompt: &str,
    images: &[ImageAttachment],
    cli: &Cli,
    targets: &ModelTargets,
    flavor: ThemeFlavor,
    pinned_model: Option<&str>,
    pinned_effort: Option<&str>,
) -> Result<ResolvedModelChoice> {
    let accent = flavor.accent_ansi();
    let warn = flavor.warn_ansi();

    if let Some(manual_model) = pinned_model {
        let (final_model, cli_effort) = apply_effort_override(manual_model, pinned_effort);
        if !cli.dry_run {
            let eff_label = cli_effort.as_deref().or(pinned_effort).unwrap_or("baked");
            let img_note = if images.is_empty() {
                String::new()
            } else {
                format!(", images={}", images.len())
            };
            eprintln!(
                "{accent}[agy-route]\x1b[0m model=\x1b[1m{}\x1b[0m (pinned, effort={}{img_note})",
                final_model, eff_label
            );
        }
        return Ok(ResolvedModelChoice {
            model: final_model,
            cli_effort,
            summary: None,
        });
    }

    if prompt.trim_start().starts_with('/') && images.is_empty() && !cli.dry_run {
        let base_model = targets.variant("argon", "medium");
        let (model, cli_effort) = apply_effort_override(&base_model, pinned_effort);
        eprintln!(
            "{accent}[agy-route]\x1b[0m 내장 명령 전달 -> model=\x1b[1m{}\x1b[0m",
            model
        );
        return Ok(ResolvedModelChoice {
            model,
            cli_effort,
            summary: None,
        });
    }

    let show_routing_spinner = !cli.dry_run && !cli.raw_output && io::stderr().is_terminal();
    let classify_res = if show_routing_spinner {
        let route_start = Instant::now();
        let mut frame_idx = 0usize;
        let mut tick = tokio::time::interval(Duration::from_millis(80));
        let classify_fut = classify_prompt_with_media_timeout(
            prompt,
            images,
            cli.threshold,
            targets,
            Duration::from_secs(12),
        );
        tokio::pin!(classify_fut);
        let res = loop {
            tokio::select! {
                out = &mut classify_fut => break out,
                _ = tick.tick() => {
                    render_live_progress_line(
                        frame_idx,
                        LiveProgressKind::Routing,
                        route_start.elapsed().as_secs_f64(),
                        flavor,
                    );
                    frame_idx = frame_idx.wrapping_add(1);
                }
            }
        };
        clear_live_progress_line();
        res
    } else {
        classify_prompt_with_media_timeout(
            prompt,
            images,
            cli.threshold,
            targets,
            Duration::from_secs(12),
        )
        .await
    };

    match classify_res {
        Ok(mut summary) => {
            if let Some(eff) = pinned_effort {
                let (overridden_model, overridden_flag) =
                    apply_effort_override(&summary.selected_model, Some(eff));
                summary.selected_model = overridden_model;
                summary.cli_effort_flag = overridden_flag;
                summary.recommended_effort = eff.to_string();
            }
            if !cli.dry_run {
                let img_badge = if images.is_empty() {
                    String::new()
                } else {
                    format!(", 📷x{}", images.len())
                };
                eprintln!(
                    "{accent}[agy-route]\x1b[0m model=\x1b[1m{}\x1b[0m (tier={}, score={:.2}, conf={:.0}%{}, {}ms)",
                    summary.selected_model,
                    summary.task_tier,
                    summary.complexity_score,
                    summary.tier_confidence * 100.0,
                    img_badge,
                    summary.latency_ms
                );
            }
            Ok(ResolvedModelChoice {
                model: summary.selected_model.clone(),
                cli_effort: summary.cli_effort_flag.clone(),
                summary: Some(summary),
            })
        }
        Err(err) => {
            if cli.strict || cli.dry_run {
                return Err(err);
            }
            let fallback_base = targets.variant("argon", "medium");
            let (fallback_model, cli_effort) =
                apply_effort_override(&fallback_base, pinned_effort);
            eprintln!(
                "{warn}[agy-route]\x1b[0m 판별 생략 ({err:#}) -> 기본 모델({})로 실행합니다.",
                fallback_model
            );
            Ok(ResolvedModelChoice {
                model: fallback_model,
                cli_effort,
                summary: None,
            })
        }
    }
}

fn build_sync_agy_command(
    agy_bin: &str,
    model: &str,
    cli_effort: Option<&str>,
    keep_api_key_env: bool,
) -> Command {
    let mut cmd = Command::new(agy_bin);
    cmd.env("AGY_ROUTE_ACTIVE", "1");
    if !keep_api_key_env {
        cmd.env_remove("GEMINI_API_KEY");
    }
    cmd.arg("--model").arg(model);
    if let Some(effort) = cli_effort {
        cmd.arg("--effort").arg(effort);
    }
    cmd
}

async fn run_multiturn_repl(cli: &Cli, agy_bin: &str, initial_targets: &ModelTargets) -> Result<()> {
    reset_terminal_mouse_and_modes();

    let (active_flavor, detected_luma) = resolve_theme_flavor(&cli.theme);
    let mut line_editor = build_reedline_editor(&cli.edit_mode, active_flavor, ROUTE_HISTORY_FILE)?;

    let mut active_conv_id = cli.conversation_id.clone();
    let mut use_continue_flag = cli.continue_recent;
    if active_conv_id.is_none() && use_continue_flag {
        active_conv_id = resolve_most_recent_conversation_id();
        if active_conv_id.is_some() {
            use_continue_flag = false;
        }
    }

    let mut state = SharedReplState::new(
        "agy-route",
        ROUTE_HISTORY_FILE,
        initial_targets.clone(),
        active_flavor,
        detected_luma,
        cli.edit_mode.clone(),
        active_conv_id,
        use_continue_flag,
    );

    if cli.auto {
        state.pinned_model = None;
    } else if let Some(custom_model) = cli.model.as_deref() {
        let (resolved, _) = apply_effort_override(custom_model, None);
        state.pinned_model = Some(resolved.clone());
        state.last_model = resolved;
    }

    print_repl_banner(&state, "9단계 스펙트럼 REPL");

    if cli.paste_clipboard {
        attach_clipboard_image_interactive(&mut state, None);
    }

    if let Some(cid) = state.active_conv_id.as_deref() {
        print_conversation_resume_preview("agy-route", cid, state.active_flavor);
    }

    if let Some(initial_prompt) = cli.prompt.as_deref()
        && (!initial_prompt.trim().is_empty() || !state.pending_images.is_empty())
    {
        state.turn_idx += 1;
        let effective = if initial_prompt.trim().is_empty() {
            DEFAULT_IMAGE_ANALYSIS_PROMPT
        } else {
            initial_prompt
        };
        execute_repl_turn(effective, cli, agy_bin, &mut state).await?;
        if cli.print_mode || (!cli.repl && !io::stdin().is_terminal()) {
            reset_terminal_mouse_and_modes();
            return Ok(());
        }
    }

    let mut skip_newline_before_prompt = false;

    loop {
        reset_terminal_mouse_and_modes();
        if !skip_newline_before_prompt {
            eprintln!();
        }
        skip_newline_before_prompt = false;

        let (line, left_seg) = match read_repl_line(&mut line_editor, &mut state) {
            ReadLineTurnOutcome::Submit {
                raw_line,
                left_prompt,
            } => (raw_line, left_prompt),
            ReadLineTurnOutcome::ContinueLoop { skip_newline } => {
                skip_newline_before_prompt = skip_newline;
                continue;
            }
            ReadLineTurnOutcome::Exit => break,
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !state.pending_images.is_empty() {
                erase_prompt_line_in_place(&left_seg, &line);
                state.turn_idx += 1;
                if let Err(err) =
                    execute_repl_turn(DEFAULT_IMAGE_ANALYSIS_PROMPT, cli, agy_bin, &mut state).await
                {
                    let warn = state.active_flavor.warn_ansi();
                    eprintln!("{warn}[agy-route] 턴 실행 오류: {err:#}\x1b[0m");
                }
            }
            continue;
        }

        match dispatch_common_slash_command(trimmed, &mut state, &mut line_editor)? {
            CommonSlashAction::NotSlash => {}
            CommonSlashAction::Handled => continue,
            CommonSlashAction::Exit => break,
            CommonSlashAction::NewSession { clear_screen } => {
                state.active_conv_id = None;
                state.use_continue_flag = false;
                state.turn_idx = 0;
                state.pending_images.clear();
                let accent = state.active_flavor.accent_ansi();
                if clear_screen {
                    clear_terminal_screen();
                    print_repl_banner(&state, "9단계 스펙트럼 REPL");
                    eprintln!("{accent}[agy-route]\x1b[0m 화면을 지우고 새 대화 세션으로 초기화했습니다.");
                } else {
                    eprintln!("{accent}[agy-route]\x1b[0m 새 대화 세션으로 초기화되었습니다.");
                }
                continue;
            }
            CommonSlashAction::Resume { arg } => {
                if let Some(selected_cid) = select_conversation_interactive(&arg, &state, &[]) {
                    activate_resumed_conversation_state(&selected_cid, &mut state);
                }
                continue;
            }
            CommonSlashAction::RunPrompt(prompt_to_run) => {
                state.turn_idx += 1;
                if let Err(err) = execute_repl_turn(&prompt_to_run, cli, agy_bin, &mut state).await
                {
                    let warn = state.active_flavor.warn_ansi();
                    eprintln!("{warn}[agy-route] 턴 실행 오류: {err:#}\x1b[0m");
                }
                continue;
            }
            CommonSlashAction::Status => {
                print_common_status(&state, None);
                continue;
            }
            CommonSlashAction::SwitchToTui => {
                reset_terminal_mouse_and_modes();
                let accent = state.active_flavor.accent_ansi();
                eprintln!("{accent}[agy-route]\x1b[0m 현재 세션을 순정 agy TUI로 전환합니다...");
                let active_model = state
                    .pinned_model
                    .as_deref()
                    .unwrap_or(&state.last_model);
                let mut cmd = build_sync_agy_command(
                    agy_bin,
                    active_model,
                    state
                        .pinned_effort
                        .as_deref()
                        .or(state.last_effort.as_deref()),
                    cli.keep_api_key_env,
                );
                cmd.env_remove("AGY_ROUTE_ACTIVE");
                if let Some(id) = &state.active_conv_id {
                    cmd.arg("--conversation").arg(id);
                } else if state.use_continue_flag || state.turn_idx > 0 {
                    cmd.arg("--continue");
                }
                cmd.args(&cli.agy_args);
                let err = cmd.exec();
                bail!("순정 agy TUI 전환 실패: {err}");
            }
        }

        erase_prompt_line_in_place(&left_seg, &line);

        state.turn_idx += 1;
        let expanded_prompt = consume_quotes_and_expand_prompt(trimmed, &mut state);
        if let Err(err) = execute_repl_turn(&expanded_prompt, cli, agy_bin, &mut state).await {
            let warn = state.active_flavor.warn_ansi();
            eprintln!("{warn}[agy-route] 턴 실행 오류: {err:#}\x1b[0m");
        }
    }

    reset_terminal_mouse_and_modes();
    if let Some(id) = state.active_conv_id {
        eprintln!(
            "[agy-route] 종료됨. 이어서 작업하려면: agy-route --conversation {id} (또는 REPL에서 /resume)"
        );
    }
    Ok(())
}

enum RouteProgressPhase {
    Thinking {
        started_at: Instant,
        completed_tools: Option<String>,
    },
    ToolRunning {
        label: String,
        started_at: Instant,
    },
    Streaming,
}

async fn execute_repl_turn(
    prompt: &str,
    cli: &Cli,
    agy_bin: &str,
    state: &mut SharedReplState,
) -> Result<()> {
    let turn_images = collect_turn_images(&mut state.pending_images, prompt);

    let term_width = current_terminal_width();
    let img_refs: Vec<&Path> = turn_images.iter().map(|img| img.path.as_path()).collect();
    render_user_prompt_bubble(prompt, &img_refs, state.active_flavor, term_width);

    let decision = decide_or_fallback(
        prompt,
        &turn_images,
        cli,
        &state.targets,
        state.active_flavor,
        state.pinned_model.as_deref(),
        state.pinned_effort.as_deref(),
    )
    .await?;
    state.last_model = decision.model.clone();
    state.last_effort = decision.cli_effort.clone();

    let final_prompt = build_prompt_with_images(prompt, &turn_images);

    if cli.raw_output {
        let mut cmd = build_sync_agy_command(
            agy_bin,
            &decision.model,
            decision.cli_effort.as_deref(),
            cli.keep_api_key_env,
        );
        if let Some(id) = state.active_conv_id.as_deref() {
            cmd.arg("--conversation").arg(id);
        } else if state.use_continue_flag || state.turn_idx > 1 {
            cmd.arg("--continue");
        }
        cmd.stdin(Stdio::null());
        cmd.arg("--print").arg(&final_prompt);
        cmd.args(&cli.agy_args);
        cmd.status()
            .with_context(|| format!("'{}' 호출 실패", agy_bin))?;
        reset_terminal_mouse_and_modes();
        return Ok(());
    }

    let mut cmd = tokio::process::Command::new(agy_bin);
    cmd.env("AGY_ROUTE_ACTIVE", "1");
    if !cli.keep_api_key_env {
        cmd.env_remove("GEMINI_API_KEY");
    }
    cmd.arg("--model").arg(&decision.model);
    if let Some(effort) = decision.cli_effort.as_deref() {
        cmd.arg("--effort").arg(effort);
    }
    if let Some(id) = state.active_conv_id.as_deref() {
        cmd.arg("--conversation").arg(id);
    } else if state.use_continue_flag || state.turn_idx > 1 {
        cmd.arg("--continue");
    }
    cmd.arg("--output-format")
        .arg("stream-json")
        .arg("--print")
        .arg(&final_prompt);
    cmd.args(&cli.agy_args);

    cmd.process_group(0);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::inherit());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("'{}' 프로세스 시작 실패", agy_bin))?;

    let stdout = child.stdout.take().context("stdout 파이프 연결 실패")?;
    let mut lines = BufReader::new(stdout).lines();
    let mut md_printer = StreamingMarkdownPrinter::new(state.active_flavor);
    let mut tool_tracker = ToolActivityTracker::default();
    let mut pending_tool_summary_to_print: Option<(String, f64)> = None;
    let mut turn_stats: Option<(f64, u64, u64)> = None;
    let warn = state.active_flavor.warn_ansi();
    let mut canceled = false;
    let mut progress_phase = RouteProgressPhase::Thinking {
        started_at: Instant::now(),
        completed_tools: None,
    };
    let mut spinner_idx = 0usize;
    let mut tick_timer = tokio::time::interval(Duration::from_millis(80));
    let mut has_active_status_line = false;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c(), if !canceled => {
                canceled = true;
                if has_active_status_line {
                    clear_live_progress_line();
                    has_active_status_line = false;
                }
                md_printer.finish();
                eprintln!("\n{warn}[agy-route] Ctrl+C 감지 — 현재 턴 실행을 중단합니다...\x1b[0m");
                let _ = child.kill().await;
                break;
            }
            _ = tick_timer.tick(), if !matches!(progress_phase, RouteProgressPhase::Streaming) => {
                match &progress_phase {
                    RouteProgressPhase::Thinking { started_at, completed_tools } => {
                        render_live_progress_line(
                            spinner_idx,
                            LiveProgressKind::Thinking { completed_tools: completed_tools.as_deref() },
                            started_at.elapsed().as_secs_f64(),
                            state.active_flavor,
                        );
                        has_active_status_line = true;
                    }
                    RouteProgressPhase::ToolRunning { label, started_at } => {
                        render_live_progress_line(
                            spinner_idx,
                            LiveProgressKind::ToolRunning { label },
                            started_at.elapsed().as_secs_f64(),
                            state.active_flavor,
                        );
                        has_active_status_line = true;
                    }
                    RouteProgressPhase::Streaming => {}
                }
                spinner_idx = spinner_idx.wrapping_add(1);
            }
            line_res = lines.next_line() => {
                let Some(raw_line) = line_res? else {
                    break;
                };
                let trimmed = raw_line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(ev) = serde_json::from_str::<StreamEventEnvelope>(trimmed) else {
                    if has_active_status_line {
                        clear_live_progress_line();
                        has_active_status_line = false;
                    }
                    println!("{raw_line}");
                    continue;
                };

                if let Some(cid) = ev
                    .conversation_id
                    .as_deref()
                    .or_else(|| {
                        ev.step_update
                            .as_ref()
                            .and_then(|s| s.conversation_id.as_deref())
                    })
                    .or_else(|| {
                        ev.result
                            .as_ref()
                            .and_then(|r| r.conversation_id.as_deref())
                    })
                    && !cid.is_empty()
                    && state.active_conv_id.is_none()
                {
                    state.active_conv_id = Some(cid.to_string());
                    state.use_continue_flag = false;
                }

                match ev.event.as_str() {
                    "step_update" => {
                        if let Some(step) = ev.step_update {
                            if step.step_type == "agent_response" {
                                if let Some(delta) = step.text_delta
                                    && !delta.is_empty()
                                {
                                    if has_active_status_line {
                                        clear_live_progress_line();
                                        has_active_status_line = false;
                                    }
                                    if !delta.trim().is_empty()
                                        && let Some((sum, el)) = pending_tool_summary_to_print.take()
                                    {
                                        print_completed_tool_summary_line(&sum, el, state.active_flavor);
                                    }
                                    progress_phase = RouteProgressPhase::Streaming;
                                    md_printer.push_delta(&delta);
                                }
                            } else if step.step_type == "tool" {
                                if matches!(progress_phase, RouteProgressPhase::Streaming) {
                                    md_printer.finish();
                                    println!();
                                }
                                if let Some((is_running, label_or_summary, started, elapsed)) =
                                    tool_tracker.on_tool_step(&step)
                                {
                                    if is_running {
                                        progress_phase = RouteProgressPhase::ToolRunning {
                                            label: label_or_summary,
                                            started_at: started,
                                        };
                                    } else {
                                        pending_tool_summary_to_print =
                                            Some((label_or_summary.clone(), elapsed));
                                        progress_phase = RouteProgressPhase::Thinking {
                                            started_at: Instant::now(),
                                            completed_tools: Some(label_or_summary),
                                        };
                                    }
                                }
                            }
                        }
                    }
                    "result" => {
                        if let Some(res) = ev.result {
                            let dur = res.duration_seconds.unwrap_or(0.0);
                            let (in_tok, out_tok) = res
                                .usage
                                .map(|u| (u.input_tokens, u.output_tokens))
                                .unwrap_or((0, 0));
                            turn_stats = Some((dur, in_tok, out_tok));
                            if res.status != "SUCCESS" && !res.status.is_empty() {
                                if has_active_status_line {
                                    clear_live_progress_line();
                                    has_active_status_line = false;
                                }
                                eprintln!("{warn}  [상태: {}]\x1b[0m", res.status);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if has_active_status_line {
        clear_live_progress_line();
    }
    if let Some((sum, el)) = pending_tool_summary_to_print.take() {
        print_completed_tool_summary_line(&sum, el, state.active_flavor);
    }
    let (blocks, raw_resp) = md_printer.finish();
    let status = child.wait().await?;
    reset_terminal_mouse_and_modes();

    state.record_turn_output(blocks, raw_resp);

    if let Some((dur, in_tok, out_tok)) = turn_stats {
        render_turn_summary_card(
            dur,
            in_tok,
            out_tok,
            &tool_tracker.edited_files,
            state.active_flavor,
        );
    } else if !canceled && !status.success() {
        eprintln!(
            "{warn}[agy-route] 경고: agy 종료 코드 {:?}\x1b[0m",
            status.code()
        );
    }

    Ok(())
}

#[derive(Default)]
struct ToolActivityTracker {
    read_count: usize,
    edit_count: usize,
    command_count: usize,
    other_count: usize,
    edited_files: Vec<String>,
    seen_active_steps: HashSet<u64>,
    tool_start_instant: Option<Instant>,
}

impl ToolActivityTracker {
    fn on_tool_step(&mut self, step: &StepUpdatePayload) -> Option<(bool, String, Instant, f64)> {
        let tool_name = step.tool_name.as_deref().unwrap_or("tool");
        let tool_info = step.tool_info.as_ref();
        let target_param = extract_tool_target_label(tool_name, tool_info);

        if step.state == "ACTIVE" {
            if self.seen_active_steps.insert(step.step_index) {
                self.record_tool_count(tool_name, tool_info);
            }
            if self.tool_start_instant.is_none() {
                self.tool_start_instant = Some(Instant::now());
            }
            let started = self.tool_start_instant.unwrap_or_else(Instant::now);
            let elapsed = started.elapsed().as_secs_f64();
            let action_label = format_active_tool_label(tool_name, target_param.as_deref());
            Some((true, action_label, started, elapsed))
        } else if step.state == "DONE" {
            let started = self.tool_start_instant.unwrap_or_else(Instant::now);
            let elapsed = step
                .duration_seconds
                .unwrap_or_else(|| started.elapsed().as_secs_f64());
            let cum = format_cumulative_tool_summary(
                self.command_count,
                self.edit_count,
                self.read_count,
                self.other_count,
            );
            Some((false, cum, started, elapsed))
        } else {
            None
        }
    }

    fn record_tool_count(&mut self, tool_name: &str, tool_info: Option<&serde_json::Value>) {
        match tool_name {
            "read_file" | "view_file" | "list_dir" | "grep_search" | "find_by_name"
            | "read_resource" => {
                self.read_count += 1;
            }
            "write_to_file" | "replace_file_content" | "multi_replace_file_content"
            | "sed_file" | "write_file" | "edit_file" => {
                self.edit_count += 1;
                if let Some(path) = extract_file_path_from_info(tool_info)
                    && !self.edited_files.contains(&path)
                {
                    self.edited_files.push(path);
                }
            }
            "run_command" | "send_command_input" => {
                self.command_count += 1;
            }
            _ => {
                self.other_count += 1;
            }
        }
    }
}
