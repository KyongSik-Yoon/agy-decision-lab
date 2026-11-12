use std::{
    collections::HashMap,
    io::{self, IsTerminal, Write},
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use agy_decision_lab::{
    render::{
        LiveProgressKind, StreamingMarkdownPrinter, ThemeFlavor, clear_live_progress_line,
        print_completed_tool_summary_line, render_live_progress_line, render_turn_summary_card,
        render_user_prompt_bubble, resolve_theme_flavor,
    },
    repl_ui::{
        CommonSlashAction, ConversationRow, DEFAULT_IMAGE_ANALYSIS_PROMPT, ReadLineTurnOutcome,
        SharedReplState, activate_resumed_conversation_state, apply_effort_override,
        attach_clipboard_image_interactive, build_reedline_editor, clear_terminal_screen,
        collect_turn_images, consume_quotes_and_expand_prompt, current_terminal_width,
        dispatch_common_slash_command, erase_prompt_line_in_place, format_active_tool_label,
        format_cumulative_tool_summary, load_lab_dotenv, maybe_exec_native_subcommand,
        print_common_status, print_conversation_resume_preview, print_repl_banner, read_repl_line,
        read_single_prompt, reset_terminal_mouse_and_modes, resolve_conversation_id_prefix,
        resolve_targets, select_conversation_interactive,
    },
    router::{ModelTargets, classify_prompt_with_media_timeout, env_or},
};
use anyhow::{Context as _, Result, bail};
use clap::Parser;
use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};

const ACP_HISTORY_FILE: &str = "acp_repl_history.txt";

/// 상주 `agy acp` 프로세스 위에서 콜드 스타트 0ms로 매 턴 모델을 자동 교체(`session/set_model`)하는 동적 라우터 및 표준 ACP 프록시
#[derive(Debug, Parser)]
#[command(name = "agy-acp", version, about)]
struct Cli {
    /// Zed / Neovim 등 외부 ACP 에디터용 투과 JSON-RPC stdio 프록시 모드로 실행
    #[arg(long)]
    stdio_proxy: bool,

    /// 멀티턴 동적 라우팅 REPL 모드로 실행 (`agy-route -r` 호환)
    #[arg(short = 'r', long = "repl")]
    repl: bool,

    /// 모델 패밀리 구성 프리셋 (spectrum | no-carbon | claude)
    #[arg(long)]
    preset: Option<String>,

    /// 터미널 색상 테마 (auto | light | dark)
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

    /// 마크다운 스타일 렌더링 끄기 (원본 텍스트 그대로 스트리밍 출력)
    #[arg(long)]
    raw_output: bool,

    /// 가장 최근 agy 대화를 상주 세션으로 복원해 이어서 실행
    #[arg(short = 'c', long = "continue")]
    continue_recent: bool,

    /// 특정 conversationId(sessionId) 세션을 지정해 복원
    #[arg(long = "conversation")]
    conversation_id: Option<String>,

    /// 시작 직후 클립보드 스크린샷 첨부
    #[arg(long = "paste")]
    paste_clipboard: bool,

    /// 단일 프롬프트 1회 실행 후 종료
    #[arg(short = 'p', long = "print")]
    print_mode: bool,

    /// 라우팅 판별 결과(JSON)만 출력하고 종료
    #[arg(long)]
    dry_run: bool,

    /// Decisions API 호출 실패 시 폴백하지 않고 즉시 종료
    #[arg(long)]
    strict: bool,

    /// 확신도(confidence) 임계치
    #[arg(long, env = "AGY_CONFIDENCE_THRESHOLD", default_value_t = 0.60)]
    threshold: f64,

    /// 자식 agy acp 프로세스에도 GEMINI_API_KEY 환경변수 유지
    #[arg(long)]
    keep_api_key_env: bool,

    /// 첫 프롬프트 문자열 (생략 시 대화형 상주 REPL 진입)
    prompt: Option<String>,

    /// `agy acp` 실행 시 추가로 전달할 인자 (`--` 뒤에 지정)
    #[arg(last = true)]
    acp_args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AcpSessionItem {
    session_id: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    updated_at: String,
}

impl AcpSessionItem {
    fn to_conversation_row(&self) -> ConversationRow {
        let ws_uris = if self.cwd.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&vec![format!("file://{}", self.cwd)]).unwrap_or_default()
        };
        ConversationRow {
            conversation_id: self.session_id.clone(),
            title: self.title.clone(),
            preview: String::new(),
            step_count: 0,
            last_modified_time: self.updated_at.clone(),
            workspace_uris: ws_uris,
        }
    }
}

struct AcpDaemonClient {
    agy_bin: String,
    keep_api_key_env: bool,
    extra_args: Vec<String>,
    child: Child,
    stdin: ChildStdin,
    lines: tokio::io::Lines<BufReader<ChildStdout>>,
    next_id: AtomicU64,
    active_model_on_server: Option<String>,
}

impl AcpDaemonClient {
    async fn spawn(agy_bin: &str, keep_api_key_env: bool, extra_args: &[String]) -> Result<Self> {
        let mut cmd = tokio::process::Command::new(agy_bin);
        cmd.arg("acp");
        cmd.args(extra_args);
        cmd.env("AGY_ROUTE_ACTIVE", "1");
        if !keep_api_key_env {
            cmd.env_remove("GEMINI_API_KEY");
        }
        // 자식 프로세스를 독자 프로세스 그룹으로 분리하여 터미널 TTY의 Ctrl+C(SIGINT) 전파를 차단
        // (턴 중단은 부모가 정상적인 ACP session/cancel RPC로 제어)
        cmd.process_group(0);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("'{} acp' 상주 프로세스 시작 실패", agy_bin))?;
        let stdin = child.stdin.take().context("ACP stdin 파이프 열기 실패")?;
        let stdout = child.stdout.take().context("ACP stdout 파이프 열기 실패")?;

        let mut client = Self {
            agy_bin: agy_bin.to_string(),
            keep_api_key_env,
            extra_args: extra_args.to_vec(),
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: AtomicU64::new(1),
            active_model_on_server: None,
        };

        client
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                }),
            )
            .await
            .context("ACP initialize 핸드셰이크 실패")?;

        Ok(client)
    }

    fn is_dead(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)) | Err(_))
    }

    async fn respawn(&mut self) -> Result<()> {
        let _ = self.child.kill().await;
        let fresh = Self::spawn(&self.agy_bin, self.keep_api_key_env, &self.extra_args).await?;
        *self = fresh;
        Ok(())
    }

    /// 데몬이 예기치 않게 종료되었을 때 새 데몬을 띄우고 기존 세션을 투명하게 복구합니다.
    async fn recover_session(
        &mut self,
        session_id: &str,
        cwd: &str,
        flavor: ThemeFlavor,
        quiet_chrome: bool,
    ) -> Result<String> {
        if !quiet_chrome {
            clear_live_progress_line();
            let warn = flavor.warn_ansi();
            eprintln!(
                "{warn}[agy-acp]\x1b[0m 상주 ACP 데몬 연결이 끊어져 자동 재연결 및 세션 복구를 수행합니다..."
            );
        }
        self.respawn().await?;
        self.resume_session(session_id, cwd).await
    }

    async fn ensure_alive_for_session(
        &mut self,
        session_id: &str,
        cwd: &str,
        flavor: ThemeFlavor,
        quiet_chrome: bool,
    ) -> Result<String> {
        if self.is_dead() {
            self.recover_session(session_id, cwd, flavor, quiet_chrome)
                .await
        } else {
            Ok(session_id.to_string())
        }
    }

    async fn send_json(&mut self, value: &serde_json::Value) -> Result<()> {
        if let Ok(Some(status)) = self.child.try_wait() {
            bail!(
                "상주 ACP 데몬 프로세스가 종료되었습니다 (종료 코드: {:?})",
                status.code()
            );
        }
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("ACP 데몬으로 명령 전송 중 파이프 단절 (Broken pipe)")?;
        self.stdin.flush().await.context("ACP 데몬 파이프 flush 실패")?;
        Ok(())
    }

    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let req_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
            "params": params,
        });
        self.send_json(&payload).await?;

        while let Some(raw_line) = self.lines.next_line().await? {
            let trimmed = raw_line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };

            if msg.get("id").and_then(|v| v.as_u64()) == Some(req_id)
                && (msg.get("result").is_some() || msg.get("error").is_some())
            {
                if let Some(err) = msg.get("error") {
                    bail!("ACP '{}' 오류: {}", method, err);
                }
                return Ok(msg.get("result").cloned().unwrap_or(serde_json::json!({})));
            }
        }

        bail!(
            "ACP 서버 연결이 예기치 않게 종료되었습니다 ('{}' 대기 중)",
            method
        )
    }

    async fn new_session(&mut self, cwd: &str) -> Result<String> {
        if self.is_dead() {
            self.respawn().await?;
        }
        let res = self
            .request(
                "session/new",
                serde_json::json!({
                    "cwd": cwd,
                    "mcpServers": [],
                }),
            )
            .await?;
        let sid = res
            .get("sessionId")
            .and_then(|v| v.as_str())
            .context("session/new 응답에 sessionId가 없습니다")?
            .to_string();

        if let Some(cur_m) = res
            .get("models")
            .and_then(|m| m.get("currentModelId"))
            .and_then(|v| v.as_str())
        {
            self.active_model_on_server = Some(cur_m.to_string());
        }

        let _ = self
            .request(
                "session/set_mode",
                serde_json::json!({
                    "sessionId": sid,
                    "modeId": "auto",
                }),
            )
            .await;

        Ok(sid)
    }

    async fn resume_session(&mut self, session_id: &str, cwd: &str) -> Result<String> {
        if self.is_dead() {
            self.respawn().await?;
        }
        let res = self
            .request(
                "session/resume",
                serde_json::json!({
                    "sessionId": session_id,
                    "cwd": cwd,
                    "mcpServers": [],
                }),
            )
            .await?;
        let sid = res
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or(session_id)
            .to_string();
        self.active_model_on_server = None;

        let _ = self
            .request(
                "session/set_mode",
                serde_json::json!({
                    "sessionId": sid,
                    "modeId": "auto",
                }),
            )
            .await;

        Ok(sid)
    }

    async fn list_sessions(&mut self, cwd: &str) -> Result<Vec<AcpSessionItem>> {
        if self.is_dead() {
            self.respawn().await?;
        }
        let res = self
            .request("session/list", serde_json::json!({ "cwd": cwd }))
            .await?;
        let sessions = res
            .get("sessions")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([]));
        Ok(serde_json::from_value(sessions).unwrap_or_default())
    }

    async fn ensure_model(&mut self, session_id: &str, model_id: &str) -> Result<u128> {
        if !self.is_dead() && self.active_model_on_server.as_deref() == Some(model_id) {
            return Ok(0);
        }
        let t0 = Instant::now();
        self.request(
            "session/set_model",
            serde_json::json!({
                "sessionId": session_id,
                "modelId": model_id,
            }),
        )
        .await?;
        self.active_model_on_server = Some(model_id.to_string());
        Ok(t0.elapsed().as_millis())
    }

    /// 모델 전환 중 파이프 단절이 감지되면 1회 자동 재연결·세션 복구 후 재시도합니다.
    async fn ensure_model_resilient(
        &mut self,
        session_id: &mut String,
        model_id: &str,
        cwd: &str,
        flavor: ThemeFlavor,
        quiet_chrome: bool,
    ) -> Result<u128> {
        *session_id = self
            .ensure_alive_for_session(session_id, cwd, flavor, quiet_chrome)
            .await?;
        match self.ensure_model(session_id, model_id).await {
            Ok(ms) => Ok(ms),
            Err(_) => {
                *session_id = self
                    .recover_session(session_id, cwd, flavor, quiet_chrome)
                    .await?;
                self.ensure_model(session_id, model_id).await
            }
        }
    }

    async fn cancel_turn(&mut self, session_id: &str) -> Result<()> {
        self.send_json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": session_id },
        }))
        .await
    }

    async fn shutdown(&mut self) {
        let _ = self.child.kill().await;
    }
}

enum TurnProgressPhase {
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

#[derive(Default)]
struct AcpToolTracker {
    read_count: usize,
    edit_count: usize,
    command_count: usize,
    other_count: usize,
    edited_files: Vec<String>,
    seen_ids: HashMap<String, String>,
    started_at: Option<Instant>,
    active_label: Option<String>,
    last_summary: Option<String>,
}

impl AcpToolTracker {
    fn on_tool_event(
        &mut self,
        update: &serde_json::Value,
    ) -> Option<(bool, String, Instant)> {
        let call_id = update
            .get("toolCallId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let name = update
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("tool");
        let kind = update.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let status = update.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let title_opt = update
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|t| !t.is_empty() && *t != name);

        if self.started_at.is_none() {
            self.started_at = Some(Instant::now());
        }

        let first_loc_basename = update
            .get("locations")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|loc| loc.get("path"))
            .and_then(|v| v.as_str())
            .filter(|p| !p.is_empty())
            .map(|p| {
                Path::new(p)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(p)
                    .to_string()
            });

        if !call_id.is_empty() && !self.seen_ids.contains_key(&call_id) {
            self.seen_ids.insert(call_id, name.to_string());
            match (kind, name) {
                ("read", _)
                | (
                    _,
                    "read_file" | "view_file" | "list_dir" | "grep_search" | "find_by_name",
                ) => {
                    self.read_count += 1;
                }
                ("edit", _)
                | (
                    _,
                    "edit_file" | "write_file" | "write_to_file" | "replace_file_content",
                ) => {
                    self.edit_count += 1;
                    if let Some(locs) = update.get("locations").and_then(|v| v.as_array()) {
                        for loc in locs {
                            if let Some(p) = loc.get("path").and_then(|v| v.as_str())
                                && !p.is_empty()
                                && !self.edited_files.iter().any(|x| x == p)
                            {
                                self.edited_files.push(p.to_string());
                            }
                        }
                    }
                }
                ("execute", _) | (_, "run_command") => {
                    self.command_count += 1;
                }
                _ => {
                    self.other_count += 1;
                }
            }
        }

        if status == "in_progress" || status == "pending" {
            let target_hint = first_loc_basename.as_deref().or(title_opt);
            let active_label = format_active_tool_label(name, target_hint);
            self.active_label = Some(active_label.clone());
            Some((true, active_label, self.started_at.unwrap_or_else(Instant::now)))
        } else if status == "completed" {
            let summary = format_cumulative_tool_summary(
                self.command_count,
                self.edit_count,
                self.read_count,
                self.other_count,
            );
            self.last_summary = Some(summary.clone());
            Some((false, summary, self.started_at.unwrap_or_else(Instant::now)))
        } else {
            None
        }
    }
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

    if cli.stdio_proxy {
        return run_acp_stdio_proxy(&cli, &agy_bin, &targets).await;
    }

    if cli.dry_run {
        let prompt = cli
            .prompt
            .as_deref()
            .context("--dry-run 모드에서는 프롬프트 인자가 필요합니다")?;
        let summary = classify_prompt_with_media_timeout(
            prompt,
            &[],
            cli.threshold,
            &targets,
            Duration::from_secs(10),
        )
        .await?;
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    run_interactive_or_oneshot(&cli, &agy_bin, &targets).await
}

/// 외부 ACP 클라이언트(Zed, Neovim 등)와 `agy acp` 사이에서 `session/prompt` 직전에 `session/set_model`을 투명 주입합니다.
async fn run_acp_stdio_proxy(cli: &Cli, agy_bin: &str, targets: &ModelTargets) -> Result<()> {
    let mut cmd = tokio::process::Command::new(agy_bin);
    cmd.arg("acp");
    cmd.args(&cli.acp_args);
    cmd.env("AGY_ROUTE_ACTIVE", "1");
    if !cli.keep_api_key_env {
        cmd.env_remove("GEMINI_API_KEY");
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn().context("업스트림 agy acp 시작 실패")?;
    let mut up_stdin = child.stdin.take().context("업스트림 stdin 실패")?;
    let up_stdout = child.stdout.take().context("업스트림 stdout 실패")?;

    // 업스트림 -> 클라이언트 전달 (프록시가 내부 주입한 __agy_acp_set_model_* 응답만 필터링)
    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(up_stdout).lines();
        let mut client_out = tokio::io::stdout();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains("\"__agy_acp_set_model_") {
                continue;
            }
            let mut out_line = line;
            out_line.push('\n');
            if client_out.write_all(out_line.as_bytes()).await.is_err() {
                break;
            }
            let _ = client_out.flush().await;
        }
    });

    let mut client_lines = BufReader::new(tokio::io::stdin()).lines();
    let mut seq: u64 = 0;
    let mut last_session_models: HashMap<String, String> = HashMap::new();

    while let Some(line) = client_lines.next_line().await? {
        let trimmed = line.trim();
        if !trimmed.is_empty()
            && trimmed.contains("\"session/prompt\"")
            && let Ok(msg) = serde_json::from_str::<serde_json::Value>(trimmed)
            && msg.get("method").and_then(|v| v.as_str()) == Some("session/prompt")
            && let Some(params) = msg.get("params")
            && let Some(sid) = params.get("sessionId").and_then(|v| v.as_str())
        {
            let prompt_text = extract_text_from_acp_prompt_blocks(params.get("prompt"));
            if !prompt_text.is_empty()
                && !prompt_text.trim_start().starts_with('/')
                && let Ok(summary) = classify_prompt_with_media_timeout(
                    &prompt_text,
                    &[],
                    cli.threshold,
                    targets,
                    Duration::from_millis(3500),
                )
                .await
            {
                let selected = summary.selected_model;
                if last_session_models.get(sid).map(String::as_str) != Some(selected.as_str()) {
                    seq += 1;
                    let inject = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": format!("__agy_acp_set_model_{seq}"),
                        "method": "session/set_model",
                        "params": {
                            "sessionId": sid,
                            "modelId": selected,
                        }
                    });
                    let mut inject_line = serde_json::to_string(&inject)?;
                    inject_line.push('\n');
                    up_stdin.write_all(inject_line.as_bytes()).await?;
                    up_stdin.flush().await?;
                    last_session_models.insert(sid.to_string(), selected);
                }
            }
        }

        let mut forward = line;
        forward.push('\n');
        up_stdin.write_all(forward.as_bytes()).await?;
        up_stdin.flush().await?;
    }

    drop(up_stdin);
    let _ = child.wait().await;
    stdout_task.abort();
    Ok(())
}

fn extract_text_from_acp_prompt_blocks(prompt_val: Option<&serde_json::Value>) -> String {
    let Some(arr) = prompt_val.and_then(|v| v.as_array()) else {
        return String::new();
    };
    let mut parts = Vec::new();
    for item in arr {
        if item.get("type").and_then(|v| v.as_str()) == Some("text")
            && let Some(t) = item.get("text").and_then(|v| v.as_str())
        {
            parts.push(t);
        }
    }
    parts.join("\n")
}

async fn run_interactive_or_oneshot(
    cli: &Cli,
    agy_bin: &str,
    initial_targets: &ModelTargets,
) -> Result<()> {
    reset_terminal_mouse_and_modes();

    let (flavor, detected_luma) = resolve_theme_flavor(&cli.theme);
    let accent = flavor.accent_ansi();
    let muted = flavor.muted_ansi();

    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok())
        .unwrap_or_else(|| ".".to_string());

    let oneshot_mode = cli.print_mode || (!cli.repl && !io::stdin().is_terminal() && cli.prompt.is_none());

    if !oneshot_mode {
        eprint!("{muted}[agy-acp] 상주 ACP 서버 연결 중...\x1b[0m");
        let _ = io::stderr().flush();
    }

    let mut daemon = AcpDaemonClient::spawn(agy_bin, cli.keep_api_key_env, &cli.acp_args).await?;

    let (session_id, was_resumed) = if let Some(target_id) = cli.conversation_id.as_deref() {
        let resolved = resolve_session_prefix(&mut daemon, &cwd, target_id).await?;
        (daemon.resume_session(&resolved, &cwd).await?, true)
    } else if cli.continue_recent {
        let sessions = daemon.list_sessions(&cwd).await?;
        if let Some(first) = sessions.first() {
            (daemon.resume_session(&first.session_id, &cwd).await?, true)
        } else {
            (daemon.new_session(&cwd).await?, false)
        }
    } else {
        (daemon.new_session(&cwd).await?, false)
    };

    if !oneshot_mode {
        eprint!("\r\x1b[2K");
    }

    let mut state = SharedReplState::new(
        "agy-acp",
        ACP_HISTORY_FILE,
        initial_targets.clone(),
        flavor,
        detected_luma,
        cli.edit_mode.clone(),
        Some(session_id),
        false,
    );

    if cli.auto {
        state.pinned_model = None;
    } else if let Some(custom_model) = cli.model.as_deref() {
        let (resolved, _) = apply_effort_override(custom_model, None);
        state.pinned_model = Some(resolved.clone());
        state.last_model = resolved;
    }

    if cli.paste_clipboard {
        attach_clipboard_image_interactive(&mut state, None);
    }

    // 비대화형 -p 실행 또는 파이프 입력인 경우 1턴 실행 후 즉시 종료
    if oneshot_mode {
        let raw_prompt =
            read_single_prompt(cli.prompt.clone(), !state.pending_images.is_empty(), "agy-acp")?;
        state.turn_idx += 1;
        execute_acp_turn(&raw_prompt, cli, &cwd, &mut daemon, &mut state, true).await?;
        daemon.shutdown().await;
        reset_terminal_mouse_and_modes();
        return Ok(());
    }

    let mut editor = build_reedline_editor(&state.current_edit_mode, state.active_flavor, ACP_HISTORY_FILE)?;
    print_repl_banner(&state, "상주 ACP 동적 스위칭 REPL · 콜드 스타트 0ms");

    if was_resumed
        && let Some(cid) = state.active_conv_id.as_deref()
    {
        print_conversation_resume_preview("agy-acp", cid, state.active_flavor);
    }

    if let Some(first_prompt) = cli.prompt.as_deref()
        && (!first_prompt.trim().is_empty() || !state.pending_images.is_empty())
    {
        state.turn_idx += 1;
        let effective = if first_prompt.trim().is_empty() {
            DEFAULT_IMAGE_ANALYSIS_PROMPT
        } else {
            first_prompt
        };
        if let Err(err) = execute_acp_turn(effective, cli, &cwd, &mut daemon, &mut state, false).await {
            let warn = state.active_flavor.warn_ansi();
            eprintln!("{warn}[agy-acp] 턴 실행 오류: {err:#}\x1b[0m");
        }
        if !cli.repl && !io::stdin().is_terminal() {
            daemon.shutdown().await;
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

        let (line, left_seg) = match read_repl_line(&mut editor, &mut state) {
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
                if let Err(err) = execute_acp_turn(
                    DEFAULT_IMAGE_ANALYSIS_PROMPT,
                    cli,
                    &cwd,
                    &mut daemon,
                    &mut state,
                    false,
                )
                .await
                {
                    let warn = state.active_flavor.warn_ansi();
                    eprintln!("{warn}[agy-acp] 턴 실행 오류: {err:#}\x1b[0m");
                }
            }
            continue;
        }

        match dispatch_common_slash_command(trimmed, &mut state, &mut editor)? {
            CommonSlashAction::NotSlash => {}
            CommonSlashAction::Handled => continue,
            CommonSlashAction::Exit => break,
            CommonSlashAction::NewSession { clear_screen } => {
                if clear_screen {
                    clear_terminal_screen();
                }
                match daemon.new_session(&cwd).await {
                    Ok(new_sid) => {
                        state.active_conv_id = Some(new_sid.clone());
                        state.turn_idx = 0;
                        state.pending_images.clear();
                        let acc = state.active_flavor.accent_ansi();
                        if clear_screen {
                            print_repl_banner(&state, "상주 ACP 동적 스위칭 REPL · 콜드 스타트 0ms");
                        }
                        eprintln!("{acc}[agy-acp]\x1b[0m 새 상주 세션 생성됨: \x1b[1m{new_sid}\x1b[0m");
                    }
                    Err(err) => {
                        let warn = state.active_flavor.warn_ansi();
                        eprintln!("{warn}[agy-acp] 새 세션 생성 실패: {err:#}\x1b[0m");
                    }
                }
                continue;
            }
            CommonSlashAction::Resume { arg } => {
                let acp_sessions = daemon.list_sessions(&cwd).await.unwrap_or_default();
                let extra_rows: Vec<ConversationRow> = acp_sessions
                    .iter()
                    .map(AcpSessionItem::to_conversation_row)
                    .collect();
                if let Some(selected_cid) =
                    select_conversation_interactive(&arg, &state, &extra_rows)
                {
                    match daemon.resume_session(&selected_cid, &cwd).await {
                        Ok(resumed) => activate_resumed_conversation_state(&resumed, &mut state),
                        Err(err) => {
                            let warn = state.active_flavor.warn_ansi();
                            eprintln!("{warn}[agy-acp] 세션 복원 실패: {err:#}\x1b[0m");
                        }
                    }
                }
                continue;
            }
            CommonSlashAction::RunPrompt(prompt_to_run) => {
                state.turn_idx += 1;
                if let Err(err) =
                    execute_acp_turn(&prompt_to_run, cli, &cwd, &mut daemon, &mut state, false).await
                {
                    let warn = state.active_flavor.warn_ansi();
                    eprintln!("{warn}[agy-acp] 턴 실행 오류: {err:#}\x1b[0m");
                }
                continue;
            }
            CommonSlashAction::Status => {
                print_common_status(&state, daemon.active_model_on_server.as_deref());
                continue;
            }
            CommonSlashAction::SwitchToTui => {
                reset_terminal_mouse_and_modes();
                let sid = state.active_conv_id.clone().unwrap_or_default();
                let active_model = state
                    .pinned_model
                    .clone()
                    .unwrap_or_else(|| state.last_model.clone());
                daemon.shutdown().await;
                let acc = state.active_flavor.accent_ansi();
                eprintln!(
                    "{acc}[agy-acp]\x1b[0m 상주 세션을 순정 agy 풀스크린 TUI로 핸드오프합니다..."
                );
                let mut handoff = Command::new(agy_bin);
                if !cli.keep_api_key_env {
                    handoff.env_remove("GEMINI_API_KEY");
                }
                handoff.env_remove("AGY_ROUTE_ACTIVE");
                handoff.arg("--model").arg(&active_model);
                if let Some(eff) = state
                    .pinned_effort
                    .as_deref()
                    .or(state.last_effort.as_deref())
                {
                    handoff.arg("--effort").arg(eff);
                }
                if !sid.is_empty() {
                    handoff.arg("--conversation").arg(&sid);
                }
                let err = handoff.exec();
                bail!("순정 TUI 전환 실패: {err}");
            }
        }

        erase_prompt_line_in_place(&left_seg, &line);

        state.turn_idx += 1;
        let prompt_expanded = consume_quotes_and_expand_prompt(trimmed, &mut state);
        if let Err(err) =
            execute_acp_turn(&prompt_expanded, cli, &cwd, &mut daemon, &mut state, false).await
        {
            let warn = state.active_flavor.warn_ansi();
            eprintln!("{warn}[agy-acp] 턴 실행 오류: {err:#}\x1b[0m");
        }
    }

    let final_sid = state.active_conv_id.clone().unwrap_or_default();
    daemon.shutdown().await;
    reset_terminal_mouse_and_modes();
    eprintln!(
        "{accent}[agy-acp]\x1b[0m 종료됨. 이어하기: `agy-acp --conversation {final_sid}` (또는 REPL에서 `/resume`)"
    );
    Ok(())
}

async fn execute_acp_turn(
    prompt: &str,
    cli: &Cli,
    cwd: &str,
    daemon: &mut AcpDaemonClient,
    state: &mut SharedReplState,
    quiet_chrome: bool,
) -> Result<()> {
    let accent = state.active_flavor.accent_ansi();
    let warn = state.active_flavor.warn_ansi();
    let mut session_id = state
        .active_conv_id
        .clone()
        .context("활성 상주 세션 ID가 없습니다")?;

    let images = collect_turn_images(&mut state.pending_images, prompt);

    // 1. agy-route와 동일한 순서: 먼저 터미널 폭에 맞춰 사용자 둥근 말풍선을 렌더링합니다.
    if !quiet_chrome {
        let term_width = current_terminal_width();
        let img_refs: Vec<&Path> = images.iter().map(|i| i.path.as_path()).collect();
        render_user_prompt_bubble(prompt, &img_refs, state.active_flavor, term_width);
    }

    // 2. 모델 판별(스피너 표시) 및 상주 세션 실시간 모델 전환 (`session/set_model`, 단절 시 자동 복구)
    let (selected_model, cli_eff) = if let Some(pinned) = state.pinned_model.as_deref() {
        let (m, eff_flag) = apply_effort_override(pinned, state.pinned_effort.as_deref());
        let switch_ms = daemon
            .ensure_model_resilient(&mut session_id, &m, cwd, state.active_flavor, quiet_chrome)
            .await?;
        if !quiet_chrome {
            let sw_note = if switch_ms == 0 {
                "유지 0ms".to_string()
            } else {
                format!("전환 {switch_ms}ms")
            };
            eprintln!("{accent}[agy-acp]\x1b[0m model=\x1b[1m{m}\x1b[0m (pinned, {sw_note})");
        }
        (m, eff_flag)
    } else if prompt.trim_start().starts_with('/') && images.is_empty() {
        let (m, eff_flag) =
            apply_effort_override(&state.last_model, state.pinned_effort.as_deref());
        daemon
            .ensure_model_resilient(&mut session_id, &m, cwd, state.active_flavor, quiet_chrome)
            .await?;
        (m, eff_flag)
    } else {
        let classify_res = if !quiet_chrome && io::stderr().is_terminal() {
            let route_start = Instant::now();
            let mut route_frame = 0usize;
            let mut route_tick = tokio::time::interval(Duration::from_millis(80));
            let fut = classify_prompt_with_media_timeout(
                prompt,
                &images,
                cli.threshold,
                &state.targets,
                Duration::from_secs(10),
            );
            tokio::pin!(fut);
            let out = loop {
                tokio::select! {
                    res = &mut fut => break res,
                    _ = route_tick.tick() => {
                        render_live_progress_line(
                            route_frame,
                            LiveProgressKind::Routing,
                            route_start.elapsed().as_secs_f64(),
                            state.active_flavor,
                        );
                        route_frame = route_frame.wrapping_add(1);
                    }
                }
            };
            clear_live_progress_line();
            out
        } else {
            classify_prompt_with_media_timeout(
                prompt,
                &images,
                cli.threshold,
                &state.targets,
                Duration::from_secs(10),
            )
            .await
        };

        match classify_res {
            Ok(summary) => {
                let (m, eff_flag) = apply_effort_override(
                    &summary.selected_model,
                    state.pinned_effort.as_deref(),
                );
                let switch_ms = daemon
                    .ensure_model_resilient(
                        &mut session_id,
                        &m,
                        cwd,
                        state.active_flavor,
                        quiet_chrome,
                    )
                    .await?;
                if !quiet_chrome {
                    let sw_note = if switch_ms == 0 {
                        "유지 0ms".to_string()
                    } else {
                        format!("전환 {switch_ms}ms")
                    };
                    let img_badge = if images.is_empty() {
                        String::new()
                    } else {
                        format!(", 📷x{}", images.len())
                    };
                    eprintln!(
                        "{accent}[agy-acp]\x1b[0m model=\x1b[1m{m}\x1b[0m ({sw_note} · tier={}, score={:.2}, conf={:.0}%{img_badge}, decision={}ms)",
                        summary.task_tier,
                        summary.complexity_score,
                        summary.tier_confidence * 100.0,
                        summary.latency_ms
                    );
                }
                (m, eff_flag)
            }
            Err(err) => {
                if cli.strict {
                    return Err(err);
                }
                let (fallback, eff_flag) = apply_effort_override(
                    &state.targets.variant("argon", "medium"),
                    state.pinned_effort.as_deref(),
                );
                daemon
                    .ensure_model_resilient(
                        &mut session_id,
                        &fallback,
                        cwd,
                        state.active_flavor,
                        quiet_chrome,
                    )
                    .await?;
                if !quiet_chrome {
                    eprintln!(
                        "{warn}[agy-acp]\x1b[0m 판별 생략 ({err:#}) -> 기본 모델({fallback})로 실행합니다.\x1b[0m"
                    );
                }
                (fallback, eff_flag)
            }
        }
    };

    state.active_conv_id = Some(session_id.clone());
    state.last_model = selected_model.clone();
    state.last_effort = cli_eff;

    // 3. ACP 네이티브 ContentBlock 구성 (이미지는 base64 인라인 블록으로 직접 전달)
    let mut blocks = Vec::with_capacity(images.len() + 1);
    for img in &images {
        blocks.push(serde_json::json!({
            "type": "image",
            "data": img.base64_data,
            "mimeType": img.mime_type,
        }));
    }
    blocks.push(serde_json::json!({
        "type": "text",
        "text": prompt,
    }));

    let mut req_id = daemon.next_id.fetch_add(1, Ordering::Relaxed);
    let t_turn = Instant::now();
    let make_prompt_rpc = |id: u64, sid: &str| {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": {
                "sessionId": sid,
                "prompt": blocks,
            }
        })
    };

    if daemon
        .send_json(&make_prompt_rpc(req_id, &session_id))
        .await
        .is_err()
    {
        session_id = daemon
            .recover_session(&session_id, cwd, state.active_flavor, quiet_chrome)
            .await?;
        state.active_conv_id = Some(session_id.clone());
        let _ = daemon.ensure_model(&session_id, &selected_model).await;
        req_id = daemon.next_id.fetch_add(1, Ordering::Relaxed);
        daemon
            .send_json(&make_prompt_rpc(req_id, &session_id))
            .await?;
    }

    let use_raw_stream = quiet_chrome || cli.raw_output;
    let mut md_printer = StreamingMarkdownPrinter::new(state.active_flavor);
    let mut raw_accum = String::new();
    let mut tool_tracker = AcpToolTracker::default();
    let mut pending_tool_summary_to_print: Option<(String, f64)> = None;
    let mut usage_pair = (0_u64, 0_u64);
    let mut canceled = false;
    let mut progress_phase = TurnProgressPhase::Thinking {
        started_at: Instant::now(),
        completed_tools: None,
    };
    let mut spinner_idx: usize = 0;
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
                eprintln!("\n{warn}[agy-acp] Ctrl+C 감지 — 현재 턴 실행을 중단(session/cancel)합니다...\x1b[0m");
                let _ = daemon.cancel_turn(&session_id).await;
            }
            _ = tick_timer.tick(), if !quiet_chrome && !matches!(progress_phase, TurnProgressPhase::Streaming) => {
                match &progress_phase {
                    TurnProgressPhase::Thinking { started_at, completed_tools } => {
                        render_live_progress_line(
                            spinner_idx,
                            LiveProgressKind::Thinking { completed_tools: completed_tools.as_deref() },
                            started_at.elapsed().as_secs_f64(),
                            state.active_flavor,
                        );
                        has_active_status_line = true;
                    }
                    TurnProgressPhase::ToolRunning { label, started_at } => {
                        render_live_progress_line(
                            spinner_idx,
                            LiveProgressKind::ToolRunning { label },
                            started_at.elapsed().as_secs_f64(),
                            state.active_flavor,
                        );
                        has_active_status_line = true;
                    }
                    TurnProgressPhase::Streaming => {}
                }
                spinner_idx = spinner_idx.wrapping_add(1);
            }
            line_res = daemon.lines.next_line() => {
                let Some(raw_line) = line_res? else {
                    if canceled {
                        break;
                    }
                    bail!("상주 agy acp 프로세스 연결이 끊어졌습니다 (다음 턴에서 자동 복구됩니다).");
                };
                let trimmed = raw_line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                    continue;
                };

                // 서버 -> 클라이언트 권한 확인 RPC 요청 처리 (예: PreToolUse force_ask)
                if let Some(method) = msg.get("method").and_then(|v| v.as_str())
                    && let Some(srv_id) = msg.get("id").cloned()
                {
                    if has_active_status_line {
                        clear_live_progress_line();
                        has_active_status_line = false;
                    }
                    md_printer.finish();
                    let reply = handle_server_rpc_request(method, msg.get("params"), state.active_flavor, quiet_chrome);
                    daemon.send_json(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": srv_id,
                        "result": reply,
                    })).await?;
                    progress_phase = TurnProgressPhase::Thinking {
                        started_at: Instant::now(),
                        completed_tools: tool_tracker.last_summary.clone(),
                    };
                    continue;
                }

                // 스트리밍 업데이트 처리
                if msg.get("method").and_then(|v| v.as_str()) == Some("session/update")
                    && let Some(upd) = msg.get("params").and_then(|p| p.get("update"))
                {
                    match upd.get("sessionUpdate").and_then(|v| v.as_str()).unwrap_or("") {
                        "agent_thought_chunk" => {
                            // 이미 Thinking 상태이면 started_at을 초기화하지 않아 경과 시간 카운터가 멈추지 않게 유지
                            if !matches!(progress_phase, TurnProgressPhase::Thinking { .. }) {
                                if matches!(progress_phase, TurnProgressPhase::Streaming) && !use_raw_stream {
                                    md_printer.finish();
                                }
                                progress_phase = TurnProgressPhase::Thinking {
                                    started_at: Instant::now(),
                                    completed_tools: tool_tracker.last_summary.clone(),
                                };
                            }
                        }
                        "agent_message_chunk" => {
                            if let Some(txt) = upd.get("content").and_then(|c| c.get("text")).and_then(|v| v.as_str())
                                && !txt.is_empty()
                            {
                                if has_active_status_line {
                                    clear_live_progress_line();
                                    has_active_status_line = false;
                                }
                                if !quiet_chrome
                                    && !txt.trim().is_empty()
                                    && let Some((sum, el)) = pending_tool_summary_to_print.take()
                                {
                                    print_completed_tool_summary_line(&sum, el, state.active_flavor);
                                }
                                progress_phase = TurnProgressPhase::Streaming;

                                if use_raw_stream {
                                    raw_accum.push_str(txt);
                                    print!("{txt}");
                                    let _ = io::stdout().flush();
                                } else {
                                    md_printer.push_delta(txt);
                                }
                            }
                        }
                        "tool_call" | "tool_call_update" if !quiet_chrome => {
                            if matches!(progress_phase, TurnProgressPhase::Streaming) {
                                if !use_raw_stream {
                                    md_printer.finish();
                                }
                                println!();
                            }
                            if let Some((is_running, label_or_summary, started)) = tool_tracker.on_tool_event(upd) {
                                if is_running {
                                    progress_phase = TurnProgressPhase::ToolRunning {
                                        label: label_or_summary,
                                        started_at: started,
                                    };
                                } else {
                                    let tool_elapsed = started.elapsed().as_secs_f64();
                                    pending_tool_summary_to_print = Some((label_or_summary.clone(), tool_elapsed));
                                    // 도구 완료 직후에도 커서가 멈춰 있지 않도록 회전 스피너 + 완료 도구 요약 상태로 즉시 전환
                                    progress_phase = TurnProgressPhase::Thinking {
                                        started_at: Instant::now(),
                                        completed_tools: Some(label_or_summary),
                                    };
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // 턴 최종 응답 수신
                if msg.get("id").and_then(|v| v.as_u64()) == Some(req_id)
                    && (msg.get("result").is_some() || msg.get("error").is_some())
                {
                    if has_active_status_line {
                        clear_live_progress_line();
                        has_active_status_line = false;
                    }
                    if let Some(err) = msg.get("error") {
                        if !canceled {
                            eprintln!("\n{warn}[agy-acp] 턴 오류: {err}\x1b[0m");
                        }
                    } else if let Some(res) = msg.get("result")
                        && let Some(u) = res.get("usage")
                    {
                        let inp = u.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        let out = u.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        usage_pair = (inp, out);
                    }
                    break;
                }
            }
        }
    }

    if has_active_status_line {
        clear_live_progress_line();
    }
    if !quiet_chrome
        && let Some((sum, el)) = pending_tool_summary_to_print.take()
    {
        print_completed_tool_summary_line(&sum, el, state.active_flavor);
    }
    reset_terminal_mouse_and_modes();

    if use_raw_stream {
        println!();
        state.record_turn_output(Vec::new(), raw_accum);
        if !quiet_chrome {
            render_turn_summary_card(
                t_turn.elapsed().as_secs_f64(),
                usage_pair.0,
                usage_pair.1,
                &tool_tracker.edited_files,
                state.active_flavor,
            );
        }
    } else {
        let (blocks, raw_resp) = md_printer.finish();
        state.record_turn_output(blocks, raw_resp);
        render_turn_summary_card(
            t_turn.elapsed().as_secs_f64(),
            usage_pair.0,
            usage_pair.1,
            &tool_tracker.edited_files,
            state.active_flavor,
        );
    }

    Ok(())
}

fn handle_server_rpc_request(
    method: &str,
    params: Option<&serde_json::Value>,
    flavor: ThemeFlavor,
    non_interactive: bool,
) -> serde_json::Value {
    let warn = flavor.warn_ansi();
    let accent = flavor.accent_ansi();

    if method == "session/request_permission"
        && let Some(p) = params
    {
        let options = p
            .get("options")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let title = p
            .get("toolCall")
            .and_then(|t| t.get("title"))
            .and_then(|v| v.as_str())
            .unwrap_or("권한 확인 요청");

        if non_interactive || !io::stdin().is_terminal() {
            return serde_json::json!({ "outcome": { "outcome": "cancelled" } });
        }

        eprintln!("\n{warn}[Decision Gate / ACP 권한 확인]\x1b[0m \x1b[1m{title}\x1b[0m");
        for (i, opt) in options.iter().enumerate() {
            let name = opt.get("name").and_then(|v| v.as_str()).unwrap_or("옵션");
            eprintln!("  {accent}{}.\x1b[0m {name}", i + 1);
        }
        eprint!(
            "  선택 번호 입력 (1={}, Enter=거부/취소): ",
            options
                .first()
                .and_then(|o| o.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("허용")
        );
        let _ = io::stderr().flush();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_ok()
            && let Ok(idx) = input.trim().parse::<usize>()
            && idx >= 1
            && idx <= options.len()
            && let Some(opt_id) = options[idx - 1].get("optionId").and_then(|v| v.as_str())
        {
            return serde_json::json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": opt_id,
                }
            });
        }
        return serde_json::json!({ "outcome": { "outcome": "cancelled" } });
    }

    serde_json::json!({})
}

async fn resolve_session_prefix(
    daemon: &mut AcpDaemonClient,
    cwd: &str,
    prefix: &str,
) -> Result<String> {
    if prefix.len() >= 36 {
        return Ok(prefix.to_string());
    }
    let sessions = daemon.list_sessions(cwd).await?;
    if let Some(found) = sessions
        .into_iter()
        .find(|s| s.session_id.starts_with(prefix))
    {
        return Ok(found.session_id);
    }
    Ok(prefix.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_text_from_acp_prompt_blocks() {
        let blocks = serde_json::json!([
            { "type": "image", "data": "AAAA", "mimeType": "image/png" },
            { "type": "text", "text": "이 화면의 오류 원인을 분석해줘" }
        ]);
        assert_eq!(
            extract_text_from_acp_prompt_blocks(Some(&blocks)),
            "이 화면의 오류 원인을 분석해줘"
        );
    }

    #[tokio::test]
    async fn daemon_isolates_process_group_and_recovers_after_crash() {
        use std::os::unix::fs::PermissionsExt;

        let tmp_script = std::env::temp_dir().join(format!(
            "mock_agy_acp_{}.py",
            std::process::id()
        ));
        let py_code = r#"#!/usr/bin/env python3
import json, sys

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    req_id = msg.get("id")
    method = msg.get("method", "")
    if req_id is None:
        continue
    if method == "initialize":
        res = {"protocolVersion": 1}
    elif method == "session/new":
        res = {"sessionId": "sess-init-123", "models": {"currentModelId": "gemini-3.8-flash-low"}}
    elif method == "session/resume":
        res = {"sessionId": msg["params"]["sessionId"]}
    else:
        res = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": res}) + "\n")
    sys.stdout.flush()
"#;
        std::fs::write(&tmp_script, py_code).unwrap();
        let mut perms = std::fs::metadata(&tmp_script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp_script, perms).unwrap();

        let script_str = tmp_script.to_str().unwrap();
        let mut daemon = AcpDaemonClient::spawn(script_str, false, &[]).await.unwrap();

        // 1. 자식 프로세스가 자체 프로세스 그룹(PGID == child PID != 부모 PID)으로 격리되었는지 검증
        unsafe extern "C" {
            fn getpgid(pid: i32) -> i32;
            fn getpgrp() -> i32;
        }
        let child_pid = daemon.child.id().expect("child pid") as i32;
        let child_pgid = unsafe { getpgid(child_pid) };
        let parent_pgid = unsafe { getpgrp() };
        assert_eq!(child_pgid, child_pid, "자식 ACP 데몬은 독립 프로세스 그룹 리더여야 함");
        assert_ne!(child_pgid, parent_pgid, "터미널 SIGINT 전파 방지를 위해 부모와 다른 PGID여야 함");

        let mut sid = daemon.new_session("/tmp").await.unwrap();
        assert_eq!(sid, "sess-init-123");

        // 2. 데몬이 외부 시그널/오류로 죽은 상황(Broken pipe 원인)을 강제 재현
        daemon.child.kill().await.unwrap();
        let _ = daemon.child.wait().await;
        assert!(daemon.is_dead());

        // 3. ensure_model_resilient 호출 시 Broken pipe로 죽지 않고 자동 재스폰 + 세션 복구 후 모델 전환 성공 검증
        let switch_ms = daemon
            .ensure_model_resilient(&mut sid, "gemini-4-carbon-medium", "/tmp", ThemeFlavor::Dark, true)
            .await
            .expect("죽은 데몬에서 자동 재연결 및 모델 전환이 성공해야 함");
        let _ = switch_ms;
        assert!(!daemon.is_dead());
        assert_eq!(sid, "sess-init-123");
        assert_eq!(
            daemon.active_model_on_server.as_deref(),
            Some("gemini-4-carbon-medium")
        );

        daemon.shutdown().await;
        let _ = std::fs::remove_file(&tmp_script);
    }
}
